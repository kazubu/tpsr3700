//! SVG print preview.
//!
//! Renders the same `rows` bitmap that will be sent to the printer into an
//! SVG file, opens it with the OS default viewer when possible, and asks the
//! user to confirm in the terminal before printing. This avoids depending on
//! GPU-backed GUI textures while still showing the exact raster data.

use crate::tepra::Tape;
#[cfg(test)]
use crate::tepra::TapeWidth;
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Outcome of showing the preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewResult {
    /// User confirmed printing.
    Print,
    /// User declined printing.
    Cancel,
}

/// Shows a preview rendering `rows` (the same bitmap rows that
/// [`crate::tepra::builder::build_print_job`] would encode) for `tape`, and
/// blocks until the user confirms or cancels.
///
/// `rows` is a sequence of feed-direction raster rows, each a bit-packed row
/// `tape.bitmap_bits()` dots wide (MSB-first), exactly as produced by
/// [`crate::font::FontRenderer::render_text`] /
/// [`crate::font::FontRenderer::render_text_multiline`].
///
/// `top_margin_bits` / `bottom_margin_bits` are the non-printing margins at
/// the top/bottom edges of the tape (same meaning as the `--top-margin` /
/// `--bottom-margin` CLI options). `left_margin_rows` / `right_margin_rows`
/// are the blank feed-direction rows already included at the start/end of
/// `rows` (same meaning as the `margin_rows` returned by
/// [`crate::font::FontRenderer::render_text`] and the `--right-margin-mm` CLI
/// option converted to rows via [`crate::font::mm_to_rows`]). The area outside
/// the printable band on any edge is shaded gray in the preview to distinguish
/// it from the printable area.
pub fn show_preview(
    rows: &[Vec<u8>],
    tape: Tape,
    label_text: &str,
    top_margin_bits: usize,
    bottom_margin_bits: usize,
    left_margin_rows: u32,
    right_margin_rows: u32,
) -> Result<PreviewResult> {
    let svg = build_svg_preview(
        rows,
        tape,
        label_text,
        top_margin_bits,
        bottom_margin_bits,
        left_margin_rows,
        right_margin_rows,
    );
    let path = write_temp_svg(&svg)?;

    eprintln!("preview written to {}", path.display());
    match open_file(&path) {
        Ok(()) => eprintln!("opened preview in the default viewer"),
        Err(err) => eprintln!("could not open preview automatically: {err}"),
    }

    if confirm_print()? {
        Ok(PreviewResult::Print)
    } else {
        Ok(PreviewResult::Cancel)
    }
}

/// Builds the SVG preview string for `rows` without writing a file,
/// opening a viewer, or prompting. Used by the `--preview-out` CLI mode
/// (and by the Web frontend) so the exact same rendering that
/// [`show_preview`] displays can be saved or served over HTTP.
pub fn build_svg_preview(
    rows: &[Vec<u8>],
    tape: Tape,
    label_text: &str,
    top_margin_bits: usize,
    bottom_margin_bits: usize,
    left_margin_rows: u32,
    right_margin_rows: u32,
) -> String {
    let total_bits = tape.bitmap_bits();
    let feed_len = rows.len().max(1);
    let head_offset = bottom_margin_bits;
    let head_bits = total_bits.saturating_sub(top_margin_bits + bottom_margin_bits);
    let head_end = head_offset + head_bits;
    let left_margin_rows = (left_margin_rows as usize).min(feed_len);
    let right_margin_rows = (right_margin_rows as usize).min(feed_len);
    let content_end = feed_len.saturating_sub(right_margin_rows);
    let title = escape_xml(label_text);
    let display_width = feed_len.max(1);

    // Draw the whole tape, not just the dot band: the physical tape is
    // wider than the head dots it uses (e.g. 9mm tape prints 7.9mm), and
    // every part of the label that gets no ink is hatched so it reads as
    // "label, but not printed" rather than as a gap.
    let tape_rows = (tape.mm_to_rows(tape.width_mm() as f32) as usize).max(total_bits);
    let band_offset = (tape_rows - total_bits) / 2;
    let display_height = tape_rows.max(1);
    // Non-printing bands in tape coordinates (y grows downwards; bit 0 of
    // a row is the bottom edge of the dot band).
    let hatch_top = band_offset + (total_bits - head_end);
    let hatch_bottom_y = band_offset + (total_bits - head_offset);

    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{display_width}" height="{display_height}" viewBox="0 0 {display_width} {display_height}" shape-rendering="crispEdges">"#
    );
    let _ = writeln!(svg, "<title>{title}</title>");
    let _ = writeln!(
        svg,
        r##"<defs><pattern id="hatch" patternUnits="userSpaceOnUse" width="8" height="8" patternTransform="rotate(45)"><line x1="0" y1="0" x2="0" y2="8" stroke="#b8b8b8" stroke-width="1.5"/></pattern></defs>"##
    );
    let _ = writeln!(svg, r#"<rect width="100%" height="100%" fill="white"/>"#);

    // Tape edges above/below the printable band (includes the tape area
    // the head cannot reach and the configured top/bottom margins).
    if hatch_top > 0 {
        let _ = writeln!(svg, r##"<rect class="margin" x="0" y="0" width="{feed_len}" height="{hatch_top}" fill="url(#hatch)"/>"##);
    }
    if hatch_bottom_y < tape_rows {
        let h = tape_rows - hatch_bottom_y;
        let _ = writeln!(svg, r##"<rect class="margin" x="0" y="{hatch_bottom_y}" width="{feed_len}" height="{h}" fill="url(#hatch)"/>"##);
    }
    // Blank feed margins before and after the content.
    if left_margin_rows > 0 {
        let _ = writeln!(svg, r##"<rect class="margin" x="0" y="0" width="{left_margin_rows}" height="{tape_rows}" fill="url(#hatch)"/>"##);
    }
    if right_margin_rows > 0 {
        let x = content_end;
        let _ = writeln!(svg, r##"<rect class="margin" x="{x}" y="0" width="{right_margin_rows}" height="{tape_rows}" fill="url(#hatch)"/>"##);
    }

    let _ = writeln!(
        svg,
        r#"<path fill="black" transform="translate(0 {band_offset})" d="{}"/>"#,
        ink_path(rows, total_bits)
    );
    let _ = writeln!(svg, r##"<rect x="0" y="0" width="{feed_len}" height="{tape_rows}" fill="none" stroke="#808080" stroke-width="1" vector-effect="non-scaling-stroke"/>"##);
    let _ = writeln!(svg, "</svg>");
    svg
}

fn ink_path(rows: &[Vec<u8>], total_bits: usize) -> String {
    let mut path = String::new();
    for (x, row) in rows.iter().enumerate() {
        let mut bit = 0;
        while bit < total_bits {
            if !bit_is_set(row, bit) {
                bit += 1;
                continue;
            }

            let start = bit;
            bit += 1;
            while bit < total_bits && bit_is_set(row, bit) {
                bit += 1;
            }

            let y = total_bits - bit;
            let h = bit - start;
            let _ = write!(path, "M{x} {y}h1v{h}h-1z");
        }
    }
    path
}

fn bit_is_set(row: &[u8], bit: usize) -> bool {
    let byte_i = bit / 8;
    let bit_j = bit % 8;
    byte_i < row.len() && (row[byte_i] >> (7 - bit_j)) & 1 == 1
}

fn write_temp_svg(svg: &str) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    path.push(format!("tpsr3700-preview-{}.svg", std::process::id()));
    std::fs::write(&path, svg).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn open_file(path: &Path) -> Result<()> {
    let mut command = open_command(path);
    command
        .spawn()
        .with_context(|| format!("failed to launch viewer for {}", path.display()))?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_command(path: &Path) -> Command {
    let mut command = Command::new("open");
    command.arg(path);
    command
}

#[cfg(target_os = "windows")]
fn open_command(path: &Path) -> Command {
    let mut command = Command::new("cmd");
    command.arg("/C").arg("start").arg("").arg(path);
    command
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_command(path: &Path) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    command
}

fn confirm_print() -> Result<bool> {
    eprint!("Print this label? [y/N] ");
    std::io::stderr().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim(), "y" | "Y" | "yes" | "YES" | "Yes"))
}

fn escape_xml(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_xml_escapes_special_chars() {
        assert_eq!(escape_xml("A&B<C>\"'"), "A&amp;B&lt;C&gt;&quot;&apos;");
    }

    #[test]
    fn ink_path_groups_vertical_runs() {
        let rows = vec![vec![0b0111_0000]];
        assert_eq!(ink_path(&rows, 8), "M0 4h1v3h-1z");
    }

    #[test]
    fn svg_contains_margin_and_ink() {
        let rows = vec![vec![0b0100_0000], vec![0]];
        let svg = build_svg_preview(&rows, Tape::sr3700p(TapeWidth::Mm6), "A&B", 1, 1, 1, 0);
        assert!(svg.contains("A&amp;B"));
        // Margins are hatched, not grey.
        assert!(svg.contains(r##"fill="url(#hatch)""##));
        assert!(!svg.contains("#e0e0e0"));
        // 6mm tape is 64 rows tall at 270 dpi and the 56-dot band sits in
        // the middle (offset 4); bit 1 (MSB-first) is drawn at y = 56 - 1 - 1
        // within the band.
        assert!(svg.contains(r#"height="64""#));
        assert!(svg.contains(r#"<path fill="black" transform="translate(0 4)" d="M0 54h1v1h-1z"/>"#));
    }
}

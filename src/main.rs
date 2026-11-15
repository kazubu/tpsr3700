use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tpsr3700::font::{FontRenderer, TextAlign, DEFAULT_FONT_BYTES, DEFAULT_FEED_MARGIN_MM};
use tpsr3700::tepra::builder::build_print_job;
use tpsr3700::tepra::TapeWidth;

/// Parse `job` and print a human-readable summary plus an ASCII-art
/// rendering of the raster rows to stdout.
fn dump_explain(job: &[u8], tape: TapeWidth, top_margin: usize, bottom_margin: usize) {
    let bitmap_bytes = tape.bitmap_bytes();
    let bitmap_bits = tape.bitmap_bits();
    let head_offset = bottom_margin;
    let head_bits = tape.bitmap_bits().saturating_sub(top_margin + bottom_margin).max(1);

    // ---- Header block (fixed 2179 bytes) ----
    let tape_mm_byte = job[8];
    println!("=== TEPRA job dump ({} bytes) ===", job.len());
    println!();
    println!("[Header block]  bytes 0-2178 (2179 bytes, fixed)");
    println!("  tape width byte (offset 8) : 0x{:02X} = {}mm", tape_mm_byte, tape_mm_byte);

    // ---- Config commands ----
    // Frame layout: ESC { <len> <cmd> <params…> <checksum> }
    // Lengths: C=10, D=7, G=6, L=10, T=9  (ESC+{+len+cmd+params+cs+})
    const HEADER_LEN: usize = 2179;
    const CMD_FRAME_OVERHEAD: usize = 4; // ESC { len cmd ... cs }  (excl params)
    let cmd_c_len = CMD_FRAME_OVERHEAD + 4 + 2; // 4 params + cs + }
    let cmd_d_len = CMD_FRAME_OVERHEAD + 1 + 2;
    let cmd_g_len = CMD_FRAME_OVERHEAD + 0 + 2;
    let cmd_l_len = CMD_FRAME_OVERHEAD + 4 + 2;
    let cmd_t_len = CMD_FRAME_OVERHEAD + 2 + 2;

    let l_params = HEADER_LEN + cmd_c_len + cmd_d_len + cmd_g_len + 4; // skip ESC { len 'L'
    let row_count = u32::from_le_bytes(job[l_params..l_params + 4].try_into().unwrap());

    let t_params = HEADER_LEN + cmd_c_len + cmd_d_len + cmd_g_len + cmd_l_len + 4;
    let margin_rows = u16::from_le_bytes(job[t_params..t_params + 2].try_into().unwrap());

    println!();
    println!("[Config commands]");
    println!("  L (row count)  : {}", row_count);
    println!("  T (margin rows): {}", margin_rows);

    // ---- Raster rows ----
    let row_len = 7 + bitmap_bytes; // ESC . 00 0D 0D 01 <width_byte> <bitmap>
    let raster_start = HEADER_LEN + cmd_c_len + cmd_d_len + cmd_g_len + cmd_l_len + cmd_t_len;

    println!();
    println!(
        "[Raster rows]  {} rows × {} bitmap bytes ({} bits, printable: bits {}..{})",
        row_count,
        bitmap_bytes,
        bitmap_bits,
        head_offset,
        head_offset + head_bits - 1,
    );

    // Collect bitmaps.
    let mut bitmaps: Vec<&[u8]> = Vec::new();
    for i in 0..row_count as usize {
        let start = raster_start + i * row_len;
        bitmaps.push(&job[start + 7..start + 7 + bitmap_bytes]);
    }

    // Per-bit ink counts (for summary).
    let mut bit_ink = vec![0u32; bitmap_bits];
    for bm in &bitmaps {
        for (byte_i, &byte) in bm.iter().enumerate() {
            for bit_j in 0..8 {
                let bit_pos = byte_i * 8 + bit_j;
                if bit_pos >= bitmap_bits { break; }
                if (byte >> (7 - bit_j)) & 1 == 1 {
                    bit_ink[bit_pos] += 1;
                }
            }
        }
    }
    let first_ink = bit_ink.iter().position(|&v| v > 0);
    let last_ink  = bit_ink.iter().rposition(|&v| v > 0);
    match (first_ink, last_ink) {
        (Some(f), Some(l)) => println!("  ink bit range  : {}..{} (span {})", f, l, l - f + 1),
        _ => println!("  ink bit range  : (none)"),
    }

    // ---- Footer ----
    println!();
    println!("[Footer]  0x0C + ESC{{'{{'@'}}");
}

/// Print text on a KING JIM TEPRA PRO SR3700P label printer from Linux or macOS.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Text to print.
    text: Option<String>,

    /// USB vendor ID override (hex, e.g. 0d8a). Defaults to the
    /// TEPRA_USB_VID env var, or the SR3700P's 0d8a. Useful for other
    /// TEPRA PRO models such as the SR3900P: find the IDs with `lsusb`
    /// while the printer is attached.
    #[arg(long)]
    vid: Option<String>,

    /// USB product ID override (hex, e.g. 0103). Defaults to the
    /// TEPRA_USB_PID env var, or the SR3700P's 0103.
    #[arg(long)]
    pid: Option<String>,

    /// Write an SVG preview of the job to this path and exit without
    /// printing and without prompting. The SVG is rendered from the
    /// exact raster rows that would be sent to the printer (same code
    /// path as the interactive preview). Intended for Web frontends.
    #[arg(long)]
    preview_out: Option<PathBuf>,

    /// Query the printer for the currently loaded tape width, print the
    /// result as one line of JSON on stdout, and exit. No TEXT argument
    /// is needed in this mode and nothing is printed.
    ///
    /// Output: {"tape_mm": 18, "index": 4, "supported": true}
    /// `index` is the raw tape width index reported by the printer
    /// (see docs/protocol.md section 1.5). `supported` is false when the
    /// loaded width isn't supported by this driver (e.g. 9mm/36mm); in
    /// that case `tape_mm` is the nominal width for the index, or null
    /// if the index itself is unknown. If no printer is found, an error
    /// naming the vendor/product IDs that were searched for is printed
    /// to stderr and the exit status is non-zero.
    #[arg(long)]
    detect_tape: bool,


    /// Tape width in millimeters (6, 12, 18, and experimental 24 are supported).
    /// If omitted, the width is auto-detected by querying the printer
    /// (requires the printer to be connected; incompatible with
    /// --no-print).
    #[arg(long)]
    tape_mm: Option<u8>,

    /// Font size in points (as shown in the TEPRA desktop app, e.g. 36).
    /// If omitted, the largest size that fits the tape width (or one
    /// line-band when multi-line wrapping is active) is used automatically.
    #[arg(long)]
    size: Option<f32>,

    /// Per-line font size ratios for newline-separated multi-line text,
    /// comma-separated (e.g. "7,3" makes the first line 7/3 the size of
    /// the second). The number of ratios must equal the number of lines;
    /// a mismatch is an error. Each line's band of the tape width is
    /// also divided proportionally to the ratios.
    ///
    /// Size semantics: with --size S, S is the size of the largest-ratio
    /// line and line i is S * ratio_i / max(ratios). Without --size,
    /// line i is max_size * ratio_i / sum(ratios), so each line fills
    /// its proportional band (equal ratios = the traditional
    /// max/lines in equal bands).
    ///
    /// Cannot be combined with --max-tape-length-mm (wrap mode): ratios
    /// apply only to explicit '\n'-separated lines.
    #[arg(long)]
    line_ratios: Option<String>,

    /// Blank tape feed margin (in millimeters) added before the printed
    /// content (left side when the label exits the printer).
    #[arg(long, default_value_t = 10.0)]
    left_margin_mm: f32,

    /// Blank tape feed margin (in millimeters) added after the printed
    /// content (right side).
    #[arg(long, default_value_t = DEFAULT_FEED_MARGIN_MM)]
    right_margin_mm: f32,

    /// Non-printing margin at the top edge of the tape, in dots.
    #[arg(long, default_value_t = 2)]
    top_margin: usize,

    /// Non-printing margin at the bottom edge of the tape, in dots.
    #[arg(long, default_value_t = 8)]
    bottom_margin: usize,

    /// Total label length in millimeters, including left and right feed
    /// margins.  When the text does not fit on one line at the chosen font
    /// size, it is wrapped character-by-character onto additional lines
    /// (up to the maximum allowed by the tape width; minimum line height
    /// is 3 mm, so a 6 mm tape allows at most 2 lines).  Omitting this
    /// option disables wrapping and prints everything on a single line.
    #[arg(long)]
    max_tape_length_mm: Option<f32>,

    /// Horizontal alignment of each line within the label length.
    /// Only meaningful when --max-tape-length-mm is set.
    /// Allowed values: left (default), center, right.
    #[arg(long, default_value = "left")]
    align: TextAlign,

    /// Path to a TrueType/OpenType font file (.ttf/.otf/.ttc).
    /// When omitted, the bundled BIZ UDPゴシック Regular is used.
    #[arg(long)]
    font: Option<String>,

    /// Draw a rectangular frame around the printed content. The frame
    /// is applied to the rendered raster rows (see
    /// `font::apply_frame`), so it composes with multi-line text,
    /// --line-ratios, --align and wrap mode, and appears identically in
    /// previews. The frame hugs the ink bounding box with ~1mm of
    /// padding, clamped to the printable band.
    #[arg(long)]
    frame: bool,

    /// Frame line thickness in dots (1-3). Only meaningful together
    /// with --frame.
    #[arg(long, default_value_t = 2)]
    frame_width: usize,

    /// Write the generated raw TEPRA job bytes to this file instead of (or
    /// in addition to) sending it to the printer. Useful for debugging
    /// without hardware.
    #[arg(long)]
    dump: Option<PathBuf>,

    /// Skip sending the job to the printer over USB (only useful together
    /// with --dump).
    #[arg(long)]
    no_print: bool,

    /// Skip the print preview and send the job straight to the
    /// printer (or to --dump) without asking for confirmation. By
    /// default an SVG preview is shown before printing; see the
    /// `preview` feature (enabled by default).
    #[arg(long)]
    skip_preview: bool,
}

/// Opens the printer, resolving `--vid`/`--pid` overrides (which fall
/// back to the `TEPRA_USB_VID`/`TEPRA_USB_PID` env vars, then to the
/// SR3700P defaults) the same way for every mode.
fn open_printer(args: &Args) -> Result<tpsr3700::usb::Printer> {
    match (&args.vid, &args.pid) {
        (None, None) => tpsr3700::usb::Printer::open(),
        _ => {
            let vid = match &args.vid {
                Some(s) => tpsr3700::usb::parse_usb_id(s)?,
                None => tpsr3700::usb::vendor_id(),
            };
            let pid = match &args.pid {
                Some(s) => tpsr3700::usb::parse_usb_id(s)?,
                None => tpsr3700::usb::product_id(),
            };
            tpsr3700::usb::Printer::open_with(vid, pid)
        }
    }
}

/// Nominal tape width in mm for a detected tape width index, per the
/// index mapping in docs/protocol.md section 1.5
/// (`[4, 6, 9, 12, 18, 24, 36, 50]` mm for indices 0..=7). Returns
/// `None` for indices outside that table. This exists so widths the
/// driver cannot print to yet (9mm, 36mm, ...) can still be reported
/// by name in `--detect-tape` output.
fn nominal_mm_for_detected_index(index: u8) -> Option<u8> {
    match index {
        0 => Some(4),
        1 => Some(6),
        2 => Some(9),
        3 => Some(12),
        4 => Some(18),
        5 => Some(24),
        6 => Some(36),
        7 => Some(50),
        _ => None,
    }
}

/// Formats the `--detect-tape` result as one line of JSON:
/// `{"tape_mm": 18, "index": 4, "supported": true}`. For unsupported
/// widths `tape_mm` falls back to the nominal width for the index
/// (or `null` when the index is unknown).
fn detect_tape_json(detected: Option<TapeWidth>, index: u8) -> String {
    let supported = detected.is_some();
    let tape_mm = detected
        .map(|t| t.width_mm())
        .or_else(|| nominal_mm_for_detected_index(index));
    let tape_mm_json = tape_mm
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    format!("{{\"tape_mm\": {tape_mm_json}, \"index\": {index}, \"supported\": {supported}}}")
}

/// Parse a `--line-ratios` value such as "7,3" into positive floats.
fn parse_line_ratios(s: &str) -> Result<Vec<f32>> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        let v: f32 = part
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid line ratio '{part}': not a number"))?;
        if !v.is_finite() || v <= 0.0 {
            anyhow::bail!("line ratios must be positive numbers, got '{part}'");
        }
        out.push(v);
    }
    if out.is_empty() {
        anyhow::bail!("--line-ratios must not be empty");
    }
    Ok(out)
}

fn main() -> Result<()> {
    let args = Args::parse();

    // --preview-out never prints, so it behaves like --no-print for the
    // purpose of opening the printer.
    let no_print = args.no_print || args.preview_out.is_some();

    // Replace literal "\n" (two characters: backslash + n) with a real
    // newline, so the user can write --  "line1\nline2" on the command line
    // without needing shell $'...' quoting.
    // --detect-tape: query the loaded tape width, print it as JSON on
    // stdout, and exit. TEXT is neither required nor used in this mode.
    if args.detect_tape {
        let printer = open_printer(&args)?;
        let (detected, index) = printer.get_tape_width_with_index()?;
        println!("{}", detect_tape_json(detected, index));
        return Ok(());
    }

    let text = args
        .text
        .clone()
        .map(|t| t.replace("\\n", "\n"))
        .unwrap_or_default();
    if args.text.is_none() {
        anyhow::bail!("nothing to print: pass TEXT");
    }

    // Open the printer early if we might need it, either for tape width
    // auto-detection or for actually sending the job, so both can share
    // the same connection.
    let mut printer = if !no_print || args.tape_mm.is_none() {
        Some(open_printer(&args)?)
    } else {
        None
    };

    let tape = match args.tape_mm {
        Some(6) => TapeWidth::Mm6,
        Some(12) => TapeWidth::Mm12,
        Some(18) => TapeWidth::Mm18,
        Some(24) => TapeWidth::Mm24,
        Some(other) => {
            anyhow::bail!("unsupported tape width: {other}mm (only 6, 12, 18, and experimental 24 are supported)")
        }
        None => {
            let printer = printer
                .as_ref()
                .expect("printer connection opened above when --tape-mm is omitted");
            let (detected, index) = printer.get_tape_width_with_index()?;
            match detected {
                Some(tape) => {
                    eprintln!("detected {}mm tape loaded in printer", tape.width_mm());
                    tape
                }
                None => anyhow::bail!(
                    "detected tape width index {index} from the printer, but this width \
                     isn't supported yet (only 6, 12, 18, and experimental 24mm are); pass --tape-mm explicitly \
                     if you know the width, or see docs/protocol.md section 1.5 for the index \
                     mapping"
                ),
            }
        }
    };

    let renderer = match &args.font {
        Some(path) => FontRenderer::load(path)?,
        None => FontRenderer::from_bytes(DEFAULT_FONT_BYTES)
            .map_err(|e| anyhow::anyhow!("bundled font failed to load: {e}"))?,
    };

    let line_ratios = match &args.line_ratios {
        Some(s) => Some(parse_line_ratios(s)?),
        None => None,
    };
    if line_ratios.is_some() && args.max_tape_length_mm.is_some() {
        anyhow::bail!(
            "--line-ratios cannot be combined with --max-tape-length-mm (wrap mode); \
             ratios apply only to explicit newline-separated lines"
        );
    }

    let (rows, margin) = if let Some(ratios) = &line_ratios {
        // With ratios, --size (when given) is the largest-ratio line's
        // size, so the existing would_clip warning on it bounds every
        // line.
        if let Some(size) = args.size {
            if renderer.would_clip(size, tape, args.top_margin, args.bottom_margin) {
                let max = renderer.max_size_pt_for_tape(tape, args.top_margin, args.bottom_margin);
                eprintln!(
                    "warning: {size}pt may be too large for {}mm tape and could be \
                     clipped at the top/bottom; try --size {max:.1} or smaller",
                    tape.width_mm()
                );
            }
        }
        renderer.render_text_ratios(
            &text,
            args.size,
            ratios,
            tape,
            args.left_margin_mm,
            args.right_margin_mm,
            args.top_margin,
            args.bottom_margin,
        )?
    } else if let Some(tape_length_mm) = args.max_tape_length_mm {
        // Multi-line mode: wrap text to fit the specified tape length.
        let size = match args.size {
            Some(size) => {
                // Validate manually since would_clip is tape-width-based,
                // but the per-band check happens inside render_text_multiline.
                if renderer.would_clip(size, tape, args.top_margin, args.bottom_margin) {
                    let max = renderer.max_size_pt_for_tape(tape, args.top_margin, args.bottom_margin);
                    eprintln!(
                        "warning: {size}pt may be too large for {}mm tape and could be \
                         clipped at the top/bottom; try --size {max:.1} or smaller",
                        tape.width_mm()
                    );
                }
                Some(size)
            }
            None => None,
        };
        renderer.render_text_multiline(
            &text,
            size,
            tape,
            args.left_margin_mm,
            args.right_margin_mm,
            tape_length_mm,
            args.align,
            args.top_margin,
            args.bottom_margin,
        )
    } else {
        // Single-line mode, or implicit multi-line via '\n'.
        let num_lines = text.lines().count().max(1);
        let size = match args.size {
            Some(size) => {
                if renderer.would_clip(size, tape, args.top_margin, args.bottom_margin) {
                    let max = renderer.max_size_pt_for_tape(tape, args.top_margin, args.bottom_margin);
                    eprintln!(
                        "warning: {size}pt may be too large for {}mm tape and could be \
                         clipped at the top/bottom; try --size {max:.1} or smaller",
                        tape.width_mm()
                    );
                }
                size
            }
            None => {
                let max = renderer.max_size_pt_for_tape(tape, args.top_margin, args.bottom_margin);
                let size = max / num_lines as f32;
                if num_lines > 1 {
                    eprintln!("no --size given; using {size:.1}pt ({num_lines} lines on {}mm tape)", tape.width_mm());
                } else {
                    eprintln!("no --size given; using {size:.1}pt (largest that fits {}mm tape)", tape.width_mm());
                }
                size
            }
        };
        renderer.render_text(&text, size, tape, args.left_margin_mm, args.right_margin_mm, args.top_margin, args.bottom_margin)
    };

    // Optional frame: post-process the rendered rows so the same
    // frame appears in the print job and in every preview path.
    let (mut rows, margin) = (rows, margin);
    if args.frame {
        if !(1..=3).contains(&args.frame_width) {
            anyhow::bail!("--frame-width must be between 1 and 3, got {}", args.frame_width);
        }
        tpsr3700::font::apply_frame(
            &mut rows,
            tape.bitmap_bits(),
            args.top_margin,
            args.bottom_margin,
            args.frame_width,
        );
    }

    let char_count = text.chars().count() as u32;
    let job = build_print_job(tape, char_count, &rows, margin);

    if let Some(path) = &args.dump {
        std::fs::write(path, &job)?;
        eprintln!("wrote {} bytes to {}", job.len(), path.display());
        dump_explain(&job, tape, args.top_margin, args.bottom_margin);
    }

    // --preview-out: write the SVG preview (rendered from the exact
    // raster rows that would be printed) and exit. Never prompts and
    // never sends the job to the printer.
    if let Some(path) = &args.preview_out {
        #[cfg(feature = "preview")]
        {
            let right_margin_rows = tpsr3700::font::mm_to_rows(args.right_margin_mm);
            let svg = tpsr3700::preview::build_svg_preview(
                &rows,
                tape,
                &text,
                args.top_margin,
                args.bottom_margin,
                margin,
                right_margin_rows,
            );
            std::fs::write(path, svg)?;
            eprintln!("preview written to {}", path.display());
            return Ok(());
        }
        #[cfg(not(feature = "preview"))]
        {
            anyhow::bail!("--preview-out requires the crate to be built with the `preview` feature");
        }
    }

    if !args.skip_preview {
        #[cfg(feature = "preview")]
        {
            let right_margin_rows = tpsr3700::font::mm_to_rows(args.right_margin_mm);
            match tpsr3700::preview::show_preview(
                &rows,
                tape,
                &text,
                args.top_margin,
                args.bottom_margin,
                margin,
                right_margin_rows,
            )? {
                tpsr3700::preview::PreviewResult::Print => {}
                tpsr3700::preview::PreviewResult::Cancel => {
                    eprintln!("cancelled by user in preview; not printing.");
                    return Ok(());
                }
            }
        }
        #[cfg(not(feature = "preview"))]
        {
            anyhow::bail!(
                "the print preview requires the crate to be built with the \
                 `preview` feature; pass --skip-preview to print without previewing"
            );
        }
    }

    if !args.no_print {
        let printer = printer
            .as_mut()
            .expect("printer connection opened above when --no-print is not set");
        printer.send_job(&job)?;
        eprintln!("sent job to printer ({} bytes)", job.len());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{detect_tape_json, nominal_mm_for_detected_index, parse_line_ratios};
    use tpsr3700::tepra::TapeWidth;

    #[test]
    fn nominal_mm_matches_protocol_table() {
        // docs/protocol.md section 1.5: [4, 6, 9, 12, 18, 24, 36, 50]
        assert_eq!(nominal_mm_for_detected_index(0), Some(4));
        assert_eq!(nominal_mm_for_detected_index(1), Some(6));
        assert_eq!(nominal_mm_for_detected_index(2), Some(9));
        assert_eq!(nominal_mm_for_detected_index(3), Some(12));
        assert_eq!(nominal_mm_for_detected_index(4), Some(18));
        assert_eq!(nominal_mm_for_detected_index(5), Some(24));
        assert_eq!(nominal_mm_for_detected_index(6), Some(36));
        assert_eq!(nominal_mm_for_detected_index(7), Some(50));
        assert_eq!(nominal_mm_for_detected_index(8), None);
        assert_eq!(nominal_mm_for_detected_index(255), None);
    }

    #[test]
    fn detect_json_supported_width() {
        assert_eq!(
            detect_tape_json(Some(TapeWidth::Mm18), 4),
            "{\"tape_mm\": 18, \"index\": 4, \"supported\": true}"
        );
        assert_eq!(
            detect_tape_json(Some(TapeWidth::Mm6), 1),
            "{\"tape_mm\": 6, \"index\": 1, \"supported\": true}"
        );
    }

    #[test]
    fn detect_json_unsupported_width_reports_nominal_mm() {
        // 9mm (index 2) and 36mm (index 6) are real widths the driver
        // cannot print to yet; they must still be reported by name.
        assert_eq!(
            detect_tape_json(None, 2),
            "{\"tape_mm\": 9, \"index\": 2, \"supported\": false}"
        );
        assert_eq!(
            detect_tape_json(None, 6),
            "{\"tape_mm\": 36, \"index\": 6, \"supported\": false}"
        );
    }

    #[test]
    fn detect_json_unknown_index_reports_null() {
        assert_eq!(
            detect_tape_json(None, 9),
            "{\"tape_mm\": null, \"index\": 9, \"supported\": false}"
        );
    }

    #[test]
    fn parse_ratios_basic() {
        assert_eq!(parse_line_ratios("7,3").unwrap(), vec![7.0, 3.0]);
        assert_eq!(parse_line_ratios("1,1,1").unwrap(), vec![1.0, 1.0, 1.0]);
        assert_eq!(parse_line_ratios(" 2.5 , 1 ").unwrap(), vec![2.5, 1.0]);
        assert_eq!(parse_line_ratios("5").unwrap(), vec![5.0]);
    }

    #[test]
    fn parse_ratios_rejects_bad_values() {
        assert!(parse_line_ratios("").is_err());
        assert!(parse_line_ratios("7,0").is_err());
        assert!(parse_line_ratios("7,-3").is_err());
        assert!(parse_line_ratios("7,x").is_err());
        assert!(parse_line_ratios("7,").is_err());
    }
}

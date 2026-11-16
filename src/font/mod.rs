//! Font rendering: turns text into TEPRA raster rows.
//!
//! Empirically, the TEPRA raster row index (feed direction) corresponds
//! directly to a glyph's X axis, and the bit position within a row (tape
//! width direction) corresponds directly to the glyph's Y axis — no
//! explicit rotation step is needed; rendering a string left-to-right in
//! the usual way and mapping (x, y) -> (row, bit) produces correctly
//! oriented output, confirmed against captured "A"/"B" print jobs.

use crate::tepra::{Model, Tape};
#[cfg(test)]
use crate::tepra::TapeWidth;
use anyhow::{Context, Result};

/// BIZ UDPゴシック Regular — bundled directly in the binary so no system
/// font installation is required.
pub const DEFAULT_FONT_BYTES: &[u8] =
    include_bytes!("BIZUDPGothic-Regular.ttf");

/// Empirically derived print head resolution (dots per inch), used to
/// convert a point size (as used by the TEPRA desktop app, matching the
/// `T` command's margin value in captured jobs) into fontdue's
/// pixels-per-em `px` parameter.
///
/// How the TEPRA desktop app's point sizes map to dots: an em of `pt`
/// points is rendered `pt * APP_PT_SCALE * dpi / 72` dots tall, i.e. the
/// app's "point" is 1.1 real points. Measured from the half-width
/// character pitch (exactly 0.5 em in MS Gothic) in two captures:
///
/// - SR3700P driver, 36pt "AA" (`tests/fixtures/aa.prn`): pitch 74 rows
///   at 270 dpi → em 148 dots = 36pt × 4.11 → factor 1.097.
/// - SR3900P driver, MS Gothic 16pt "TEST 123"
///   (`tests/fixtures/sr3900p-w9-full.prn` etc.): pitch 43.8 rows at
///   360 dpi → em 87.6 dots = 16pt × 5.47 → factor 1.095.
///
/// The cap height in those captures (104 and 61 dots) is 0.70 em, which
/// matches MS Gothic. Upstream used 208 "dpi" (2.89 dots/pt on the
/// SR3700P) from matching BIZ UDPGothic's wider "A" to the capture; that
/// made the same point size print ~30% smaller than the app.
const APP_PT_SCALE: f32 = 1.1;

/// Default blank feed margins (in millimeters) before and after the
/// printed content. On an SR3900P, equal data margins print with the
/// left (leading) margin visibly shorter, so the left default is 1mm
/// larger; 4mm / 3mm was chosen by printing and comparing. (Upstream's
/// defaults were 10mm / 3mm. For reference, the SR3900P driver captures
/// with the app's default margin setting carry 48 blank rows (3.4mm)
/// before the first ink and 66 (4.7mm) after it.)
pub const DEFAULT_LEFT_MARGIN_MM: f32 = 4.0;
pub const DEFAULT_RIGHT_MARGIN_MM: f32 = 3.0;

/// Symmetric margin used by the renderer tests.
pub const DEFAULT_FEED_MARGIN_MM: f32 = 3.0;

/// Minimum height of a single printed line in millimeters.
/// A 6mm tape allows at most 2 lines (6 / 3 = 2).
const MIN_LINE_HEIGHT_MM: f32 = 3.0;

/// Text alignment for multi-line (and single-line) output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    /// Left-align each line (default).
    #[default]
    Left,
    /// Center each line within the tape length.
    Center,
    /// Right-align each line.
    Right,
}

/// Characters that are turned a quarter turn in vertical Japanese
/// writing: the long vowel mark, dashes, ellipses and the bracket
/// family. Everything else keeps its ordinary orientation.
///
/// Left out on purpose: small kana, which are also nudged towards one
/// corner in careful typesetting. At label sizes the nudge is a dot or
/// two and not worth the special case.
const ROTATED_IN_VERTICAL: &[char] = &[
    'ー', '〜', '～', '…', '‥', '―', '—', '–', '‐', '－', '-',
    '（', '）', '(', ')', '「', '」', '『', '』', '【', '】',
    '〔', '〕', '〈', '〉', '《', '》', '｛', '｝', '{', '}', '[', ']',
    '：', ':', '；', ';', '＝', '=',
];

/// How to lay a label out sideways, for tape that will be applied
/// vertically (a spine label, a cable run label read from above).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerticalMode {
    /// 縦書き proper: one character after another along the feed, each
    /// turned a quarter turn so it stands upright once the tape hangs
    /// vertically. The label is as long as the text has characters, and
    /// the tape width limits how big a character can be.
    Stacked,
    /// The whole label turned a quarter turn: a line of text runs across
    /// the tape and the next line follows along the feed. The tape width
    /// is the limit on line length, which is what makes this different
    /// from simply sticking an ordinary label on sideways.
    Rotated,
}

impl std::str::FromStr for VerticalMode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "stacked" => Ok(VerticalMode::Stacked),
            "rotated" => Ok(VerticalMode::Rotated),
            other => Err(format!("unknown vertical mode '{other}' (use stacked or rotated)")),
        }
    }
}

impl std::str::FromStr for TextAlign {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "left" => Ok(TextAlign::Left),
            "center" => Ok(TextAlign::Center),
            "right" => Ok(TextAlign::Right),
            other => Err(format!("unknown alignment '{other}'; expected left, center, or right")),
        }
    }
}

/// Dots per "inch" of TEPRA point size for `model` (see
/// [`APP_PT_SCALE`]).
fn app_pt_dots_per_inch(model: Model) -> f32 {
    APP_PT_SCALE * model.dpi()
}

/// Converts a TEPRA point size into fontdue's `px` (pixels-per-em)
/// parameter for `model`'s dot pitch.
fn pt_to_px(pt: f32, model: Model) -> f32 {
    pt * app_pt_dots_per_inch(model) / 72.0
}

/// Converts fontdue's `px` (pixels-per-em) parameter for `model`'s dot
/// pitch into a TEPRA point size.
fn px_to_pt(px: f32, model: Model) -> f32 {
    px * 72.0 / app_pt_dots_per_inch(model)
}

/// A loaded font, ready to rasterize text into TEPRA raster rows.
pub struct FontRenderer {
    font: fontdue::Font,
}

impl FontRenderer {
    /// Load a font from a file path (`.ttf`, `.otf`, or `.ttc`; for `.ttc`
    /// collections, the first face is used).
    pub fn load(path: &str) -> Result<Self> {
        let data = std::fs::read(path)
            .with_context(|| format!("failed to read font file: {path}"))?;
        Self::from_bytes(&data).map_err(|e| anyhow::anyhow!("failed to parse font {path}: {e}"))
    }

    /// Parse a font directly from raw bytes (`.ttf`, `.otf`, or `.ttc`).
    /// Useful for embedding fonts via `include_bytes!` without filesystem access.
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let font = fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
            .map_err(|e| anyhow::anyhow!("failed to parse font from bytes: {e}"))?;
        Ok(Self { font })
    }

    /// Returns this font's line height (ascent - descent) in dots
    /// (fontdue `px` units) for a given `px` size. fontdue's line metrics
    /// scale linearly with `px`, so this is also usable as a per-font
    /// dots-per-`px` ratio (i.e. `line_height_px(1.0)`).
    fn line_height_px(&self, size_px: f32) -> f32 {
        match self.font.horizontal_line_metrics(size_px) {
            Some(m) => m.ascent - m.descent,
            None => size_px,
        }
    }

    /// Returns the largest point size at which this font's line height
    /// (ascent - descent) fits within `tape`'s usable dot range.
    /// Used for the `--size` warning and single-line mode default size.
    ///
    /// `top_margin_bits` / `bottom_margin_bits` are the number of non-printing
    /// dots at the respective tape edge (default: [`TapeWidth::HEAD_MARGIN_BITS`]
    /// each).
    pub fn max_size_pt_for_tape(
        &self,
        tape: Tape,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
    ) -> f32 {
        let ratio_per_px = self.line_height_px(1.0);
        let head_bits = tape
            .bitmap_bits()
            .saturating_sub(top_margin_bits + bottom_margin_bits)
            .max(1);
        let max_px = head_bits as f32 / ratio_per_px;
        px_to_pt(max_px, tape.model)
    }

    /// Returns `true` if rendering at `size_pt` would clip the top or
    /// bottom of the text on `tape` (based on the font's overall vertical
    /// metrics).
    ///
    /// `top_margin_bits` / `bottom_margin_bits` are the number of non-printing
    /// dots at the respective tape edge (default: [`TapeWidth::HEAD_MARGIN_BITS`]
    /// each).
    pub fn would_clip(
        &self,
        size_pt: f32,
        tape: Tape,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
    ) -> bool {
        let size_px = pt_to_px(size_pt, tape.model);
        let head_bits = tape
            .bitmap_bits()
            .saturating_sub(top_margin_bits + bottom_margin_bits)
            .max(1);
        self.line_height_px(size_px) > head_bits as f32
    }

    /// Render one line of text into a framebuffer using font metrics.
    ///
    /// The framebuffer uses a logical coordinate system where row 0 is the
    /// top of the line.  Glyph placement follows the font's `ascent` so all
    /// characters share a common baseline.  The framebuffer height is
    /// `ascent - descent` (i.e. `line_height_px`); glyphs are not clipped.
    ///
    /// Returns `(columns, advance_width)` where `columns[x][y]` is true if
    /// dot (x, y) is inked.  `y=0` is the topmost row.
    fn rasterize_line(&self, text: &str, size_px: f32) -> (Vec<Vec<bool>>, f32) {
        let line_metrics = self.font.horizontal_line_metrics(size_px);
        let ascent = match line_metrics {
            Some(m) => m.ascent,
            None => size_px,
        };
        let line_height = self.line_height_px(size_px).ceil() as usize;

        let mut columns: Vec<Vec<bool>> = Vec::new();
        let mut cursor_x = 0.0f32;

        for ch in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(ch, size_px);
            let glyph_width = metrics.width;
            let glyph_height = metrics.height;

            let start_col = cursor_x.round() as usize;
            while columns.len() < start_col + glyph_width.max(1) {
                columns.push(vec![false; line_height]);
            }

            for gy in 0..glyph_height {
                for gx in 0..glyph_width {
                    let coverage = bitmap[gy * glyph_width + gx];
                    if coverage > 127 {
                        let glyph_top =
                            ascent - (metrics.ymin as f32 + glyph_height as f32);
                        let row = (glyph_top + gy as f32).round() as isize;
                        if row >= 0 && (row as usize) < line_height {
                            let col = start_col + gx;
                            columns[col][row as usize] = true;
                        }
                    }
                }
            }
            cursor_x += metrics.advance_width;
        }

        (columns, cursor_x)
    }

    /// Returns the topmost inked row index across all columns, or 0 if no ink.
    fn ink_top(columns: &[Vec<bool>]) -> usize {
        let mut top: Option<usize> = None;
        for col in columns {
            for (y, &ink) in col.iter().enumerate() {
                if ink {
                    top = Some(top.map_or(y, |v| v.min(y)));
                    break; // only need the topmost in this column
                }
            }
        }
        top.unwrap_or(0)
    }

    /// Returns the span from the topmost to bottommost inked row (inclusive).
    /// Returns 0 if there is no ink.
    fn ink_height(columns: &[Vec<bool>]) -> usize {
        let mut top: Option<usize> = None;
        let mut bot: Option<usize> = None;
        for col in columns {
            for (y, &ink) in col.iter().enumerate() {
                if ink {
                    top = Some(top.map_or(y, |v| v.min(y)));
                    bot = Some(bot.map_or(y, |v| v.max(y)));
                }
            }
        }
        match (top, bot) {
            (Some(t), Some(b)) => b - t + 1,
            _ => 0,
        }
    }

    /// Place a pre-rasterized framebuffer into the tape bitmap.
    ///
    /// `bit_start_signed` is the framebuffer row-0 position within the
    /// printable area (may be negative when centering pushes the top up).
    /// Only rows that fall within `[band_start, band_start + band_head_bits)`
    /// are written, so ink never bleeds into adjacent bands.
    fn place_framebuf_at(
        framebuf: &[Vec<bool>],
        merged: &mut Vec<Vec<bool>>,
        total_bits: usize,
        head_offset: usize,
        head_bits: usize,
        bit_start_signed: isize,
        band_start: usize,
        band_head_bits: usize,
        h_offset: usize,
        content_dots: usize,
    ) {
        let band_end = band_start + band_head_bits;
        for (ci, col_bits) in framebuf.iter().enumerate() {
            let dest_col = h_offset + ci;
            if dest_col >= content_dots {
                break;
            }
            for (row, &ink) in col_bits.iter().enumerate() {
                if !ink {
                    continue;
                }
                let abs_head_row = bit_start_signed + row as isize;
                if abs_head_row < 0 {
                    continue;
                }
                let abs_head_row = abs_head_row as usize;
                // Clamp to this line's band.
                if abs_head_row < band_start || abs_head_row >= band_end {
                    continue;
                }
                if abs_head_row >= head_bits {
                    continue;
                }
                // Mirror within printable area, then offset into bitmap.
                let bit_pos = head_offset + (head_bits - 1 - abs_head_row);
                if bit_pos < total_bits {
                    merged[dest_col][bit_pos] = true;
                }
            }
        }
    }

    /// Horizontal (feed-direction) offset of a line of `line_width_dots`
    /// within a `content_dots`-wide block for `align`.
    fn h_offset_for(align: TextAlign, content_dots: usize, line_width_dots: usize) -> usize {
        let slack = content_dots.saturating_sub(line_width_dots);
        match align {
            TextAlign::Left => 0,
            TextAlign::Center => slack / 2,
            TextAlign::Right => slack,
        }
    }

    /// Render `text` at the given point size (matching the size shown in
    /// the TEPRA desktop app, e.g. 36pt) into a sequence of TEPRA raster
    /// rows for the given tape width, with `left_margin_mm` / `right_margin_mm`
    /// millimeters of blank tape feed added before and after the printed
    /// content (see [`DEFAULT_FEED_MARGIN_MM`]).
    ///
    /// If `text` contains `'\n'` characters the text is split into lines and
    /// each line is rendered into an equal vertical band, matching the
    /// behaviour of [`render_text_multiline`] but without a fixed tape
    /// length; lines are left-aligned (see [`render_text_aligned`]).
    ///
    /// Returns `(rows, margin_rows)` where `margin_rows` is the number of
    /// leading (left) blank rows (matching the `T` command semantics), or
    /// an error if the text has more lines than fit on the tape (nothing
    /// is ever silently dropped).
    pub fn render_text(
        &self,
        text: &str,
        size_pt: f32,
        tape: Tape,
        left_margin_mm: f32,
        right_margin_mm: f32,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
    ) -> Result<(Vec<Vec<u8>>, u32)> {
        self.render_text_aligned(text, size_pt, tape, left_margin_mm, right_margin_mm, top_margin_bits, bottom_margin_bits, TextAlign::Left)
    }

    /// Same as [`render_text`], aligning each line of a multi-line text
    /// within the widest line according to `align` (the label is as long
    /// as the widest line, so alignment has no effect on single-line
    /// text).
    /// Renders text sideways, for tape that will be applied vertically.
    ///
    /// Both modes share one placement rule: a segment is rasterised the
    /// ordinary way, then written into the tape with its axes swapped,
    /// so the text direction runs across the tape and the line height
    /// runs along the feed. A segment is one character in
    /// [`VerticalMode::Stacked`] and one line in [`VerticalMode::Rotated`].
    ///
    /// The tape width therefore limits how long a segment can be: a
    /// character in stacked mode, a whole line in rotated mode. Anything
    /// longer is an error rather than a silent clip.
    ///
    /// Returns the rows and the leading margin in rows, like the other
    /// `render_text*` methods.
    pub fn render_text_vertical(
        &self,
        text: &str,
        size_pt: f32,
        tape: Tape,
        left_margin_mm: f32,
        right_margin_mm: f32,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
        mode: VerticalMode,
    ) -> Result<(Vec<Vec<u8>>, u32)> {
        let size_px = pt_to_px(size_pt, tape.model);
        let total_bits = tape.bitmap_bits();
        let head_bits = total_bits
            .saturating_sub(top_margin_bits + bottom_margin_bits)
            .max(1);
        let head_offset = bottom_margin_bits;

        // A newline starts a new column in stacked mode and a new line in
        // rotated mode. Either way the text splits on it first.
        let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
        if lines.iter().all(|l| l.trim().is_empty()) {
            anyhow::bail!("nothing to print");
        }

        // Stacked columns sit side by side across the tape, so the width
        // has to be shared out between them; rotated lines follow one
        // another along the feed and each gets the whole width.
        let band = match mode {
            VerticalMode::Stacked => {
                let max_columns =
                    ((tape.width_mm() as f32 / MIN_LINE_HEIGHT_MM).floor() as usize).max(1);
                if lines.len() > max_columns {
                    anyhow::bail!(
                        "too many columns ({}) for {}mm tape (max {max_columns}); \
                         shorten the text or use wider tape",
                        lines.len(),
                        tape.width_mm()
                    );
                }
                head_bits / lines.len()
            }
            VerticalMode::Rotated => head_bits,
        };

        let mut columns: Vec<Vec<bool>> = Vec::new();
        // Where the next rotated line starts along the feed. Stacked
        // columns each start from the beginning instead.
        let mut feed_cursor = 0usize;
        for (index, line) in lines.iter().enumerate() {
            // The band this line is written into, measured from the far
            // edge of the tape: in vertical writing the next column sits
            // to the left of the one before it, which here means lower
            // bits.
            let band_lo = match mode {
                VerticalMode::Stacked => head_bits - (index + 1) * band,
                VerticalMode::Rotated => 0,
            };
            // One character at a time when stacking, the whole line at
            // once when rotating.
            let segments: Vec<String> = match mode {
                VerticalMode::Stacked => line.chars().map(|c| c.to_string()).collect(),
                VerticalMode::Rotated => vec![line.to_string()],
            };
            let mut row_cursor = match mode {
                VerticalMode::Stacked => 0,
                VerticalMode::Rotated => feed_cursor,
            };
            for segment in &segments {
                let (cols, _advance) = self.rasterize_line(segment, size_px);
                let width = cols.len();
                let height = cols.first().map(|c| c.len()).unwrap_or(0);
                if height == 0 {
                    continue;
                }
                if width > band {
                    let what = match mode {
                        VerticalMode::Stacked => "character",
                        VerticalMode::Rotated => "line",
                    };
                    anyhow::bail!(
                        "this {what} needs {width} dots across {}mm tape, which has {band} \
                         for it; use a smaller --size, fewer columns, or wider tape",
                        tape.width_mm()
                    );
                }
                // Centre within the band, then write with the axes
                // swapped: the text direction becomes the tape width and
                // the line height becomes the feed.
                let lead = band_lo + (band - width) / 2;
                if columns.len() < row_cursor + height {
                    columns.resize(row_cursor + height, vec![false; total_bits]);
                }
                // In vertical writing a few characters are themselves
                // turned a quarter turn: the long vowel mark reads as a
                // vertical stroke, brackets open downwards.
                let turn = mode == VerticalMode::Stacked
                    && segment.chars().next().is_some_and(|c| ROTATED_IN_VERTICAL.contains(&c));
                let turned_lead = band_lo + band.saturating_sub(height) / 2;
                for (x, col) in cols.iter().enumerate() {
                    for (y, &ink) in col.iter().enumerate() {
                        if !ink {
                            continue;
                        }
                        // Hold the tape with the end that came out first
                        // at the top: the feed then runs downwards, which
                        // is the reading direction, and the text direction
                        // runs across the tape towards the far edge.
                        let (row, bit) = if turn {
                            (row_cursor + x, head_offset + turned_lead + (height - 1 - y))
                        } else {
                            (row_cursor + y, head_offset + lead + x)
                        };
                        if bit < total_bits && row < columns.len() {
                            columns[row][bit] = true;
                        }
                    }
                }
                row_cursor += height;
            }
            feed_cursor = row_cursor.max(feed_cursor);
        }

        let rows: Vec<Vec<u8>> = columns
            .iter()
            .map(|col| pack_bits(col, tape.bitmap_bytes()))
            .collect();
        let first = rows.iter().position(|r| r.iter().any(|&b| b != 0));
        let last = rows.iter().rposition(|r| r.iter().any(|&b| b != 0));
        let trimmed = match (first, last) {
            (Some(first), Some(last)) => rows[first..=last].to_vec(),
            _ => anyhow::bail!("nothing to print"),
        };
        let left_rows = tape.mm_to_rows(left_margin_mm);
        let right_rows = tape.mm_to_rows(right_margin_mm);
        let blank = vec![0u8; tape.bitmap_bytes()];
        let mut padded =
            Vec::with_capacity(trimmed.len() + left_rows as usize + right_rows as usize);
        padded.extend(std::iter::repeat(blank.clone()).take(left_rows as usize));
        padded.extend(trimmed);
        padded.extend(std::iter::repeat(blank).take(right_rows as usize));
        Ok((padded, left_rows))
    }

    pub fn render_text_aligned(
        &self,
        text: &str,
        size_pt: f32,
        tape: Tape,
        left_margin_mm: f32,
        right_margin_mm: f32,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
        align: TextAlign,
    ) -> Result<(Vec<Vec<u8>>, u32)> {
        let size_px = pt_to_px(size_pt, tape.model);
        let total_bits = tape.bitmap_bits();
        let head_bits = tape
            .bitmap_bits()
            .saturating_sub(top_margin_bits + bottom_margin_bits)
            .max(1);
        let head_offset = bottom_margin_bits;

        // Multi-line path: split on '\n' and render each band separately.
        let line_strs: Vec<&str> = text.split('\n').collect();
        if line_strs.len() > 1 {
            let max_lines = ((tape.width_mm() as f32 / MIN_LINE_HEIGHT_MM).floor() as usize).max(1);
            if line_strs.len() > max_lines {
                anyhow::bail!(
                    "too many lines ({}) for {}mm tape (max {max_lines}); shorten the text or use wider tape",
                    line_strs.len(),
                    tape.width_mm()
                );
            }
            let num_lines = line_strs.len();
            let band_head_bits = head_bits / num_lines;

            // Rasterize each line and find the widest advance width.
            let rasterized: Vec<(Vec<Vec<bool>>, f32)> = line_strs[..num_lines]
                .iter()
                .map(|s| self.rasterize_line(s, size_px))
                .collect();

            let max_width = rasterized
                .iter()
                .map(|(_, w)| w.ceil() as usize)
                .max()
                .unwrap_or(1)
                .max(1);

            let mut merged: Vec<Vec<bool>> = vec![vec![false; total_bits]; max_width];

            for (line_idx, (fb, advance)) in rasterized.iter().enumerate() {
                let band_start = line_idx * band_head_bits;
                let ink_h = Self::ink_height(fb);
                let ink_top = Self::ink_top(fb);
                let v_offset = if ink_h < band_head_bits {
                    (band_head_bits - ink_h) / 2
                } else {
                    0
                };
                let bit_start_signed =
                    band_start as isize + v_offset as isize - ink_top as isize;
                let h_offset = Self::h_offset_for(align, max_width, advance.ceil() as usize);

                Self::place_framebuf_at(
                    fb,
                    &mut merged,
                    total_bits,
                    head_offset,
                    head_bits,
                    bit_start_signed,
                    band_start,
                    band_head_bits,
                    h_offset,
                    max_width,
                );
            }

            let rows: Vec<Vec<u8>> = merged
                .iter()
                .map(|col| pack_bits(col, tape.bitmap_bytes()))
                .collect();

            let first_nonblank = rows.iter().position(|r| r.iter().any(|&b| b != 0));
            let last_nonblank = rows.iter().rposition(|r| r.iter().any(|&b| b != 0));
            let trimmed = match (first_nonblank, last_nonblank) {
                (Some(first), Some(last)) => rows[first..=last].to_vec(),
                _ => Vec::new(),
            };
            let left_rows = tape.mm_to_rows(left_margin_mm);
            let right_rows = tape.mm_to_rows(right_margin_mm);
            let blank_row = vec![0u8; tape.bitmap_bytes()];
            let mut padded = Vec::with_capacity(
                trimmed.len() + left_rows as usize + right_rows as usize,
            );
            padded.extend(std::iter::repeat(blank_row.clone()).take(left_rows as usize));
            padded.extend(trimmed);
            padded.extend(std::iter::repeat(blank_row).take(right_rows as usize));
            return Ok((padded, left_rows));
        }

        // Single-line path (original behaviour).
        let bit_depth = total_bits;
        let mut columns: Vec<Vec<bool>> = Vec::new();

        // Align glyphs to the top of the printable area, using the font's
        // overall ascent extent (not per-glyph metrics), so that all
        // characters in a string sit on a common baseline.
        let line_metrics = self.font.horizontal_line_metrics(size_px);
        let ascent = match line_metrics {
            Some(m) => m.ascent,
            None => size_px,
        };

        let mut cursor_x = 0.0f32;
        for ch in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(ch, size_px);
            let glyph_width = metrics.width;
            let glyph_height = metrics.height;

            // Ensure we have enough columns allocated.
            let start_col = cursor_x.round() as usize;
            while columns.len() < start_col + glyph_width {
                columns.push(vec![false; bit_depth]);
            }

            for gy in 0..glyph_height {
                for gx in 0..glyph_width {
                    let coverage = bitmap[gy * glyph_width + gx];
                    if coverage > 127 {
                        // fontdue's bitmap is top-left origin, with row 0
                        // at `ascent - (ymin + height)` relative to the
                        // baseline. Convert to a row within the printable area.
                        let glyph_top = ascent - (metrics.ymin as f32 + glyph_height as f32);
                        let row = (glyph_top + gy as f32).round() as isize;
                        // Clip to printable area.
                        if row < 0 || row as usize >= head_bits {
                            continue;
                        }
                        // Mirror within printable area, then offset into bitmap.
                        // Confirmed against real-hardware printouts of "AB" on
                        // 6mm tape.
                        let bit_pos = head_offset + (head_bits - 1 - row as usize);
                        if bit_pos < bit_depth {
                            let col = start_col + gx;
                            columns[col][bit_pos] = true;
                        }
                    }
                }
            }

            cursor_x += metrics.advance_width;
        }

        let rows: Vec<Vec<u8>> = columns
            .iter()
            .map(|col| pack_bits(col, tape.bitmap_bytes()))
            .collect();

        // Trim leading/trailing all-blank columns (any inherent glyph side
        // bearing), then add an explicit feed margin of blank rows before
        // and after -- this is a fixed physical distance (tape feed),
        // independent of glyph shape, unlike the side-bearing trimming
        // above.
        let first_nonblank = rows.iter().position(|r| r.iter().any(|&b| b != 0));
        let last_nonblank = rows.iter().rposition(|r| r.iter().any(|&b| b != 0));

        let trimmed = match (first_nonblank, last_nonblank) {
            (Some(first), Some(last)) => rows[first..=last].to_vec(),
            _ => Vec::new(),
        };

        let left_rows = tape.mm_to_rows(left_margin_mm);
        let right_rows = tape.mm_to_rows(right_margin_mm);
        let blank_row = vec![0u8; tape.bitmap_bytes()];

        let mut padded = Vec::with_capacity(trimmed.len() + left_rows as usize + right_rows as usize);
        padded.extend(std::iter::repeat(blank_row.clone()).take(left_rows as usize));
        padded.extend(trimmed);
        padded.extend(std::iter::repeat(blank_row).take(right_rows as usize));

        Ok((padded, left_rows))
    }

    /// Validate per-line size ratios (used by [`render_text_ratios`]).
    ///
    /// Ratios must be non-empty, and every value must be finite and > 0.
    fn validate_line_ratios(line_ratios: &[f32]) -> Result<()> {
        if line_ratios.is_empty() {
            anyhow::bail!("line ratios must not be empty");
        }
        for &r in line_ratios {
            if !r.is_finite() || r <= 0.0 {
                anyhow::bail!("line ratios must be positive numbers, got {r}");
            }
        }
        Ok(())
    }

    /// Band boundaries (in head-bit coordinates) when the printable
    /// height is divided proportionally to `ratios`. Returns one
    /// `(start, end)` pair per line; the bands are contiguous and the
    /// last band always ends exactly at `head_bits`.
    fn ratio_band_bounds(head_bits: usize, ratios: &[f32]) -> Vec<(usize, usize)> {
        let sum: f32 = ratios.iter().sum();
        let mut bounds = Vec::with_capacity(ratios.len());
        let mut start = 0usize;
        let mut cum = 0.0f32;
        for (i, &r) in ratios.iter().enumerate() {
            cum += r;
            let end = if i + 1 == ratios.len() {
                head_bits
            } else {
                (head_bits as f32 * cum / sum).round() as usize
            };
            bounds.push((start, end.max(start)));
            start = end.max(start);
        }
        bounds
    }

    /// Per-line point sizes for the given ratios — the single place that
    /// defines the size semantics of `--line-ratios`:
    ///
    /// - `size_pt = Some(s)`: `s` is the size of the largest-ratio line;
    ///   line *i* gets `s * r_i / max(r)`. No line ever exceeds `s`, so
    ///   the usual [`would_clip`] check on `s` bounds every line.
    /// - `size_pt = None` (auto): line *i* gets
    ///   `max_size_pt_for_tape * r_i / sum(r)`, i.e. each line's font
    ///   line-height exactly fills its proportional band (see
    ///   [`ratio_band_bounds`]). Equal ratios reduce to the legacy
    ///   behaviour of `max / num_lines` per line in equal bands.
    ///
    /// Assumes `ratios` has already been validated.
    pub fn line_sizes_for_ratios(
        &self,
        size_pt: Option<f32>,
        ratios: &[f32],
        tape: Tape,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
    ) -> Vec<f32> {
        match size_pt {
            Some(s) => {
                let max_r = ratios.iter().cloned().fold(0.0f32, f32::max);
                ratios.iter().map(|&r| s * r / max_r).collect()
            }
            None => {
                let max = self.max_size_pt_for_tape(tape, top_margin_bits, bottom_margin_bits);
                let sum: f32 = ratios.iter().sum();
                ratios.iter().map(|&r| max * r / sum).collect()
            }
        }
    }

    /// Render newline-separated multi-line `text` with a per-line font
    /// size ratio (`--line-ratios`).
    ///
    /// The number of ratios must equal the number of `\n`-separated
    /// lines; a mismatch is an error (never silently ignored). Each
    /// line is centred vertically within its own band, and the bands
    /// divide the printable height proportionally to the ratios.
    /// Per-line sizes follow [`line_sizes_for_ratios`].
    ///
    /// This is the ratio-aware counterpart of the multi-line path of
    /// [`render_text`]; wrap mode ([`render_text_multiline`]) does not
    /// support ratios.
    pub fn render_text_ratios(
        &self,
        text: &str,
        size_pt: Option<f32>,
        line_ratios: &[f32],
        tape: Tape,
        left_margin_mm: f32,
        right_margin_mm: f32,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
    ) -> Result<(Vec<Vec<u8>>, u32)> {
        self.render_text_ratios_aligned(text, size_pt, line_ratios, tape, left_margin_mm, right_margin_mm, top_margin_bits, bottom_margin_bits, TextAlign::Left, None)
    }

    /// Same as [`render_text_ratios`], aligning each line according to
    /// `align` within the widest line, or — when `fixed_length_mm` is
    /// given — within a label of exactly that length (margins included).
    /// With a fixed length the text is never wrapped: if the widest line
    /// does not fit between the margins this returns an error.
    pub fn render_text_ratios_aligned(
        &self,
        text: &str,
        size_pt: Option<f32>,
        line_ratios: &[f32],
        tape: Tape,
        left_margin_mm: f32,
        right_margin_mm: f32,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
        align: TextAlign,
        fixed_length_mm: Option<f32>,
    ) -> Result<(Vec<Vec<u8>>, u32)> {
        Self::validate_line_ratios(line_ratios)?;

        let line_strs: Vec<&str> = text.split('\n').collect();
        if line_strs.len() != line_ratios.len() {
            anyhow::bail!(
                "line ratio count ({}) does not match line count ({})",
                line_ratios.len(),
                line_strs.len()
            );
        }
        let max_lines = ((tape.width_mm() as f32 / MIN_LINE_HEIGHT_MM).floor() as usize).max(1);
        if line_strs.len() > max_lines {
            anyhow::bail!(
                "too many lines ({}) for {}mm tape (max {max_lines})",
                line_strs.len(),
                tape.width_mm()
            );
        }

        // Single line: ratios carry no information (share is 1.0). (With a
        // fixed length fall through so the length and alignment apply.)
        if line_strs.len() == 1 && fixed_length_mm.is_none() {
            let size = size_pt
                .unwrap_or_else(|| self.max_size_pt_for_tape(tape, top_margin_bits, bottom_margin_bits));
            return self.render_text(
                text,
                size,
                tape,
                left_margin_mm,
                right_margin_mm,
                top_margin_bits,
                bottom_margin_bits,
            );
        }

        let total_bits = tape.bitmap_bits();
        let head_bits = tape
            .bitmap_bits()
            .saturating_sub(top_margin_bits + bottom_margin_bits)
            .max(1);
        let head_offset = bottom_margin_bits;

        let sizes = self.line_sizes_for_ratios(
            size_pt,
            line_ratios,
            tape,
            top_margin_bits,
            bottom_margin_bits,
        );
        let bounds = Self::ratio_band_bounds(head_bits, line_ratios);

        // Rasterize each line at its own size.
        let rasterized: Vec<(Vec<Vec<bool>>, f32)> = line_strs
            .iter()
            .zip(&sizes)
            .map(|(s, &pt)| self.rasterize_line(s, pt_to_px(pt, tape.model)))
            .collect();

        let max_width = rasterized
            .iter()
            .map(|(_, w)| w.ceil() as usize)
            .max()
            .unwrap_or(1)
            .max(1);

        let left_rows = tape.mm_to_rows(left_margin_mm);
        let right_rows = tape.mm_to_rows(right_margin_mm);
        // Width of the printed block: the widest line, or the space
        // between the margins on a fixed-length label.
        let content_dots = match fixed_length_mm {
            None => max_width,
            Some(len_mm) => {
                let total_rows = tape.mm_to_rows(len_mm) as usize;
                let margins = (left_rows + right_rows) as usize;
                if total_rows <= margins {
                    anyhow::bail!(
                        "label length {len_mm}mm leaves no room for text after the {}mm + {}mm margins",
                        left_margin_mm, right_margin_mm
                    );
                }
                let content = total_rows - margins;
                if max_width > content {
                    anyhow::bail!(
                        "text is {:.1}mm wide but only {:.1}mm fits on a {len_mm}mm label after the margins; \
                         wrapping is not supported with --line-ratios, so shorten the text, reduce the size, \
                         or use a longer label",
                        tape.rows_to_mm(max_width as u32),
                        tape.rows_to_mm(content as u32)
                    );
                }
                content
            }
        };

        let mut merged: Vec<Vec<bool>> = vec![vec![false; total_bits]; content_dots];

        for (line_idx, (fb, advance)) in rasterized.iter().enumerate() {
            let (band_start, band_end) = bounds[line_idx];
            let band_head_bits = band_end - band_start;
            let ink_h = Self::ink_height(fb);
            let ink_top = Self::ink_top(fb);
            let v_offset = if ink_h < band_head_bits {
                (band_head_bits - ink_h) / 2
            } else {
                0
            };
            let bit_start_signed = band_start as isize + v_offset as isize - ink_top as isize;
            let h_offset = Self::h_offset_for(align, content_dots, advance.ceil() as usize);

            Self::place_framebuf_at(
                fb,
                &mut merged,
                total_bits,
                head_offset,
                head_bits,
                bit_start_signed,
                band_start,
                band_head_bits,
                h_offset,
                content_dots,
            );
        }

        let rows: Vec<Vec<u8>> = merged
            .iter()
            .map(|col| pack_bits(col, tape.bitmap_bytes()))
            .collect();

        // Free length: trim blank columns so the label hugs the ink. Fixed
        // length: keep the whole content block so the label is exactly as
        // long as requested.
        let trimmed = if fixed_length_mm.is_some() {
            rows
        } else {
            let first_nonblank = rows.iter().position(|r| r.iter().any(|&b| b != 0));
            let last_nonblank = rows.iter().rposition(|r| r.iter().any(|&b| b != 0));
            match (first_nonblank, last_nonblank) {
                (Some(first), Some(last)) => rows[first..=last].to_vec(),
                _ => Vec::new(),
            }
        };
        let blank_row = vec![0u8; tape.bitmap_bytes()];
        let mut padded = Vec::with_capacity(trimmed.len() + left_rows as usize + right_rows as usize);
        padded.extend(std::iter::repeat(blank_row.clone()).take(left_rows as usize));
        padded.extend(trimmed);
        padded.extend(std::iter::repeat(blank_row).take(right_rows as usize));
        Ok((padded, left_rows))
    }

    /// Multi-line variant of [`render_text`].
    ///
    /// Algorithm:
    /// 1. Count `num_lines` from explicit `\n` separators (≥ 1).
    ///    If `num_lines > max_lines`, clamp to `max_lines`.
    /// 2. `band_head_bits = head_bits / num_lines` — fixed for the whole job.
    /// 3. For each segment (in order):
    ///    a. Rasterize at a provisional size and split at `content_length_dots`.
    ///       Overflow is prepended back to the pending queue (same band).
    ///    b. Rasterize the fitting text at the provisional size, measure the
    ///       actual ink height (`ink_h`) from the bitmap.
    ///    c. Scale: `final_px = provisional_px * band_head_bits / ink_h`.
    ///    d. Re-check width at `final_px`.  If overflow, prepend to pending
    ///       and repeat (b–d) until stable.
    ///    e. If pending now has more segments than remaining bands, and
    ///       `num_lines < max_lines`, restart with `num_lines + 1`.
    ///       Otherwise discard excess segments.
    /// 4. Render each confirmed line into the tape bitmap, centering the ink
    ///    vertically within its band.
    pub fn render_text_multiline(
        &self,
        text: &str,
        size_pt: Option<f32>,
        tape: Tape,
        left_margin_mm: f32,
        right_margin_mm: f32,
        tape_length_mm: f32,
        align: TextAlign,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
    ) -> Result<(Vec<Vec<u8>>, u32)> {
        let total_bits = tape.bitmap_bits();
        let head_bits = tape
            .bitmap_bits()
            .saturating_sub(top_margin_bits + bottom_margin_bits)
            .max(1);
        let head_offset = bottom_margin_bits;
        let left_rows = tape.mm_to_rows(left_margin_mm);
        let right_rows = tape.mm_to_rows(right_margin_mm);
        let content_length_dots =
            (tape.mm_to_rows(tape_length_mm) as f32 - left_rows as f32 - right_rows as f32).max(1.0);
        let content_dots = content_length_dots as usize;

        let max_lines = (tape.width_mm() as f32 / MIN_LINE_HEIGHT_MM).floor() as usize;
        let max_lines = max_lines.max(1);

        // Step 1: split on explicit \n.  num_lines is fixed by the \n count,
        // but may grow (up to max_lines) if wrap overflow exceeds remaining bands.
        let initial_segments: Vec<String> = text
            .split('\n')
            .map(|s| s.to_string())
            .collect();

        if initial_segments.len() > max_lines {
            anyhow::bail!(
                "too many lines ({}) for {}mm tape (max {max_lines}); shorten the text or use wider tape",
                initial_segments.len(),
                tape.width_mm()
            );
        }
        let mut num_lines = initial_segments.len().max(1);

        // Outer retry loop: re-run with num_lines+1 when overflow can't be absorbed.
        let confirmed: Vec<(String, f32)> = 'retry: loop {
            let band_head_bits = head_bits / num_lines;

            // provisional_px: size such that font's ascent–descent fills one band.
            let provisional_px = match size_pt {
                Some(pt) => pt_to_px(pt, tape.model),
                None => {
                    let ratio = self.line_height_px(1.0);
                    band_head_bits as f32 / ratio
                }
            };

            let mut pending: std::collections::VecDeque<String> =
                initial_segments.iter().cloned().collect();
            let mut confirmed: Vec<(String, f32)> = Vec::new();

            'band: loop {
                let Some(segment) = pending.pop_front() else { break 'band };
                let bands_left = num_lines - confirmed.len();

                // Step 3a: split at content width using provisional size.
                let (mut fit_text, mut overflow) =
                    self.split_at_width(&segment, provisional_px, content_length_dots);

                // Step 3b–d: measure actual ink height, scale, re-check width.
                let final_px = loop {
                    let (fb, _) = self.rasterize_line(&fit_text, provisional_px);
                    let ink_h = Self::ink_height(&fb);
                    let scaled_px = if ink_h > 0 {
                        provisional_px * band_head_bits as f32 / ink_h as f32
                    } else {
                        provisional_px
                    };

                    let (refit, new_ov) =
                        self.split_at_width(&fit_text, scaled_px, content_length_dots);
                    if refit == fit_text {
                        break scaled_px;
                    }
                    // scaled_px caused new overflow — merge and retry with shorter text.
                    overflow = if overflow.is_empty() {
                        new_ov
                    } else {
                        format!("{}{}", new_ov, overflow)
                    };
                    fit_text = refit;
                };

                // Prepend leftover back to pending.
                if !overflow.is_empty() {
                    pending.push_front(overflow);
                }

                confirmed.push((fit_text, final_px));

                // Step 3e: check if pending overflows remaining bands.
                let remaining_bands = bands_left - 1; // we just used one
                if pending.len() > remaining_bands {
                    if num_lines < max_lines {
                        // Restart with one more line.
                        num_lines += 1;
                        continue 'retry;
                    } else {
                        // Can't grow further: refuse rather than drop text.
                        anyhow::bail!(
                            "text does not fit on a {tape_length_mm}mm label at this size \
                             (even with {max_lines} lines on {}mm tape); shorten the text, \
                             reduce the size, or use a longer label",
                            tape.width_mm()
                        );
                    }
                }

                if confirmed.len() >= num_lines {
                    break 'band;
                }
            }

            break 'retry confirmed;
        };

        // Step 4: render confirmed lines into tape bitmap.
        let num_confirmed = confirmed.len().max(1);
        let band_head_bits = head_bits / num_confirmed;

        let mut merged: Vec<Vec<bool>> = vec![vec![false; total_bits]; content_dots];

        for (line_idx, (line_text, final_px)) in confirmed.iter().enumerate() {
            let band_start = line_idx * band_head_bits;

            let (fb, advance) = self.rasterize_line(line_text, *final_px);

            // Center ink vertically within the band.
            let ink_h = Self::ink_height(&fb);
            let ink_top = Self::ink_top(&fb);
            let v_offset = if ink_h < band_head_bits {
                (band_head_bits - ink_h) / 2
            } else {
                0
            };
            // bit_start: where row 0 of this band lands, adjusted so that
            // the ink block is centred.  We subtract ink_top so the
            // framebuffer's logical row 0 maps to (band_start + v_offset - ink_top).
            let bit_start_signed =
                band_start as isize + v_offset as isize - ink_top as isize;

            let line_width_dots = advance.round() as usize;
            let h_offset = Self::h_offset_for(align, content_dots, line_width_dots);

            Self::place_framebuf_at(
                &fb,
                &mut merged,
                total_bits,
                head_offset,
                head_bits,
                bit_start_signed,
                band_start,
                band_head_bits,
                h_offset,
                content_dots,
            );
        }

        // Pack, trim, add margins.
        let content_rows: Vec<Vec<u8>> = merged
            .iter()
            .map(|col| pack_bits(col, tape.bitmap_bytes()))
            .collect();

        let first_nonblank = content_rows.iter().position(|r| r.iter().any(|&b| b != 0));
        let last_nonblank = content_rows.iter().rposition(|r| r.iter().any(|&b| b != 0));
        let trimmed = match (first_nonblank, last_nonblank) {
            (Some(f), Some(l)) => content_rows[f..=l].to_vec(),
            _ => Vec::new(),
        };

        let blank_row = vec![0u8; tape.bitmap_bytes()];
        let mut padded = Vec::with_capacity(trimmed.len() + left_rows as usize + right_rows as usize);
        padded.extend(std::iter::repeat(blank_row.clone()).take(left_rows as usize));
        padded.extend(trimmed);
        padded.extend(std::iter::repeat(blank_row).take(right_rows as usize));

        Ok((padded, left_rows))
    }

    /// Split `text` at the first character that would cause the cumulative
    /// advance width to exceed `max_width` when rasterized at `size_px`.
    /// Returns `(fit, overflow)` where `fit` is the prefix that fits and
    /// `overflow` is the remainder (may be empty).
    fn split_at_width(&self, text: &str, size_px: f32, max_width: f32) -> (String, String) {
        let mut width = 0.0f32;
        let mut split_byte = text.len(); // default: everything fits
        for (byte_idx, ch) in text.char_indices() {
            let (metrics, _) = self.font.rasterize(ch, size_px);
            if width + metrics.advance_width > max_width && byte_idx > 0 {
                split_byte = byte_idx;
                break;
            }
            width += metrics.advance_width;
        }
        (text[..split_byte].to_string(), text[split_byte..].to_string())
    }
}

/// Padding (in dots) left between the ink bounding box and the frame
/// drawn by [`apply_frame`]. At 208 dpi this is roughly 1 mm.
pub const FRAME_PAD_DOTS: usize = 8;

fn row_bit_is_set(row: &[u8], bit: usize) -> bool {
    let byte_idx = bit / 8;
    byte_idx < row.len() && (row[byte_idx] >> (7 - (bit % 8))) & 1 == 1
}

fn set_row_bit(row: &mut [u8], bit: usize) {
    let byte_idx = bit / 8;
    if byte_idx < row.len() {
        row[byte_idx] |= 1 << (7 - (bit % 8));
    }
}

/// Draws a rectangular frame around the printed content of `rows`.
///
/// This is a pure post-processing step over the raster rows produced by
/// any of the `render_text*` methods, so it composes with single-line,
/// multi-line, ratio and wrap rendering alike, and (because the preview
/// is built from the same rows) it shows up in previews unchanged.
///
/// The frame hugs the bounding box of all ink in `rows`, expanded by
/// [`FRAME_PAD_DOTS`] on every side and clamped to the printable band
/// `[bottom_margin_bits, total_bits - top_margin_bits)` in the tape-width
/// (bit) direction and to the row range in the feed direction.
/// `thickness` is the frame line width in dots (clamped to 1..=3).
/// Rows without any ink are left untouched.
pub fn apply_frame(
    rows: &mut [Vec<u8>],
    total_bits: usize,
    top_margin_bits: usize,
    bottom_margin_bits: usize,
    thickness: usize,
) {
    apply_frame_sized(rows, total_bits, top_margin_bits, bottom_margin_bits, thickness, None)
}

/// Like [`apply_frame`], but with an explicit frame width in the feed
/// direction (the label's horizontal direction as it comes out of the
/// printer), given in rows.
///
/// `width_rows` of `None` keeps the default behaviour of hugging the
/// ink. With `Some(w)` the frame is `w` rows wide and centred on the
/// label rather than on the ink, so labels of the same length get an
/// identical box wherever their text happens to sit; `w` is clamped to
/// the label's length. The extent across the tape still comes from the
/// ink, as before.
pub fn apply_frame_sized(
    rows: &mut [Vec<u8>],
    total_bits: usize,
    top_margin_bits: usize,
    bottom_margin_bits: usize,
    thickness: usize,
    width_rows: Option<usize>,
) {
    let thickness = thickness.clamp(1, 3);
    if rows.is_empty() || total_bits == 0 {
        return;
    }
    // Find the ink bounding box.
    let mut first_row: Option<usize> = None;
    let mut last_row = 0usize;
    let mut min_bit = usize::MAX;
    let mut max_bit = 0usize;
    for (r, row) in rows.iter().enumerate() {
        for bit in 0..total_bits {
            if row_bit_is_set(row, bit) {
                if first_row.is_none() {
                    first_row = Some(r);
                }
                last_row = r;
                min_bit = min_bit.min(bit);
                max_bit = max_bit.max(bit);
            }
        }
    }
    let Some(first_row) = first_row else {
        return; // no ink: nothing to frame
    };
    let (row_lo, row_hi) = match width_rows {
        Some(w) if w > 0 => {
            let w = w.min(rows.len());
            // Centre the box on the label, not on the ink.
            let lo = (rows.len() / 2).saturating_sub(w / 2).min(rows.len() - w);
            (lo, lo + w - 1)
        }
        _ => (
            first_row.saturating_sub(FRAME_PAD_DOTS),
            (last_row + FRAME_PAD_DOTS).min(rows.len() - 1),
        ),
    };
    let band_lo = bottom_margin_bits.min(total_bits - 1);
    let band_hi = total_bits.saturating_sub(top_margin_bits).saturating_sub(1).max(band_lo);
    let bit_lo = min_bit.saturating_sub(FRAME_PAD_DOTS).max(band_lo);
    let bit_hi = (max_bit + FRAME_PAD_DOTS).min(band_hi);
    if row_hi < row_lo || bit_hi < bit_lo {
        return;
    }
    // Lines parallel to the tape edges (constant feed position, spanning
    // the bit range) at both ends of the frame.
    for r in row_lo..=(row_lo + thickness - 1).min(row_hi) {
        for bit in bit_lo..=bit_hi {
            set_row_bit(&mut rows[r], bit);
        }
    }
    for r in (row_hi + 1).saturating_sub(thickness)..=row_hi {
        for bit in bit_lo..=bit_hi {
            set_row_bit(&mut rows[r], bit);
        }
    }
    // Lines along the feed direction (constant bit, spanning the row
    // range) at both edges of the printable band side.
    for row in rows.iter_mut().take(row_hi + 1).skip(row_lo) {
        for bit in bit_lo..=(bit_lo + thickness - 1).min(bit_hi) {
            set_row_bit(row, bit);
        }
        for bit in (bit_hi + 1).saturating_sub(thickness)..=bit_hi {
            set_row_bit(row, bit);
        }
    }
}

/// Packs a boolean slice (MSB-first) into bytes.
fn pack_bits(bits: &[bool], out_len: usize) -> Vec<u8> {
    let mut out = vec![0u8; out_len];
    for (i, &bit) in bits.iter().enumerate() {
        if bit {
            let byte_idx = i / 8;
            let bit_idx = 7 - (i % 8);
            if byte_idx < out.len() {
                out[byte_idx] |= 1 << bit_idx;
            }
        }
    }
    out
}

/// Converts raster rows into an ASCII-art string for snapshot testing.
/// `#`=ink  `.`=blank  `|`=non-printing margin boundary at `head_offset`
/// and `head_offset + head_bits`.
/// Feed direction is downsampled to at most `max_rows` lines.
#[cfg(test)]
fn rows_to_ascii(
    rows: &[Vec<u8>],
    total_bits: usize,
    head_offset: usize,
    head_bits: usize,
    max_rows: usize,
) -> String {
    let step = (rows.len() / max_rows).max(1);
    rows.iter()
        .enumerate()
        .filter(|(i, _)| i % step == 0)
        .map(|(_, row)| {
            (0..total_bits)
                .map(|bit| {
                    if bit == head_offset || bit == head_offset + head_bits {
                        '|'
                    } else {
                        let byte_idx = bit / 8;
                        let bit_idx = 7 - (bit % 8);
                        if byte_idx < row.len() && (row[byte_idx] >> bit_idx) & 1 == 1 {
                            '#'
                        } else {
                            '.'
                        }
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_renderer() -> FontRenderer {
        const FONT_BYTES: &[u8] = include_bytes!("BIZUDPGothic-Regular.ttf");
        FontRenderer::from_bytes(FONT_BYTES).expect("bundled test font must parse")
    }

    #[test]
    fn max_size_shrinks_with_narrower_tape() {
        let renderer = default_renderer();
        let max_6mm = renderer.max_size_pt_for_tape(Tape::sr3700p(TapeWidth::Mm6), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        let max_9mm = renderer.max_size_pt_for_tape(Tape::sr3700p(TapeWidth::Mm9), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        let max_12mm = renderer.max_size_pt_for_tape(Tape::sr3700p(TapeWidth::Mm12), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        let max_18mm = renderer.max_size_pt_for_tape(Tape::sr3700p(TapeWidth::Mm18), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        let max_24mm = renderer.max_size_pt_for_tape(Tape::sr3700p(TapeWidth::Mm24), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        let max_36mm = renderer.max_size_pt_for_tape(Tape::sr3700p(TapeWidth::Mm36), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        assert!(max_6mm < max_9mm);
        assert!(max_9mm < max_12mm);
        assert!(max_12mm < max_18mm);
        assert!(max_18mm < max_24mm);
        // On the SR3700P both 24mm and 36mm are limited by the 192-dot head.
        assert_eq!(max_24mm, max_36mm);

        // On the SR3900P every width is strictly wider than the previous.
        let sizes: Vec<f32> = TapeWidth::ALL
            .iter()
            .map(|w| renderer.max_size_pt_for_tape(w.on(Model::Sr3900p), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS))
            .collect();
        for pair in sizes.windows(2) {
            assert!(pair[0] < pair[1], "{sizes:?}");
        }
        // Point sizes are scaled per model so a similar physical band gives
        // a similar point size: 18mm is 168 dots @ 270 dpi on the SR3700P
        // (15.8mm) and 216 dots @ 360 dpi on the SR3900P (15.2mm).
        let max_18mm_3900 = renderer.max_size_pt_for_tape(TapeWidth::Mm18.on(Model::Sr3900p), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        assert!((max_18mm_3900 / max_18mm - 1.0).abs() < 0.05, "{max_18mm_3900} vs {max_18mm}");
    }

    /// Returns the first and last inked row (feed direction) within the
    /// band of `line_idx` out of `num_lines` equal bands.
    fn band_ink_extent(rows: &[Vec<u8>], tape: Tape, line_idx: usize, num_lines: usize, top: usize, bottom: usize) -> (usize, usize) {
        let total_bits = tape.bitmap_bits();
        let head_bits = total_bits - top - bottom;
        let band = head_bits / num_lines;
        // Bands are mirrored: line 0 is the top band = highest bit positions.
        let band_start = bottom + (num_lines - 1 - line_idx) * band;
        let bits = band_start..band_start + band;
        let inked: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| bits.clone().any(|b| (r[b / 8] >> (7 - b % 8)) & 1 == 1))
            .map(|(i, _)| i)
            .collect();
        (*inked.first().unwrap(), *inked.last().unwrap())
    }

    #[test]
    fn multiline_without_length_honours_alignment() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let text = "WWWWWWWW\ni";
        let mut extents = Vec::new();
        for align in [TextAlign::Left, TextAlign::Center, TextAlign::Right] {
            let (rows, margin) = renderer.render_text_aligned(text, 12.0, tape, 0.0, 0.0, 2, 0, align).unwrap();
            assert_eq!(margin, 0);
            let long = band_ink_extent(&rows, tape, 0, 2, 2, 0);
            let short = band_ink_extent(&rows, tape, 1, 2, 2, 0);
            extents.push((align, long, short));
        }
        let (_, long, left) = extents[0];
        let (_, _, center) = extents[1];
        let (_, _, right) = extents[2];
        // The long line spans the label in every mode; the short line moves.
        assert!(left.0 < center.0 && center.0 < right.0, "{extents:?}");
        assert!(left.0 <= long.0 + 2, "left-aligned short line starts at the left edge: {extents:?}");
        // The advance width includes the glyph's side bearing, so allow a few dots.
        assert!(right.1 + 12 >= long.1, "right-aligned short line ends at the right edge: {extents:?}");
        let mid_long = (long.0 + long.1) / 2;
        let mid_center = (center.0 + center.1) / 2;
        // Centred by advance width, so the ink can sit a few dots off (side bearings).
        assert!((mid_long as isize - mid_center as isize).abs() <= 8, "centred short line is centred: {extents:?}");

        // Ratios path, same expectation.
        let (rows_l, _) = renderer.render_text_ratios_aligned(text, None, &[1.0, 1.0], tape, 0.0, 0.0, 2, 0, TextAlign::Left, None).unwrap();
        let (rows_r, _) = renderer.render_text_ratios_aligned(text, None, &[1.0, 1.0], tape, 0.0, 0.0, 2, 0, TextAlign::Right, None).unwrap();
        let s_l = band_ink_extent(&rows_l, tape, 1, 2, 2, 0);
        let s_r = band_ink_extent(&rows_r, tape, 1, 2, 2, 0);
        assert!(s_l.0 < s_r.0, "ratios: {s_l:?} vs {s_r:?}");
    }

    #[test]
    fn ratios_with_fixed_length_pad_and_align_without_wrapping() {
        let renderer = default_renderer();
        let tape = TapeWidth::Mm18.on(Model::Sr3900p);
        let text = "HOST\n10.0.0.1";
        // Fits: the label is exactly the requested length and the short
        // line moves with the alignment.
        let (rows_l, margin) = renderer
            .render_text_ratios_aligned(text, Some(14.0), &[7.0, 3.0], tape, 4.0, 3.0, 2, 0, TextAlign::Left, Some(60.0))
            .unwrap();
        assert_eq!(rows_l.len() as u32, tape.mm_to_rows(60.0));
        assert_eq!(margin, tape.mm_to_rows(4.0));
        let (rows_r, _) = renderer
            .render_text_ratios_aligned(text, Some(14.0), &[7.0, 3.0], tape, 4.0, 3.0, 2, 0, TextAlign::Right, Some(60.0))
            .unwrap();
        assert_eq!(rows_r.len(), rows_l.len());
        let first_ink = |rows: &[Vec<u8>]| rows.iter().position(|r| r.iter().any(|&b| b != 0)).unwrap();
        assert!(first_ink(&rows_r) > first_ink(&rows_l));

        // Single line with a fixed length is padded too.
        let (one, _) = renderer
            .render_text_ratios_aligned("HOST", Some(14.0), &[1.0], tape, 4.0, 3.0, 2, 0, TextAlign::Center, Some(60.0))
            .unwrap();
        assert_eq!(one.len() as u32, tape.mm_to_rows(60.0));

        // Too long for the label: an error, never wrapping.
        let err = renderer
            .render_text_ratios_aligned(text, Some(14.0), &[7.0, 3.0], tape, 4.0, 3.0, 2, 0, TextAlign::Left, Some(15.0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("fits"), "{err}");
        let err = renderer
            .render_text_ratios_aligned(text, Some(14.0), &[7.0, 3.0], tape, 4.0, 3.0, 2, 0, TextAlign::Left, Some(5.0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("margins"), "{err}");
    }

    #[test]
    fn too_many_lines_is_an_error_not_a_truncation() {
        // 6mm tape fits 2 lines (3mm minimum each); a third line must not
        // be silently dropped on any render path.
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let two = renderer.render_text("A\nB", 8.0, tape, 0.0, 0.0, 2, 0).unwrap();
        let err = renderer.render_text("A\nB\nC", 8.0, tape, 0.0, 0.0, 2, 0).unwrap_err().to_string();
        assert!(err.contains("too many lines"), "{err}");
        assert!(!two.0.is_empty());
        let err = renderer
            .render_text_multiline("A\nB\nC", Some(8.0), tape, 0.0, 0.0, 40.0, TextAlign::Left, 2, 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("too many lines"), "{err}");
        // Wrap mode: text that cannot fit even with the maximum line count
        // is refused instead of having its tail discarded.
        let err = renderer
            .render_text_multiline("WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW", Some(20.0), tape, 0.0, 0.0, 12.0, TextAlign::Left, 2, 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not fit"), "{err}");
    }

    #[test]
    fn point_scale_matches_captured_character_pitch() {
        // 0.5 em half-width pitch in the captures (see APP_PT_SCALE).
        let em_3700 = pt_to_px(36.0, Model::Sr3700p);
        assert!((em_3700 / 2.0 - 74.0).abs() < 1.0, "SR3700P 36pt half-width pitch {}", em_3700 / 2.0);
        let em_3900 = pt_to_px(16.0, Model::Sr3900p);
        assert!((em_3900 / 2.0 - 43.8).abs() < 1.0, "SR3900P 16pt half-width pitch {}", em_3900 / 2.0);
        // Round trip.
        assert!((px_to_pt(pt_to_px(12.5, Model::Sr3900p), Model::Sr3900p) - 12.5).abs() < 1e-4);
    }

    #[test]
    fn max_size_does_not_clip() {
        let renderer = default_renderer();
        for tape in TapeWidth::ALL.map(Tape::sr3700p) {
            let max = renderer.max_size_pt_for_tape(tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
            assert!(!renderer.would_clip(max, tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS));
            // A couple of points larger should clip.
            assert!(renderer.would_clip(max + 2.0, tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS));
        }
    }

    #[test]
    fn would_clip_detects_oversized_36pt_on_6mm() {
        let renderer = default_renderer();
        // 36pt was captured to clip visibly on 12mm tape in real hardware
        // testing; the much narrower 6mm tape should clip it too.
        assert!(renderer.would_clip(36.0, Tape::sr3700p(TapeWidth::Mm6), TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS));
    }

    #[test]
    fn text_align_from_str() {
        assert_eq!("left".parse::<TextAlign>().unwrap(), TextAlign::Left);
        assert_eq!("center".parse::<TextAlign>().unwrap(), TextAlign::Center);
        assert_eq!("right".parse::<TextAlign>().unwrap(), TextAlign::Right);
        assert!("other".parse::<TextAlign>().is_err());
    }

    #[test]
    fn split_at_width_fits_single_line() {
        let renderer = default_renderer();
        let size_px = pt_to_px(12.0, Model::Sr3700p);
        // A very wide max width: everything fits, overflow is empty.
        let (fit, overflow) = renderer.split_at_width("ABC", size_px, 10000.0);
        assert_eq!(fit, "ABC");
        assert_eq!(overflow, "");
    }

    #[test]
    fn split_at_width_wraps_when_narrow() {
        let renderer = default_renderer();
        let size_px = pt_to_px(12.0, Model::Sr3700p);
        // Width=1: only the first char (or nothing extra) fits.
        let (fit, overflow) = renderer.split_at_width("ABC", size_px, 1.0);
        assert!(!overflow.is_empty(), "expected overflow for narrow width");
        assert!(fit.len() < "ABC".len());
    }

    #[test]
    fn split_at_width_explicit_newline_handled_by_caller() {
        let renderer = default_renderer();
        let size_px = pt_to_px(12.0, Model::Sr3700p);
        // \n splitting is done before split_at_width (by render_text_multiline).
        // split_at_width sees each segment without \n.
        let segments: Vec<&str> = "AB\nCD".split('\n').collect();
        assert_eq!(segments.len(), 2);
        let (fit0, ov0) = renderer.split_at_width(segments[0], size_px, 10000.0);
        let (fit1, ov1) = renderer.split_at_width(segments[1], size_px, 10000.0);
        assert_eq!(fit0, "AB");
        assert_eq!(ov0, "");
        assert_eq!(fit1, "CD");
        assert_eq!(ov1, "");
    }

    #[test]
    fn split_at_width_empty_segment() {
        let renderer = default_renderer();
        let size_px = pt_to_px(12.0, Model::Sr3700p);
        let (fit, overflow) = renderer.split_at_width("", size_px, 10000.0);
        assert_eq!(fit, "");
        assert_eq!(overflow, "");
    }

    #[test]
    fn render_multiline_produces_rows() {
        let renderer = default_renderer();
        let (rows, margin) = renderer.render_text_multiline(
            "Hello World",
            None,
            Tape::sr3700p(TapeWidth::Mm18),
            DEFAULT_FEED_MARGIN_MM,
            DEFAULT_FEED_MARGIN_MM,
            50.0,
            TextAlign::Left,
            TapeWidth::HEAD_MARGIN_BITS,
            TapeWidth::HEAD_MARGIN_BITS,
        ).unwrap();
        assert!(!rows.is_empty());
        assert!(margin > 0);
    }

    /// Asserts that the ASCII-art rendering of `rows` matches `expected`.
    /// On failure prints both expected and actual art.
    #[track_caller]
    fn assert_ascii_art(
        rows: &[Vec<u8>],
        tape: Tape,
        top_margin_bits: usize,
        bottom_margin_bits: usize,
        max_cols: usize,
        expected: &str,
    ) {
        let total_bits = tape.bitmap_bits();
        let head_offset = bottom_margin_bits;
        let head_bits = total_bits.saturating_sub(top_margin_bits + bottom_margin_bits);
        let actual = rows_to_ascii(rows, total_bits, head_offset, head_bits, max_cols);
        assert_eq!(
            actual, expected,
            "\nASCII art mismatch.\nExpected:\n{expected}\nActual:\n{actual}"
        );
    }

    /// Returns true if any bit in `[bit_start, bit_end)` is set in any row.
    fn any_ink_in_bit_range(rows: &[Vec<u8>], bit_start: usize, bit_end: usize) -> bool {
        rows.iter().any(|row| {
            (bit_start..bit_end).any(|bit| {
                let byte_idx = bit / 8;
                let bit_idx = 7 - (bit % 8);
                byte_idx < row.len() && (row[byte_idx] >> bit_idx) & 1 == 1
            })
        })
    }

    #[test]
    fn multiline_uses_both_bands_when_explicit_newline() {
        // With an explicit \n the text is split into two lines.
        // Line 0 occupies the first half of the printable area (bit_start=0),
        // line 1 occupies the second half (bit_start=head_bits/2).
        // Both halves should contain ink.
        let renderer = default_renderer();
        for tape in [Tape::sr3700p(TapeWidth::Mm12), Tape::sr3700p(TapeWidth::Mm18)] {
            let (rows, _margin) = renderer.render_text_multiline(
                "D39M-06N\n420V20A",
                None,
                tape,
                DEFAULT_FEED_MARGIN_MM,
                DEFAULT_FEED_MARGIN_MM,
                32.0,
                TextAlign::Left,
                TapeWidth::HEAD_MARGIN_BITS,
                TapeWidth::HEAD_MARGIN_BITS,
            ).unwrap();
            let ho = tape.head_offset();
            let hb = tape.head_bits();
            let half = hb / 2;
            // Line 0: bit_start=0, mirrored into [ho + half .. ho + hb)
            assert!(
                any_ink_in_bit_range(&rows, ho + half, ho + hb),
                "expected ink in first-line band on {}mm tape",
                tape.width_mm()
            );
            // Line 1: bit_start=half, mirrored into [ho .. ho + half)
            assert!(
                any_ink_in_bit_range(&rows, ho, ho + half),
                "expected ink in second-line band on {}mm tape",
                tape.width_mm()
            );
        }
    }

    /// Snapshot test: `--tape-length-mm 32 "D39M-06N 420V20A"` on 6mm tape.
    ///
    /// The auto-size logic picks nl=2 (half-height font) because that yields a
    /// larger point size than nl=1 for this text/length combination.  At that
    /// smaller font size the whole text fits on a single line, so wrap_chars
    /// returns 1 line and render_text_multiline renders it as one line using
    /// the full tape height.  This test pins that behaviour.
    #[test]
    fn multiline_6mm_tape_length_32_fits_single_line() {
        let renderer = default_renderer();
        let text = "D39M-06N 420V20A";
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let tape_length_mm = 32.0;

        let (rows, _margin) = renderer.render_text_multiline(
            text,
            None,
            tape,
            DEFAULT_FEED_MARGIN_MM,
            DEFAULT_FEED_MARGIN_MM,
            tape_length_mm,
            TextAlign::Left,
            TapeWidth::HEAD_MARGIN_BITS,
            TapeWidth::HEAD_MARGIN_BITS,
        ).unwrap();

        // Output must be non-empty and contain ink somewhere.
        assert!(!rows.is_empty());
        let total_bits = tape.bitmap_bits();
        assert!(any_ink_in_bit_range(&rows, 0, total_bits));
    }

    /// Regression test: `"D39pb"` on 6mm tape.
    ///
    /// `p` and `b` have descenders that extend below the baseline.
    /// Verifies that:
    /// 1. Output is non-empty and contains ink.
    /// 2. Ink is present in both the upper half (ascenders: D, 3, 9) and
    ///    the lower half (descenders: p, b) of the printable area.
    ///    The printable area spans [head_offset .. head_offset + head_bits).
    ///    With top-aligned rendering and the font's ascent placing the
    ///    baseline roughly 70% down, ascenders land in the upper portion
    ///    and descenders extend into the lower portion.
    #[test]
    fn render_6mm_descender_text_not_clipped() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let (rows, _margin) = renderer.render_text_multiline(
            "D39pb",
            None,
            tape,
            DEFAULT_FEED_MARGIN_MM,
            DEFAULT_FEED_MARGIN_MM,
            32.0,
            TextAlign::Left,
            TapeWidth::HEAD_MARGIN_BITS,
            TapeWidth::HEAD_MARGIN_BITS,
        ).unwrap();

        assert!(!rows.is_empty(), "expected non-empty output");

        assert_ascii_art(&rows, tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS, 120, "\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|...##################################..|.......\n\
........|...##################################..|.......\n\
........|...##################################..|.......\n\
........|...##################################..|.......\n\
........|...###............................###..|.......\n\
........|...###............................###..|.......\n\
........|...###............................###..|.......\n\
........|...###............................###..|.......\n\
........|...###............................###..|.......\n\
........|...###............................###..|.......\n\
........|...###............................###..|.......\n\
........|...###............................###..|.......\n\
........|...####...........................###..|.......\n\
........|...####..........................####..|.......\n\
........|...####..........................####..|.......\n\
........|...####..........................####..|.......\n\
........|....###..........................###...|.......\n\
........|....####........................####...|.......\n\
........|....####........................####...|.......\n\
........|.....####......................####....|.......\n\
........|.....#####.....................####....|.......\n\
........|......#####...................####.....|.......\n\
........|......######................######.....|.......\n\
........|.......#######.............######......|.......\n\
........|........#########......#########.......|.......\n\
........|.........######################........|.......\n\
........|...........##################..........|.......\n\
........|.............##############............|.......\n\
........|................########...............|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|......##...............................|.......\n\
........|......###........................#.....|.......\n\
........|.....####.......................###....|.......\n\
........|....#####......................####....|.......\n\
........|....####........................####...|.......\n\
........|...####.........................####...|.......\n\
........|...####..........................####..|.......\n\
........|...###.............###............###..|.......\n\
........|...###.............####...........###..|.......\n\
........|..####.............####...........###..|.......\n\
........|..####.............####...........####.|.......\n\
........|..####.............####...........####.|.......\n\
........|..####.............####...........####.|.......\n\
........|..####.............####...........####.|.......\n\
........|..####.............####...........####.|.......\n\
........|...###.............####...........###..|.......\n\
........|...###............######.........####..|.......\n\
........|...####...........##..###........####..|.......\n\
........|...#####.........###..####......####...|.......\n\
........|....#####.......####..##############...|.......\n\
........|....###############....############....|.......\n\
........|.....##############.....##########.....|.......\n\
........|......############.......########......|.......\n\
........|.......#########.......................|.......\n\
........|..........####.........................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.....................#########.........|.......\n\
........|...................#############.......|.......\n\
........|..................################.....|.......\n\
........|.................##################....|.......\n\
........|...##...........######........#####....|.......\n\
........|..###...........####...........#####...|.......\n\
........|..###..........####..............####..|.......\n\
........|..####.........####..............####..|.......\n\
........|..####.........###................###..|.......\n\
........|...###........####................####.|.......\n\
........|...###........####................####.|.......\n\
........|...####.......####................####.|.......\n\
........|...####........###................####.|.......\n\
........|....####.......###................####.|.......\n\
........|....####.......###................###..|.......\n\
........|.....####.......###..............####..|.......\n\
........|.....#####......####............#####..|.......\n\
........|......######.....####..........#####...|.......\n\
........|.......#######....#####......######....|.......\n\
........|........#########..################....|.......\n\
........|.........########################......|.......\n\
........|...........#####################.......|.......\n\
........|..............################.........|.......\n\
........|..................########.............|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|############################...........|.......\n\
........|############################...........|.......\n\
........|############################...........|.......\n\
........|############################...........|.......\n\
........|........####.........####..............|.......\n\
........|.......###............####.............|.......\n\
........|......###..............####............|.......\n\
........|.....###................###............|.......\n\
........|.....###................####...........|.......\n\
........|....####.................###...........|.......\n\
........|....###..................###...........|.......\n\
........|....###..................###...........|.......\n\
........|....###..................###...........|.......\n\
........|....###..................###...........|.......\n\
........|....####.................###...........|.......\n\
........|....####................####...........|.......\n\
........|.....####...............####...........|.......\n\
........|.....#####.............####............|.......\n\
........|......#####..........######............|.......\n\
........|......########....########.............|.......\n\
........|.......##################..............|.......\n\
........|........################...............|.......\n\
........|..........############.................|.......\n\
........|..............#####....................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|...####################################|.......\n\
........|...####################################|.......\n\
........|...####################################|.......\n\
........|.....##################################|.......\n\
........|.....#####...........####..............|.......\n\
........|.....###..............####.............|.......\n\
........|....###................####............|.......\n\
........|...####.................###............|.......\n\
........|...###..................####...........|.......\n\
........|...###...................###...........|.......\n\
........|..####...................###...........|.......\n\
........|..####...................###...........|.......\n\
........|..####...................###...........|.......\n\
........|..####...................###...........|.......\n\
........|..####...................###...........|.......\n\
........|...####.................####...........|.......\n\
........|...####................#####...........|.......\n\
........|....####..............#####............|.......\n\
........|....######...........#####.............|.......\n\
........|.....########....#########.............|.......\n\
........|......###################..............|.......\n\
........|.......################................|.......\n\
........|.........############..................|.......\n\
........|.............#####.....................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......");
    }

    /// Snapshot test: `"D39pb"` on 6mm tape with no margins at all
    /// (top-margin=0, bottom-margin=0): the whole 56-dot row is printable.
    #[test]
    fn render_6mm_descender_no_margins() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let (rows, _margin) = renderer.render_text_multiline(
            "D39pb",
            None,
            tape,
            DEFAULT_FEED_MARGIN_MM,
            DEFAULT_FEED_MARGIN_MM,
            32.0,
            TextAlign::Left,
            0,   // top_margin_bits
            0,   // bottom_margin_bits
        ).unwrap();

        assert!(!rows.is_empty(), "expected non-empty output");

        assert_ascii_art(&rows, tape, 0, 0, 120, "\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.....###############################################...\n\
|.....###############################################...\n\
|.....###############################################...\n\
|.....####.......................................####...\n\
|.....####.......................................####...\n\
|.....####.......................................####...\n\
|.....####.......................................####...\n\
|.....####.......................................####...\n\
|.....####.......................................####...\n\
|.....#####.....................................#####...\n\
|.....#####.....................................#####...\n\
|.....######...................................######...\n\
|......#####...................................#####....\n\
|.......#####.................................#####.....\n\
|........######.............................######......\n\
|.........#######.........................#######.......\n\
|..........#########...................#########........\n\
|............#################################..........\n\
|...............###########################.............\n\
|...................###################.................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|..........#............................................\n\
|........#####.................................##.......\n\
|.......######...............................######.....\n\
|......#####..................................######....\n\
|.....#####....................................######...\n\
|....#####...................####...............#####...\n\
|....#####...................####................#####..\n\
|....####....................####.................####..\n\
|....####...................#####.................####..\n\
|....####...................#####.................####..\n\
|....#####..................######...............#####..\n\
|....#####.................########..............#####..\n\
|.....#####...............####..####............#####...\n\
|.....######.............#####..######.......#######....\n\
|......#########.....########....##################.....\n\
|........###################.......##############.......\n\
|..........###############.............#######..........\n\
|..............######...................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.................................#######...............\n\
|............................#################..........\n\
|.........................#######################.......\n\
|.....###...............#########.........#########.....\n\
|...#####..............#######................######....\n\
|...#####.............######....................#####...\n\
|....####.............#####......................#####..\n\
|....#####............####........................####..\n\
|.....#####...........####........................####..\n\
|.....######..........####........................####..\n\
|......######.........#####......................#####..\n\
|.......#######........#####....................#####...\n\
|.........#######........#####................#######...\n\
|..........#########.......######..........########.....\n\
|............#####################################......\n\
|...............###############################.........\n\
|....................######################.............\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|#######################################................\n\
|#######################################................\n\
|#######################################................\n\
|..........######...............#####...................\n\
|.........#####...................#####.................\n\
|.......#####......................#####................\n\
|.......####........................#####...............\n\
|......#####.........................####...............\n\
|......####..........................####...............\n\
|......#####.........................####...............\n\
|......#####........................#####...............\n\
|.......#####......................#####................\n\
|........######..................#######................\n\
|.........#########..........#########..................\n\
|..........##########################...................\n\
|.............####################......................\n\
|...................#########...........................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|....###################################################\n\
|....###################################################\n\
|.......################################################\n\
|........#####.................######...................\n\
|......#####......................#####.................\n\
|.....####.........................#####................\n\
|....#####..........................#####...............\n\
|....####............................####...............\n\
|....####............................####...............\n\
|....####...........................#####...............\n\
|....#####..........................#####...............\n\
|.....#####.......................######................\n\
|.....#######...................#######.................\n\
|.......#########...........##########..................\n\
|.........##########################....................\n\
|............####################.......................\n\
|.................##########............................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................\n\
|.......................................................");
    }

        /// Snapshot test: `"Dpb\nFpb"` on 6mm tape (2 lines, all with descenders).
    #[test]
    fn render_6mm_two_line_descenders() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let (rows, _margin) = renderer.render_text_multiline(
            "Dpb\nFpb",
            None,
            tape,
            DEFAULT_FEED_MARGIN_MM,
            DEFAULT_FEED_MARGIN_MM,
            32.0,
            TextAlign::Left,
            TapeWidth::HEAD_MARGIN_BITS,
            TapeWidth::HEAD_MARGIN_BITS,
        ).unwrap();

        assert!(!rows.is_empty(), "expected non-empty output");

        assert_ascii_art(&rows, tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS, 120, "\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|..################....################.|.......\n\
........|.##################..##################|.......\n\
........|.........##......##..##..............##|.......\n\
........|.........##......##..##..............##|.......\n\
........|.........##......##..##..............##|.......\n\
........|.........##......##..##..............##|.......\n\
........|.........##......##...#..............#.|.......\n\
........|.........##......##...#..............#.|.......\n\
........|.........##......##...##............##.|.......\n\
........|.........##......##...##............##.|.......\n\
........|.................#.....##..........##..|.......\n\
........|.......................###........###..|.......\n\
........|........................############...|.......\n\
........|..........................########.....|.......\n\
........|##############.........................|.......\n\
........|##############.........................|.......\n\
........|....#......##..........................|.......\n\
........|...#........#..........................|.......\n\
........|..##........##......##############.....|.......\n\
........|..##.........#......##############.....|.......\n\
........|..##.........#..........#......##......|.......\n\
........|..##........##.........#........#......|.......\n\
........|..###.......##........##........##.....|.......\n\
........|...###....###.........##.........#.....|.......\n\
........|....########..........##.........#.....|.......\n\
........|......####............##........##.....|.......\n\
........|......................###.......##.....|.......\n\
........|.......................###....###......|.......\n\
........|........................########.......|.......\n\
........|.##################.......####.........|.......\n\
........|..#################....................|.......\n\
........|...#.......##..........................|.......\n\
........|..#.........#..........................|.......\n\
........|.##.........##.......##################|.......\n\
........|.##..........#........#################|.......\n\
........|.##.........##.........#.......##......|.......\n\
........|.##.........##........#.........#......|.......\n\
........|..##.......###.......##.........##.....|.......\n\
........|..####....###........##..........#.....|.......\n\
........|...#########.........##.........##.....|.......\n\
........|.....#####...........##.........##.....|.......\n\
........|......................##.......###.....|.......\n\
........|......................####....###......|.......\n\
........|.......................#########.......|.......\n\
........|.........................#####.........|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......");
    }

    /// Snapshot test: `render_text` with `"Dpb\nFpb"` on 6mm tape (no tape-length
    /// argument — exercises the `'\n'` multi-line path inside `render_text`).
    /// The font size is set to half of `max_size_pt_for_tape` so both lines fit
    /// within their respective bands.
    #[test]
    fn render_text_6mm_newline_two_lines() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let size_pt = renderer.max_size_pt_for_tape(
            tape,
            TapeWidth::HEAD_MARGIN_BITS,
            TapeWidth::HEAD_MARGIN_BITS,
        ) / 2.0;
        let (rows, _margin) = renderer.render_text(
            "Dpb\nFpb",
            size_pt,
            tape,
            DEFAULT_FEED_MARGIN_MM,
            DEFAULT_FEED_MARGIN_MM,
            TapeWidth::HEAD_MARGIN_BITS,
            TapeWidth::HEAD_MARGIN_BITS,
        ).unwrap();

        assert!(!rows.is_empty(), "expected non-empty output");

        assert_ascii_art(&rows, tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS, 120, "\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|..################.....................|.......\n\
........|..################....################.|.......\n\
........|.........##.....##....################.|.......\n\
........|.........##.....##....#.............##.|.......\n\
........|.........##.....##....#.............##.|.......\n\
........|.........##.....##....#.............##.|.......\n\
........|.........##.....##....#.............##.|.......\n\
........|.........##.....##....#.............##.|.......\n\
........|.........##.....##....##............#..|.......\n\
........|................##....##...........##..|.......\n\
........|......................###..........##..|.......\n\
........|.......................###........##...|.......\n\
........|........................####...####....|.......\n\
........|.........................#########.....|.......\n\
........|#############...............###........|.......\n\
........|#############..........................|.......\n\
........|...##......#...........................|.......\n\
........|...#........#..........................|.......\n\
........|..##........#.......#############......|.......\n\
........|..##........##......#############......|.......\n\
........|..##........##.........##......#.......|.......\n\
........|..##........#..........#........#......|.......\n\
........|...##......##.........##........#......|.......\n\
........|...#########..........##........##.....|.......\n\
........|.....######...........##........##.....|.......\n\
........|......................##........#......|.......\n\
........|.......................##......##......|.......\n\
........|.......................#########.......|.......\n\
........|.##################......######........|.......\n\
........|..################.....................|.......\n\
........|..##.......#...........................|.......\n\
........|..#........##..........................|.......\n\
........|.##.........#........##################|.......\n\
........|.##.........##........################.|.......\n\
........|.##.........##........##.......#.......|.......\n\
........|.##........##.........#........##......|.......\n\
........|..##......###........##.........#......|.......\n\
........|...#########.........##.........##.....|.......\n\
........|....######...........##.........##.....|.......\n\
........|.....................##........##......|.......\n\
........|......................##......###......|.......\n\
........|.......................#########.......|.......\n\
........|........................######.........|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......\n\
........|.......................................|.......");
    }

    /// Snapshot test: `render_text` with `"Dpb\nFpb"` on 6mm tape,
    /// top-margin=2, bottom-margin=0 (the CLI defaults).  The size is
    /// `max_size_pt / 2` — what `main.rs` passes when `--size` is omitted
    /// and there are 2 lines. Ink must not overflow the `|` boundaries.
    #[test]
    fn render_text_autosize_newline_fits_each_band() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let top_margin = 2usize;
        let bottom_margin = 0usize;
        let max_size = renderer.max_size_pt_for_tape(tape, top_margin, bottom_margin);
        let size = max_size / 2.0; // same as main.rs: max_size / num_lines
        let (rows, _) = renderer.render_text(
            "Dpb\nFpb",
            size,
            tape,
            DEFAULT_FEED_MARGIN_MM,
            DEFAULT_FEED_MARGIN_MM,
            top_margin,
            bottom_margin,
        ).unwrap();
        assert!(!rows.is_empty(), "expected non-empty output");
        assert_ascii_art(&rows, tape, top_margin, bottom_margin, 120, "\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|..######################.....######################..|.\n\
|..######################.....######################..|.\n\
|............###.......##.....###.................##..|.\n\
|.............##.......##.....###.................##..|.\n\
|.............##.......##.....###.................##..|.\n\
|.............##.......##.....###.................##..|.\n\
|.............##.......##.....###.................##..|.\n\
|.............##.......##.....###.................##..|.\n\
|.............##.......##.....###.................##..|.\n\
|.............##.......##......##.................##..|.\n\
|.............##.......##......##................###..|.\n\
|.............##.......##......###...............##...|.\n\
|......................##.......##..............###...|.\n\
|......................##.......###............###....|.\n\
|................................####.........####....|.\n\
|.................................###############.....|.\n\
|..................................############.......|.\n\
|....................................########.........|.\n\
|##################...................................|.\n\
|##################...................................|.\n\
|.....###......###....................................|.\n\
|....###........##....................................|.\n\
|....##..........##...................................|.\n\
|...##...........##.........##################........|.\n\
|...##............##........##################........|.\n\
|...##............##.............###......###.........|.\n\
|...##...........###............###........##.........|.\n\
|...###..........##.............##..........##........|.\n\
|....##.........###............##...........##........|.\n\
|....####......####............##............##.......|.\n\
|.....############.............##............##.......|.\n\
|......##########..............##...........###.......|.\n\
|........#####.................###..........##........|.\n\
|...............................##.........###........|.\n\
|...............................####......####........|.\n\
|................................############.........|.\n\
|.................................##########..........|.\n\
|..#######################..........#####.............|.\n\
|...######################............................|.\n\
|....##........##.....................................|.\n\
|...##..........##....................................|.\n\
|...##...........##...................................|.\n\
|..##............##...........#######################.|.\n\
|..##.............##...........######################.|.\n\
|..##.............##............##........##..........|.\n\
|..##............###...........##..........##.........|.\n\
|..###...........##............##...........##........|.\n\
|...##..........###...........##............##........|.\n\
|...####......####............##.............##.......|.\n\
|....############.............##.............##.......|.\n\
|.....##########..............##............###.......|.\n\
|........#####................###...........##........|.\n\
|..............................##..........###........|.\n\
|..............................####......####.........|.\n\
|...............................############..........|.\n\
|................................##########...........|.\n\
|...................................#####.............|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.\n\
|.....................................................|.");
    }

    // ---- line ratios (--line-ratios) ----

    #[test]
    fn line_sizes_ratios_auto_are_proportional() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let max = renderer.max_size_pt_for_tape(tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        let sizes = renderer.line_sizes_for_ratios(None, &[7.0, 3.0], tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        assert_eq!(sizes.len(), 2);
        // Auto: each line gets max * ratio / sum(ratios).
        assert!((sizes[0] - max * 0.7).abs() < 0.01, "sizes[0]={} max={max}", sizes[0]);
        assert!((sizes[1] - max * 0.3).abs() < 0.01, "sizes[1]={} max={max}", sizes[1]);
        assert!((sizes[0] / sizes[1] - 7.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn line_sizes_ratios_explicit_size_is_max_line() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let sizes = renderer.line_sizes_for_ratios(Some(20.0), &[7.0, 3.0], tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        // --size is the largest-ratio line's size; others scale by r/max(r).
        assert!((sizes[0] - 20.0).abs() < 0.001);
        assert!((sizes[1] - 20.0 * 3.0 / 7.0).abs() < 0.001);
    }

    #[test]
    fn line_sizes_ratios_equal_matches_legacy() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let max = renderer.max_size_pt_for_tape(tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        let sizes = renderer.line_sizes_for_ratios(None, &[1.0, 1.0], tape, TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS);
        assert!((sizes[0] - max / 2.0).abs() < 0.01);
        assert!((sizes[1] - max / 2.0).abs() < 0.01);
    }

    #[test]
    fn ratio_band_bounds_split_proportionally() {
        // 18mm tape: head_bits = 152. 7:3 -> 106.4 rounds to 106, rest 46.
        let bounds = FontRenderer::ratio_band_bounds(152, &[7.0, 3.0]);
        assert_eq!(bounds, vec![(0, 106), (106, 152)]);
        // Equal ratios on an even split match the legacy equal bands.
        let bounds = FontRenderer::ratio_band_bounds(152, &[1.0, 1.0]);
        assert_eq!(bounds, vec![(0, 76), (76, 152)]);
        // Three lines: contiguous, last band ends at head_bits.
        let bounds = FontRenderer::ratio_band_bounds(152, &[1.0, 1.0, 1.0]);
        assert_eq!(bounds[0].0, 0);
        assert_eq!(bounds[1].0, bounds[0].1);
        assert_eq!(bounds[2].0, bounds[1].1);
        assert_eq!(bounds[2].1, 152);
    }

    #[test]
    fn render_ratios_rejects_bad_input() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let m = TapeWidth::HEAD_MARGIN_BITS;
        // Count mismatch (2 lines, 1 ratio).
        assert!(renderer.render_text_ratios("A\nB", None, &[1.0], tape, 3.0, 3.0, m, m).is_err());
        // Count mismatch (1 line, 2 ratios).
        assert!(renderer.render_text_ratios("A", None, &[1.0, 2.0], tape, 3.0, 3.0, m, m).is_err());
        // Zero / negative / NaN ratios.
        assert!(renderer.render_text_ratios("A\nB", None, &[1.0, 0.0], tape, 3.0, 3.0, m, m).is_err());
        assert!(renderer.render_text_ratios("A\nB", None, &[1.0, -1.0], tape, 3.0, 3.0, m, m).is_err());
        assert!(renderer.render_text_ratios("A\nB", None, &[1.0, f32::NAN], tape, 3.0, 3.0, m, m).is_err());
        // Empty ratios.
        assert!(renderer.render_text_ratios("A", None, &[], tape, 3.0, 3.0, m, m).is_err());
    }

    /// Ink span (max-min+1 of inked bit positions) within the bit range
    /// that corresponds to head-bit rows `[head_start, head_end)`.
    /// Bit positions are mirrored: bit = head_offset + (head_bits-1-row).
    fn band_ink_span(rows: &[Vec<u8>], tape: Tape, head_start: usize, head_end: usize) -> usize {
        let ho = tape.head_offset();
        let hb = tape.head_bits();
        let bit_lo = ho + hb - head_end;
        let bit_hi = ho + hb - head_start; // exclusive
        let mut min: Option<usize> = None;
        let mut max: Option<usize> = None;
        for row in rows {
            for bit in bit_lo..bit_hi {
                let byte_idx = bit / 8;
                let bit_idx = 7 - (bit % 8);
                if byte_idx < row.len() && (row[byte_idx] >> bit_idx) & 1 == 1 {
                    min = Some(min.map_or(bit, |v| v.min(bit)));
                    max = Some(max.map_or(bit, |v| v.max(bit)));
                }
            }
        }
        match (min, max) {
            (Some(a), Some(b)) => b - a + 1,
            _ => 0,
        }
    }

    #[test]
    fn render_ratios_equal_matches_render_text() {
        // With all ratios equal, the ratio path must produce byte-identical
        // rows to the legacy path at the auto per-line size (max / 2).
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let m = TapeWidth::HEAD_MARGIN_BITS;
        let max = renderer.max_size_pt_for_tape(tape, m, m);
        let (legacy, _) = renderer.render_text(
            "HOST\n192.168.0.1", max / 2.0, tape,
            DEFAULT_FEED_MARGIN_MM, DEFAULT_FEED_MARGIN_MM, m, m,
        ).unwrap();
        let (ratio, _) = renderer.render_text_ratios(
            "HOST\n192.168.0.1", None, &[1.0, 1.0], tape,
            DEFAULT_FEED_MARGIN_MM, DEFAULT_FEED_MARGIN_MM, m, m,
        ).unwrap();
        assert_eq!(legacy, ratio);
    }

    #[test]
    fn render_ratios_7_3_makes_first_line_bigger() {
        // The user's example: "HOST\n192.168.0.1" on 18mm tape.
        // With 7:3 the first line must render taller than the second,
        // and the long second line shrinks so the whole label gets
        // shorter than the equal-ratio rendering.
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let m = TapeWidth::HEAD_MARGIN_BITS;
        let hb = tape.head_bits();

        let (rows_eq, _) = renderer.render_text_ratios(
            "HOST\n192.168.0.1", None, &[1.0, 1.0], tape,
            DEFAULT_FEED_MARGIN_MM, DEFAULT_FEED_MARGIN_MM, m, m,
        ).unwrap();
        let (rows_73, _) = renderer.render_text_ratios(
            "HOST\n192.168.0.1", None, &[7.0, 3.0], tape,
            DEFAULT_FEED_MARGIN_MM, DEFAULT_FEED_MARGIN_MM, m, m,
        ).unwrap();

        // Equal ratios: bands are halves.
        let eq0 = band_ink_span(&rows_eq, tape, 0, hb / 2);
        let eq1 = band_ink_span(&rows_eq, tape, hb / 2, hb);
        // 7:3: bands split at round(152 * 0.7) = 106.
        let b73 = FontRenderer::ratio_band_bounds(hb, &[7.0, 3.0]);
        let s0 = band_ink_span(&rows_73, tape, b73[0].0, b73[0].1);
        let s1 = band_ink_span(&rows_73, tape, b73[1].0, b73[1].1);

        assert!(s0 > s1, "7:3 first line ink span {s0} should exceed second {s1}");
        assert!(s0 as f32 / s1 as f32 > 1.5, "7:3 ink span ratio {} too small", s0 as f32 / s1 as f32);
        assert!(s0 > eq0, "7:3 first line {s0} should be bigger than equal {eq0}");
        assert!(s1 < eq1, "7:3 second line {s1} should be smaller than equal {eq1}");
        // The narrower IP line no longer drags the label length.
        assert!(rows_73.len() < rows_eq.len(),
            "7:3 label ({} rows) should be shorter than 1:1 ({} rows)", rows_73.len(), rows_eq.len());
    }

    // Ink extents of a rendered label: (feed rows, bits across the tape).
    fn ink_extents(rows: &[Vec<u8>], total_bits: usize) -> (usize, usize) {
        let mut first_row = None;
        let mut last_row = 0;
        let mut min_bit = usize::MAX;
        let mut max_bit = 0;
        for (r, row) in rows.iter().enumerate() {
            for bit in 0..total_bits {
                if row_bit_is_set(row, bit) {
                    first_row.get_or_insert(r);
                    last_row = r;
                    min_bit = min_bit.min(bit);
                    max_bit = max_bit.max(bit);
                }
            }
        }
        match first_row {
            Some(first) => (last_row - first + 1, max_bit - min_bit + 1),
            None => (0, 0),
        }
    }

    #[test]
    fn stacked_vertical_grows_with_the_number_of_characters() {
        let renderer = default_renderer();
        let tape = TapeWidth::Mm24.on(Model::Sr3900p);
        let one = renderer
            .render_text_vertical("あ", 20.0, tape, 0.0, 0.0, 2, 0, VerticalMode::Stacked)
            .unwrap()
            .0;
        let three = renderer
            .render_text_vertical("あいう", 20.0, tape, 0.0, 0.0, 2, 0, VerticalMode::Stacked)
            .unwrap()
            .0;
        assert!(
            three.len() > one.len() * 2,
            "three characters ({} rows) should be about three times one ({} rows)",
            three.len(),
            one.len()
        );
    }

    #[test]
    fn a_newline_starts_a_new_column_when_stacked() {
        let renderer = default_renderer();
        let tape = TapeWidth::Mm24.on(Model::Sr3900p);
        let bits = tape.bitmap_bits();
        let one = renderer
            .render_text_vertical("あい", 14.0, tape, 0.0, 0.0, 2, 0, VerticalMode::Stacked)
            .unwrap()
            .0;
        let two = renderer
            .render_text_vertical("あい\nうえ", 14.0, tape, 0.0, 0.0, 2, 0, VerticalMode::Stacked)
            .unwrap()
            .0;
        // A second column sits beside the first, so the label does not get
        // longer, but it does use more of the tape width.
        assert_eq!(
            two.len(),
            one.len(),
            "a second column should not lengthen the label ({} vs {})",
            two.len(),
            one.len()
        );
        let (_, across_one) = ink_extents(&one, bits);
        let (_, across_two) = ink_extents(&two, bits);
        assert!(
            across_two > across_one,
            "two columns should span more of the tape ({across_two} vs {across_one})"
        );
    }

    #[test]
    fn more_columns_than_the_tape_allows_is_an_error() {
        let renderer = default_renderer();
        let tape = TapeWidth::Mm6.on(Model::Sr3900p);
        let err = renderer
            .render_text_vertical("あ\nい\nう", 8.0, tape, 0.0, 0.0, 2, 0, VerticalMode::Stacked)
            .unwrap_err()
            .to_string();
        assert!(err.contains("too many columns"), "{err}");
    }

    #[test]
    fn a_character_too_wide_for_the_tape_is_an_error() {
        let renderer = default_renderer();
        let tape = TapeWidth::Mm6.on(Model::Sr3900p);
        let err = renderer
            .render_text_vertical("あ", 40.0, tape, 0.0, 0.0, 2, 0, VerticalMode::Stacked)
            .unwrap_err()
            .to_string();
        assert!(err.contains("character"), "{err}");
        assert!(err.contains("6mm tape"), "{err}");
    }

    #[test]
    fn a_line_too_long_for_the_tape_is_an_error_when_rotated() {
        let renderer = default_renderer();
        let tape = TapeWidth::Mm12.on(Model::Sr3900p);
        let err = renderer
            .render_text_vertical("これは長すぎる行です", 20.0, tape, 0.0, 0.0, 2, 0, VerticalMode::Rotated)
            .unwrap_err()
            .to_string();
        assert!(err.contains("line"), "{err}");
    }

    #[test]
    fn the_long_vowel_mark_is_turned_in_stacked_mode() {
        let renderer = default_renderer();
        let tape = TapeWidth::Mm24.on(Model::Sr3900p);
        let bits = tape.bitmap_bits();
        let (rows, _) = renderer
            .render_text_vertical("ー", 20.0, tape, 0.0, 0.0, 2, 0, VerticalMode::Stacked)
            .unwrap();
        let (feed, across) = ink_extents(&rows, bits);
        // Turned, the mark is a stroke running along the feed: long in the
        // reading direction, thin across the tape.
        assert!(
            feed > across * 2,
            "long vowel mark should read as a vertical stroke (feed {feed}, across {across})"
        );
    }

    #[test]
    fn explicit_frame_width_is_centred_on_the_label() {
        // 40 rows x 32 bits, ink off to one side of the label.
        let bits = 32usize;
        let mut rows = vec![vec![0u8; bits / 8]; 40];
        set_row_bit(&mut rows[6], 16);
        apply_frame_sized(&mut rows, bits, 2, 2, 1, Some(20));
        // 20 rows wide, centred on the label: rows 10..=29, wherever the
        // ink sits. The ends are the only fully inked rows.
        for (r, want) in [(9usize, false), (10, true), (29, true), (30, false)] {
            let full = (8..=24).all(|bit| row_bit_is_set(&rows[r], bit));
            assert_eq!(full, want, "row {r} fully inked = {full}, expected {want}");
        }
        // Without a width the same label hugs the ink instead.
        let mut hugged = vec![vec![0u8; bits / 8]; 40];
        set_row_bit(&mut hugged[6], 16);
        apply_frame_sized(&mut hugged, bits, 2, 2, 1, None);
        assert!((8..=24).all(|bit| row_bit_is_set(&hugged[0], bit)), "hugging frame starts at the ink");
        assert!(!(8..=24).all(|bit| row_bit_is_set(&hugged[29], bit)), "hugging frame is shorter");
    }

    #[test]
    fn explicit_frame_width_is_clamped_to_the_label() {
        let bits = 32usize;
        let mut rows = vec![vec![0u8; bits / 8]; 12];
        set_row_bit(&mut rows[6], 16);
        apply_frame_sized(&mut rows, bits, 2, 2, 1, Some(999));
        for r in [0usize, 11] {
            assert!((8..=24).all(|bit| row_bit_is_set(&rows[r], bit)), "row {r} is an edge of the clamped frame");
        }
    }

    #[test]
    fn frame_draws_border_around_ink() {
        // 24 rows x 32 bits, one ink dot in the middle.
        let bits = 32usize;
        let bytes = bits / 8;
        let mut rows = vec![vec![0u8; bytes]; 24];
        set_row_bit(&mut rows[12], 16);
        apply_frame(&mut rows, bits, 2, 2, 2);
        // Frame should span rows 12-8=4 .. 12+8=20 and bits 16-8=8 .. 16+8=24.
        // Top edge (rows 4..=5) is fully inked across bits 8..=24.
        for r in [4usize, 5] {
            for bit in 8..=24 {
                assert!(row_bit_is_set(&rows[r], bit), "top edge row {r} bit {bit}");
            }
        }
        // Bottom edge (rows 19..=20).
        for r in [19usize, 20] {
            for bit in 8..=24 {
                assert!(row_bit_is_set(&rows[r], bit), "bottom edge row {r} bit {bit}");
            }
        }
        // Side edges: middle row 12 has ink only at bits 8..=9, 16, 23..=24.
        assert!(row_bit_is_set(&rows[12], 8));
        assert!(row_bit_is_set(&rows[12], 9));
        assert!(row_bit_is_set(&rows[12], 23));
        assert!(row_bit_is_set(&rows[12], 24));
        assert!(!row_bit_is_set(&rows[12], 12));
        assert!(!row_bit_is_set(&rows[12], 20));
        // Outside the frame: untouched.
        assert!(!row_bit_is_set(&rows[0], 16));
        assert!(!row_bit_is_set(&rows[23], 16));
        assert!(!row_bit_is_set(&rows[12], 0));
    }

    #[test]
    fn frame_clamps_to_printable_band() {
        // Ink touching bit 0: the frame must not cross into the bottom
        // margin (bits < bottom_margin_bits).
        let bits = 32usize;
        let bytes = bits / 8;
        let mut rows = vec![vec![0u8; bytes]; 10];
        set_row_bit(&mut rows[5], 0);
        apply_frame(&mut rows, bits, 2, 4, 1);
        // No new ink below the band boundary (bit 4); the original dot at
        // bit 0 in row 5 is pre-existing ink and stays, but nothing else
        // may appear in bits 0..4.
        for (r, row) in rows.iter().enumerate() {
            for bit in 0..4 {
                if r == 5 && bit == 0 {
                    continue;
                }
                assert!(!row_bit_is_set(row, bit),
                    "frame leaked into bottom margin at row {r} bit {bit}");
            }
        }
        // The frame's low edge sits exactly at the band boundary (bit 4).
        assert!(rows.iter().any(|r| row_bit_is_set(r, 4)));
    }

    #[test]
    fn frame_no_ink_is_noop() {
        let mut rows = vec![vec![0u8; 4]; 6];
        let before = rows.clone();
        apply_frame(&mut rows, 32, 2, 2, 2);
        assert_eq!(rows, before);
    }

    #[test]
    fn frame_on_rendered_text_adds_ink_and_keeps_size() {
        let renderer = default_renderer();
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let (rows, margin) = renderer.render_text(
            "Frame", 24.0, tape, 3.0, 3.0,
            TapeWidth::HEAD_MARGIN_BITS, TapeWidth::HEAD_MARGIN_BITS,
        ).unwrap();
        let mut framed = rows.clone();
        apply_frame(&mut framed, tape.bitmap_bits(), 2, 8, 2);
        assert_eq!(framed.len(), rows.len());
        let ink = |rs: &[Vec<u8>]| rs.iter().flat_map(|r| r.iter().map(|b| b.count_ones() as usize)).sum::<usize>();
        assert!(ink(&framed) > ink(&rows), "frame must add ink");
        let _ = margin;
    }

}

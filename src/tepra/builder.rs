//! Assembles the full byte stream for a TEPRA SR3700P / SR3900P print job.

use crate::tepra::frame::build_command_frame;
use crate::tepra::header::build_header;
use crate::tepra::raster::build_row;
use crate::tepra::Tape;

/// How the printer cuts the tape for this job, encoded in the `C`
/// configuration command's 4 parameter bytes.
///
/// Values observed in SR3900P Windows driver captures that differed only
/// in the cut setting (`tests/fixtures/sr3900p-w9-{full,half,none}.prn`,
/// same for 36mm):
///
/// | Mode | `C` params      |
/// |------|-----------------|
/// | Full | `03 01 01 01`   |
/// | Half | `02 02 01 01`   |
/// | None | `00 00 00 00`   |
///
/// Full cut is what every SR3700P capture used and is the default. The
/// individual byte meanings are unknown; only these three combinations
/// have been observed, so they are passed through verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CutMode {
    /// Cut the tape all the way through after the label (the default).
    #[default]
    Full,
    /// Half cut: cut the tape but leave the backing paper intact.
    Half,
    /// Do not cut.
    NoCut,
}

impl CutMode {
    /// Parameter bytes of the `C` command for this mode.
    pub fn c_params(self) -> [u8; 4] {
        match self {
            CutMode::Full => [0x03, 0x01, 0x01, 0x01],
            CutMode::Half => [0x02, 0x02, 0x01, 0x01],
            CutMode::NoCut => [0x00, 0x00, 0x00, 0x00],
        }
    }

    /// Name as accepted on the command line (`full`, `half`, `none`).
    pub fn name(self) -> &'static str {
        match self {
            CutMode::Full => "full",
            CutMode::Half => "half",
            CutMode::NoCut => "none",
        }
    }
}

impl std::str::FromStr for CutMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Ok(CutMode::Full),
            "half" => Ok(CutMode::Half),
            "none" | "no" | "off" => Ok(CutMode::NoCut),
            other => Err(format!("invalid cut mode '{other}': expected full, half, or none")),
        }
    }
}

/// Per-job settings that don't affect the raster content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobOptions {
    /// Cut behaviour (see [`CutMode`]).
    pub cut: CutMode,
    /// Number of identical labels to print in this job. Each copy is
    /// sent as its own `L`/`T`/raster block (see
    /// [`build_print_job_with`]); the printer cuts between copies
    /// according to `cut`. Values below 1 are treated as 1.
    pub copies: u32,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self { cut: CutMode::Full, copies: 1 }
    }
}

/// Length of the header block at the start of every job.
pub const HEADER_LEN: usize = 2179;

/// The part of a job that is actually sent to the printer over USB.
///
/// A USB capture of the SR3900P Windows driver printing a label shows
/// that the 2179-byte header block (`ESC { 0x80 ... }`) present in
/// `FILE:` port output is **not** transmitted: the first bulk write
/// starts at the `C` command. Sending the header to an SR3900P made it
/// sit for ~20 seconds before printing. This returns `job` without the
/// header (or unchanged if it doesn't start with one).
pub fn printer_payload(job: &[u8]) -> &[u8] {
    if job.len() > HEADER_LEN && job.starts_with(&[0x1B, 0x7B, 0x80]) {
        &job[HEADER_LEN..]
    } else {
        job
    }
}

/// Fixed parameters for the `D` and `G` configuration commands. Their
/// meaning is not understood; they were observed to be constant across
/// every captured sample (every model, tape width, cut mode and copy
/// count).
const CMD_D_PARAMS: [u8; 1] = [0x05];
const CMD_G_PARAMS: [u8; 0] = [];

/// Byte appended right after the last raster row of each copy and before
/// the next copy's `L` command or the closing `ESC { '@' '@' }` frame.
/// Its purpose is unknown; observed constant in every sample.
const TRAILING_BYTE: u8 = 0x0C;

/// Builds a complete print job byte stream from raster rows with the
/// default [`JobOptions`] (full cut, one copy).
///
/// `rows` is the full sequence of raster row bitmaps to send, in order,
/// each exactly `tape.bitmap_bytes()` long — this includes any leading or
/// trailing blank (all-zero) rows the caller wants sent (e.g. tape feed
/// margin; see [`crate::font::FontRenderer::render_text`]), not just the
/// "inked" content. `rows.len()` becomes the `L` command's value
/// directly.
///
/// `char_count` is the number of characters/glyphs being printed (used
/// for a still not-fully-understood header field that scales with it; see
/// [`crate::tepra::header`]).
///
/// `margin_rows` is the number of leading blank rows in `rows` before
/// content starts (encoded in the `T` command). This is *not* computed
/// automatically from `rows`, since an all-zero row is ambiguous between
/// "blank feed margin" and "a gap between glyphs" — the caller must track
/// and pass this explicitly.
pub fn build_print_job(
    tape: Tape,
    char_count: u32,
    rows: &[Vec<u8>],
    margin_rows: u32,
) -> Vec<u8> {
    build_print_job_with(tape, char_count, rows, margin_rows, JobOptions::default())
}

/// Same as [`build_print_job`], with explicit cut mode and copy count.
///
/// Job layout (from the SR3900P driver's two-copy captures,
/// `tests/fixtures/sr3900p-*-x2.prn`):
///
/// ```text
/// header, C, D, G, { L, T, raster rows..., 0x0C } x copies, '@'
/// ```
pub fn build_print_job_with(
    tape: Tape,
    char_count: u32,
    rows: &[Vec<u8>],
    margin_rows: u32,
    opts: JobOptions,
) -> Vec<u8> {
    build_print_job_multi(tape, char_count, &[(rows, margin_rows)], opts)
}

/// Builds one job that prints several different labels back to back.
///
/// `labels` are `(rows, margin_rows)` pairs with the same meaning as in
/// [`build_print_job`]. Each label becomes its own `L`/`T`/raster block
/// (repeated `opts.copies` times), exactly like the copies of a single
/// label in the SR3900P driver's two-copy captures, so the cut mode
/// applies between every label:
///
/// ```text
/// header, C, D, G, { L, T, raster rows..., 0x0C } × (labels × copies), '@'
/// ```
pub fn build_print_job_multi(
    tape: Tape,
    char_count: u32,
    labels: &[(&[Vec<u8>], u32)],
    opts: JobOptions,
) -> Vec<u8> {
    let mut job = Vec::new();

    job.extend_from_slice(&build_header(tape, char_count));

    job.extend_from_slice(&build_command_frame(b'C', &opts.cut.c_params()));
    job.extend_from_slice(&build_command_frame(b'D', &CMD_D_PARAMS));
    job.extend_from_slice(&build_command_frame(b'G', &CMD_G_PARAMS));

    for (rows, margin_rows) in labels {
        let encoded_rows: Vec<Vec<u8>> = rows.iter().map(|bitmap| build_row(tape, bitmap)).collect();
        for _ in 0..opts.copies.max(1) {
            job.extend_from_slice(&build_command_frame(b'L', &(rows.len() as u32).to_le_bytes()));
            job.extend_from_slice(&build_command_frame(b'T', &(*margin_rows as u16).to_le_bytes()));
            for row in &encoded_rows {
                job.extend_from_slice(row);
            }
            job.push(TRAILING_BYTE);
        }
    }

    job.extend_from_slice(&build_command_frame(b'@', &[]));

    job
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tepra::raster::{row_len, ROW_HEADER_LEN};
    use crate::tepra::{Model, TapeWidth};

    /// Fixed-size prefix of every job: header block + `C`, `D`, `G`.
    const HEADER_LEN: usize = 2179;
    const CMD_C_LEN: usize = 2 + 1 + 1 + 4 + 1 + 1;
    const CMD_D_LEN: usize = 2 + 1 + 1 + CMD_D_PARAMS.len() + 1 + 1;
    const CMD_G_LEN: usize = 2 + 1 + 1 + CMD_G_PARAMS.len() + 1 + 1;
    const CMD_L_LEN: usize = 2 + 1 + 1 + 4 + 1 + 1;
    const CMD_T_LEN: usize = 2 + 1 + 1 + 2 + 1 + 1;
    const FIRST_L: usize = HEADER_LEN + CMD_C_LEN + CMD_D_LEN + CMD_G_LEN;
    const RASTER_START: usize = FIRST_L + CMD_L_LEN + CMD_T_LEN;

    /// Extracts the first copy's raster rows and margin from a captured
    /// `.prn` file, for use as test input/expected-output pairs. Only the
    /// `L`/`T` parameter values and the row length are assumed.
    fn extract_rows(data: &[u8], tape: Tape) -> (Vec<Vec<u8>>, u32) {
        let row_len = row_len(tape);

        // Find L parameter (row count) directly from the bytes instead of
        // recomputing, to keep the test independent from builder logic.
        let l_params_start = FIRST_L + 4; // ESC { len 'L' -> params start here
        let row_count = u32::from_le_bytes(
            data[l_params_start..l_params_start + 4].try_into().unwrap(),
        );

        let t_params_start = FIRST_L + CMD_L_LEN + 4; // ESC { len 'T' -> params start here
        let margin = u16::from_le_bytes(
            data[t_params_start..t_params_start + 2].try_into().unwrap(),
        ) as u32;

        let mut rows = Vec::new();
        for i in 0..row_count as usize {
            let start = RASTER_START + i * row_len;
            let bitmap = data[start + ROW_HEADER_LEN..start + row_len].to_vec();
            rows.push(bitmap);
        }

        (rows, margin)
    }

    fn sr3700p_18mm() -> Tape {
        Tape::sr3700p(TapeWidth::Mm18)
    }

    #[test]
    fn rebuilds_a_prn_exactly() {
        let expected = include_bytes!("../../tests/fixtures/a.prn").to_vec();
        let (rows, margin) = extract_rows(&expected, sr3700p_18mm());
        let job = build_print_job(sr3700p_18mm(), 1, &rows, margin);
        assert_eq!(job, expected);
    }

    #[test]
    fn rebuilds_b_prn_exactly() {
        let expected = include_bytes!("../../tests/fixtures/b.prn").to_vec();
        let (rows, margin) = extract_rows(&expected, sr3700p_18mm());
        let job = build_print_job(sr3700p_18mm(), 1, &rows, margin);
        assert_eq!(job, expected);
    }

    #[test]
    fn rebuilds_aa_prn_exactly() {
        let expected = include_bytes!("../../tests/fixtures/aa.prn").to_vec();
        let (rows, margin) = extract_rows(&expected, sr3700p_18mm());
        let job = build_print_job(sr3700p_18mm(), 2, &rows, margin);
        assert_eq!(job, expected);
    }

    #[test]
    fn rebuilds_small_a_prn_exactly() {
        let expected = include_bytes!("../../tests/fixtures/small-a.prn").to_vec();
        let (rows, margin) = extract_rows(&expected, sr3700p_18mm());
        let job = build_print_job(sr3700p_18mm(), 1, &rows, margin);
        assert_eq!(job, expected);
    }

    #[test]
    fn rebuilds_a_6mm_prn_exactly() {
        let expected = include_bytes!("../../tests/fixtures/a-6mm.prn").to_vec();
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let (rows, margin) = extract_rows(&expected, tape);
        let job = build_print_job(tape, 1, &rows, margin);
        assert_eq!(job, expected);
    }

    #[test]
    fn rebuilds_a_12mm_prn_exactly() {
        let expected = include_bytes!("../../tests/fixtures/a-12mm.prn").to_vec();
        let tape = Tape::sr3700p(TapeWidth::Mm12);
        let (rows, margin) = extract_rows(&expected, tape);
        let job = build_print_job(tape, 1, &rows, margin);
        assert_eq!(job, expected);
    }

    /// Rebuilds an SR3900P driver capture and compares byte-for-byte,
    /// except for header offset 32 (`00` in most SR3900P captures but
    /// `01` in the two-copy 9mm one and in the SR3700P template; meaning
    /// unknown, kept at the template's `01`) and the header checksum
    /// that covers it. The captured text "TEST 123" sets the char-count
    /// field to `12 + 4 * 5`, so `char_count` is 5.
    fn assert_rebuilds_sr3900p_capture(captured: &[u8], width: TapeWidth, opts: JobOptions) {
        let tape = width.on(Model::Sr3900p);
        let (rows, margin) = extract_rows(captured, tape);
        assert_eq!(rows.len(), 453);
        let job = build_print_job_with(tape, 5, &rows, margin, opts);
        assert_eq!(job.len(), captured.len(), "job length");
        let excluded: &[usize] = &[32, 2177];
        for i in 0..job.len() {
            if excluded.contains(&i) {
                continue;
            }
            assert_eq!(job[i], captured[i], "byte {i} ({width:?}, {:?}, {} copies)", opts.cut, opts.copies);
        }
    }

    #[test]
    fn rebuilds_sr3900p_9mm_captures() {
        assert_rebuilds_sr3900p_capture(
            include_bytes!("../../tests/fixtures/sr3900p-w9-full.prn"),
            TapeWidth::Mm9,
            JobOptions { cut: CutMode::Full, copies: 1 },
        );
        assert_rebuilds_sr3900p_capture(
            include_bytes!("../../tests/fixtures/sr3900p-w9-half.prn"),
            TapeWidth::Mm9,
            JobOptions { cut: CutMode::Half, copies: 1 },
        );
        assert_rebuilds_sr3900p_capture(
            include_bytes!("../../tests/fixtures/sr3900p-w9-none.prn"),
            TapeWidth::Mm9,
            JobOptions { cut: CutMode::NoCut, copies: 1 },
        );
        assert_rebuilds_sr3900p_capture(
            include_bytes!("../../tests/fixtures/sr3900p-w9-full-x2.prn"),
            TapeWidth::Mm9,
            JobOptions { cut: CutMode::Full, copies: 2 },
        );
    }

    #[test]
    fn rebuilds_sr3900p_other_width_captures() {
        for (fixture, width) in [
            (&include_bytes!("../../tests/fixtures/sr3900p-w4-half.prn")[..], TapeWidth::Mm4),
            (&include_bytes!("../../tests/fixtures/sr3900p-w6-half.prn")[..], TapeWidth::Mm6),
            (&include_bytes!("../../tests/fixtures/sr3900p-w12-half.prn")[..], TapeWidth::Mm12),
            (&include_bytes!("../../tests/fixtures/sr3900p-w18-half.prn")[..], TapeWidth::Mm18),
            (&include_bytes!("../../tests/fixtures/sr3900p-w24-half.prn")[..], TapeWidth::Mm24),
        ] {
            assert_rebuilds_sr3900p_capture(fixture, width, JobOptions { cut: CutMode::Half, copies: 1 });
        }
    }

    #[test]
    fn rebuilds_sr3900p_36mm_captures() {
        assert_rebuilds_sr3900p_capture(
            include_bytes!("../../tests/fixtures/sr3900p-w36-full.prn"),
            TapeWidth::Mm36,
            JobOptions { cut: CutMode::Full, copies: 1 },
        );
        assert_rebuilds_sr3900p_capture(
            include_bytes!("../../tests/fixtures/sr3900p-w36-half.prn"),
            TapeWidth::Mm36,
            JobOptions { cut: CutMode::Half, copies: 1 },
        );
        assert_rebuilds_sr3900p_capture(
            include_bytes!("../../tests/fixtures/sr3900p-w36-half-x2.prn"),
            TapeWidth::Mm36,
            JobOptions { cut: CutMode::Half, copies: 2 },
        );
    }

    #[test]
    fn printer_payload_strips_the_header_block() {
        let captured = include_bytes!("../../tests/fixtures/sr3900p-w9-half.prn");
        let payload = printer_payload(captured);
        // Exactly what the SR3900P driver put on the USB bus for this
        // job (capture-kit/usbcap_9mm_half.pcapng): 10014 bytes starting
        // at the C command.
        assert_eq!(payload.len(), 10014);
        assert_eq!(&payload[..10], &[0x1B, 0x7B, 0x07, 0x43, 0x02, 0x02, 0x01, 0x01, 0x49, 0x7D]);
        // Not a job: passed through unchanged.
        assert_eq!(printer_payload(&[1, 2, 3]), &[1, 2, 3]);
    }

    #[test]
    fn multi_label_job_matches_two_copy_layout() {
        // Two identical labels in one job are byte-identical to one label
        // with two copies, i.e. the captured two-copy layout.
        let captured = include_bytes!("../../tests/fixtures/sr3900p-w9-full-x2.prn");
        let tape = TapeWidth::Mm9.on(Model::Sr3900p);
        let (rows, margin) = extract_rows(captured, tape);
        let two_copies = build_print_job_with(tape, 5, &rows, margin, JobOptions { cut: CutMode::Full, copies: 2 });
        let two_labels = build_print_job_multi(tape, 5, &[(&rows, margin), (&rows, margin)], JobOptions::default());
        assert_eq!(two_copies, two_labels);

        // Different labels keep their own L/T values and content.
        let short: Vec<Vec<u8>> = rows[..100].to_vec();
        let job = build_print_job_multi(tape, 5, &[(&rows, margin), (&short, 7)], JobOptions::default());
        let block1 = RASTER_START + rows.len() * row_len(tape) + 1;
        assert_eq!(&job[block1..block1 + 4], &[0x1B, 0x7B, 0x07, b'L']);
        assert_eq!(u32::from_le_bytes(job[block1 + 4..block1 + 8].try_into().unwrap()), 100);
        let t = block1 + CMD_L_LEN + 4;
        assert_eq!(u16::from_le_bytes(job[t..t + 2].try_into().unwrap()), 7);
        assert_eq!(job.len(), block1 + CMD_L_LEN + CMD_T_LEN + 100 * row_len(tape) + 1 + 6);

        // Copies apply per label: 2 labels × 2 copies = 4 blocks.
        let job4 = build_print_job_multi(tape, 5, &[(&rows, margin), (&short, 7)], JobOptions { cut: CutMode::Half, copies: 2 });
        let blocks = job4.windows(4).filter(|w| w == &[0x1B, 0x7B, 0x07, b'L']).count();
        assert_eq!(blocks, 4);
    }

    #[test]
    fn copies_below_one_print_once() {
        let tape = Tape::sr3700p(TapeWidth::Mm6);
        let rows = vec![vec![0u8; tape.bitmap_bytes()]; 4];
        let one = build_print_job(tape, 1, &rows, 0);
        let zero = build_print_job_with(tape, 1, &rows, 0, JobOptions { cut: CutMode::Full, copies: 0 });
        assert_eq!(one, zero);
        let two = build_print_job_with(tape, 1, &rows, 0, JobOptions { cut: CutMode::Full, copies: 2 });
        assert_eq!(two.len(), one.len() + CMD_L_LEN + CMD_T_LEN + 4 * row_len(tape) + 1);
    }

    #[test]
    fn cut_mode_parses_cli_names() {
        assert_eq!("full".parse::<CutMode>().unwrap(), CutMode::Full);
        assert_eq!("Half".parse::<CutMode>().unwrap(), CutMode::Half);
        assert_eq!("none".parse::<CutMode>().unwrap(), CutMode::NoCut);
        assert!("partial".parse::<CutMode>().is_err());
        for mode in [CutMode::Full, CutMode::Half, CutMode::NoCut] {
            assert_eq!(mode.name().parse::<CutMode>().unwrap(), mode);
        }
    }
}

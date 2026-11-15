//! Raster row encoding.
//!
//! Each raster row (one "column" of the printed image, since the print
//! head lays down data perpendicular to the tape feed direction) is
//! encoded as:
//!
//! ```text
//! 0x1B 0x2E <prefix: 4 bytes> <dots: u16 LE> <bitmap: dots/8 bytes>
//! ```
//!
//! `prefix` is `00 0D 0D 01` in every SR3700P driver capture and `00 0A
//! 0A 01` in every SR3900P driver capture ([`crate::tepra::Model::row_prefix`];
//! the middle bytes are probably the dot pitch in 1/3600 inch).
//!
//! `dots` is the number of print head dots used for the tape width on
//! that model ([`crate::tepra::Tape::dots`]): 18mm tape on the SR3700P
//! uses 168 (`A8 00`) with a 21-byte bitmap, 6mm tape 56 (`38 00`) with
//! a 7-byte bitmap, 36mm tape on the SR3900P 384 (`80 01`, the whole
//! head) with a 48-byte bitmap. Earlier revisions of this crate read the
//! field as one byte followed by a bitmap of `dots + 8` bits; the 36mm
//! capture (`80 01`) showed it is a 16-bit value. The byte stream is
//! identical either way for every width below 256 dots.
//!
//! There is no per-row checksum.

use crate::tepra::Tape;

/// Length of the row header: `ESC .` + 4-byte prefix + `u16` dot count.
pub const ROW_HEADER_LEN: usize = 2 + 4 + 2;

/// Build a single raster row from a bitmap.
///
/// `bitmap` must be exactly [`Tape::bitmap_bytes`] bytes long, most
/// significant bit first, representing one column of dots across the tape
/// width.
pub fn build_row(tape: Tape, bitmap: &[u8]) -> Vec<u8> {
    assert_eq!(
        bitmap.len(),
        tape.bitmap_bytes(),
        "bitmap length must match tape width's bit depth"
    );

    let mut row = Vec::with_capacity(ROW_HEADER_LEN + bitmap.len());
    row.push(0x1B);
    row.push(b'.');
    row.extend_from_slice(&tape.model.row_prefix());
    row.extend_from_slice(&tape.dots().to_le_bytes());
    row.extend_from_slice(bitmap);
    row
}

/// Build an empty (all-zero) raster row for the given tape.
pub fn build_blank_row(tape: Tape) -> Vec<u8> {
    build_row(tape, &vec![0u8; tape.bitmap_bytes()])
}

/// Total encoded length of one raster row for `tape`.
pub fn row_len(tape: Tape) -> usize {
    ROW_HEADER_LEN + tape.bitmap_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tepra::{Model, TapeWidth};

    #[test]
    fn blank_row_matches_capture() {
        // First raster row of a.prn is an all-zero 18mm row.
        let expected = include_bytes!("../../tests/fixtures/a.prn");
        let tape = Tape::sr3700p(TapeWidth::Mm18);
        let row = build_blank_row(tape);
        assert_eq!(row.len(), 29);
        assert_eq!(row_len(tape), 29);
        assert_eq!(&row, &expected[2220..2220 + 29]);
    }

    #[test]
    fn nonblank_row_matches_capture() {
        // Row 45 (0-indexed) of a.prn's raster data.
        let expected = include_bytes!("../../tests/fixtures/a.prn");
        let start = 2220 + 45 * 29;
        let bitmap = &expected[start + ROW_HEADER_LEN..start + 29];
        let row = build_row(Tape::sr3700p(TapeWidth::Mm18), bitmap);
        assert_eq!(&row, &expected[start..start + 29]);
    }

    #[test]
    fn row_dot_count_is_16_bit() {
        // 36mm uses all 384 head dots, which no longer fits in one byte.
        let row = build_blank_row(TapeWidth::Mm36.on(Model::Sr3900p));
        assert_eq!(row.len(), 8 + 48);
        assert_eq!(&row[..8], &[0x1B, 0x2E, 0x00, 0x0A, 0x0A, 0x01, 0x80, 0x01]);
        // Widths below 256 dots are byte-identical to the old one-byte
        // reading (`<width_byte> 00 ...`).
        let row = build_blank_row(Tape::sr3700p(TapeWidth::Mm12));
        assert_eq!(&row[..8], &[0x1B, 0x2E, 0x00, 0x0D, 0x0D, 0x01, 0x70, 0x00]);
    }

    #[test]
    fn rows_match_sr3900p_captures_exactly() {
        for (fixture, width) in [
            (&include_bytes!("../../tests/fixtures/sr3900p-w9-full.prn")[..], TapeWidth::Mm9),
            (&include_bytes!("../../tests/fixtures/sr3900p-w36-full.prn")[..], TapeWidth::Mm36),
        ] {
            let tape = width.on(Model::Sr3900p);
            let n = row_len(tape);
            // Row 60 is inside the printed text in both captures.
            for r in [0usize, 60] {
                let captured = &fixture[2220 + r * n..2220 + (r + 1) * n];
                let ours = build_row(tape, &captured[ROW_HEADER_LEN..]);
                assert_eq!(ours, captured, "{width:?} row {r}");
            }
        }
    }
}

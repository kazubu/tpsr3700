//! TEPRA SR3700P / SR3900P proprietary print job encoding.
//!
//! This module re-implements the printer language captured from the
//! Windows drivers' `FILE:`-port output (see `docs/` and `tests/fixtures/`
//! in the repository root for the raw samples and analysis notes).

pub mod builder;
pub mod frame;
pub mod header;
pub mod raster;

/// Printer model. The job format is shared, but the print head differs,
/// and with it the dot pitch, the number of dots each tape width uses,
/// and a few model-identifying bytes in the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    /// TEPRA PRO SR3700P: 270 dpi, 192-dot head (18.1mm). All SR3700P
    /// values come from upstream's captures of the SR3700P Windows
    /// driver (6/12/18mm) and were verified on hardware.
    #[default]
    Sr3700p,
    /// TEPRA PRO SR3900P: 360 dpi, 384-dot head (27.1mm). SR3900P
    /// values come from captures of the SR3900P Windows driver (every
    /// width from 4mm to 36mm; `tests/fixtures/sr3900p-*`).
    Sr3900p,
}

impl Model {
    /// Dot pitch of the print head and of the tape feed, in dots per
    /// inch. Feed-direction dots are square (glyphs in the captures have
    /// normal proportions when one raster row = one head dot).
    pub fn dpi(self) -> f32 {
        match self {
            Model::Sr3700p => 270.0,
            Model::Sr3900p => 360.0,
        }
    }

    /// Number of dots on the print head.
    pub fn head_dots(self) -> u16 {
        match self {
            Model::Sr3700p => 192,
            Model::Sr3900p => 384,
        }
    }

    /// The 4 fixed bytes between `ESC .` and the dot count in every
    /// raster row. The two middle bytes look like the dot pitch in
    /// 1/3600 inch (`3600 / 270 = 13.3 -> 0x0D`, `3600 / 360 = 10 ->
    /// 0x0A`); the first and last byte are constant.
    pub fn row_prefix(self) -> [u8; 4] {
        match self {
            Model::Sr3700p => [0x00, 0x0D, 0x0D, 0x01],
            Model::Sr3900p => [0x00, 0x0A, 0x0A, 0x01],
        }
    }

    /// Value of header byte 9, which differs between the two drivers'
    /// output and nothing else (meaning unknown; possibly a model id).
    pub fn header_byte_9(self) -> u8 {
        match self {
            Model::Sr3700p => 0x0E,
            Model::Sr3900p => 0x68,
        }
    }

    /// The 3-character version-like string stored as UTF-16LE at header
    /// offset 0x61 (`"306"` in SR3700P driver captures, `"500"` in
    /// SR3900P ones).
    pub fn header_version_tag(self) -> &'static str {
        match self {
            Model::Sr3700p => "306",
            Model::Sr3900p => "500",
        }
    }

    /// Name as accepted on the command line (`sr3700p`, `sr3900p`).
    pub fn name(self) -> &'static str {
        match self {
            Model::Sr3700p => "sr3700p",
            Model::Sr3900p => "sr3900p",
        }
    }

    /// Every model.
    pub const ALL: [Model; 2] = [Model::Sr3700p, Model::Sr3900p];
}

impl std::str::FromStr for Model {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('-', "").as_str() {
            "sr3700p" | "3700p" | "3700" => Ok(Model::Sr3700p),
            "sr3900p" | "3900p" | "3900" => Ok(Model::Sr3900p),
            other => Err(format!("unknown printer model '{other}': expected sr3700p or sr3900p")),
        }
    }
}

/// Supported tape widths (independent of the printer model; see
/// [`Tape`] for the model-specific geometry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapeWidth {
    Mm4,
    Mm6,
    Mm9,
    Mm12,
    Mm18,
    Mm24,
    Mm36,
}

impl TapeWidth {
    /// Tape width in millimeters, as encoded in the job header (offset 8).
    pub fn width_mm(self) -> u8 {
        match self {
            TapeWidth::Mm4 => 4,
            TapeWidth::Mm6 => 6,
            TapeWidth::Mm9 => 9,
            TapeWidth::Mm12 => 12,
            TapeWidth::Mm18 => 18,
            TapeWidth::Mm24 => 24,
            TapeWidth::Mm36 => 36,
        }
    }

    /// Secondary tape-width-dependent byte in the job header (offset 23).
    ///
    /// Exact meaning unknown. All values are captured: 6/12/18mm from the
    /// SR3700P driver, 4/6/9/12/18/24/36mm from the SR3900P driver (the
    /// 6/12/18mm values agree, so the byte does not depend on the model).
    /// 6mm..36mm look like an ordinal into KING JIM's tape lineup
    /// (3..=8), but 4mm is 18 (0x12), not 2.
    pub fn aux_byte(self) -> u8 {
        match self {
            TapeWidth::Mm4 => 18,
            TapeWidth::Mm6 => 3,
            TapeWidth::Mm9 => 4,
            TapeWidth::Mm12 => 5,
            TapeWidth::Mm18 => 6,
            TapeWidth::Mm24 => 7,
            TapeWidth::Mm36 => 8,
        }
    }

    /// Default non-printing margin at each edge of the bitmap, in dots,
    /// for library callers that don't pick their own. The CLI uses its
    /// own (smaller) defaults; see `--top-margin` / `--bottom-margin`.
    pub const HEAD_MARGIN_BITS: usize = 8;

    /// Converts a tape width index, as returned by the printer's
    /// "detect tape width" vendor control request (byte 3 of the
    /// response; see [`crate::usb::Printer::get_tape_info_raw`] and
    /// `docs/protocol.md` section 1.5), into a [`TapeWidth`].
    ///
    /// Only indices 1 (6mm), 2 (9mm), 3 (12mm), and 4 (18mm) have been
    /// confirmed against real hardware. Indices 0 (4mm), 5 (24mm) and 6
    /// (36mm) are assumed from KING JIM's official tape width lineup for
    /// this printer line (4/6/9/12/18/24/36/50mm for indices 0..=7).
    /// Index 7 (50mm) is not representable as a [`TapeWidth`] and
    /// returns `None`.
    pub fn from_detected_index(index: u8) -> Option<Self> {
        match index {
            0 => Some(TapeWidth::Mm4),
            1 => Some(TapeWidth::Mm6),
            2 => Some(TapeWidth::Mm9),
            3 => Some(TapeWidth::Mm12),
            4 => Some(TapeWidth::Mm18),
            5 => Some(TapeWidth::Mm24),
            6 => Some(TapeWidth::Mm36),
            _ => None,
        }
    }

    /// Every supported width, narrowest first.
    pub const ALL: [TapeWidth; 7] = [
        TapeWidth::Mm4,
        TapeWidth::Mm6,
        TapeWidth::Mm9,
        TapeWidth::Mm12,
        TapeWidth::Mm18,
        TapeWidth::Mm24,
        TapeWidth::Mm36,
    ];

    /// This width loaded in `model`.
    pub fn on(self, model: Model) -> Tape {
        Tape::new(model, self)
    }
}

/// Where the dot count for a (model, width) pair comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DotsSource {
    /// Taken from a capture of that model's Windows driver.
    Captured,
    /// Extrapolated; no capture for this model/width exists.
    Extrapolated,
}

/// A tape width loaded in a specific printer model: everything the
/// renderer and the job builder need to know about the raster geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tape {
    pub model: Model,
    pub width: TapeWidth,
}

impl Tape {
    pub fn new(model: Model, width: TapeWidth) -> Self {
        Self { model, width }
    }

    /// Shorthand for tests and SR3700P-only callers.
    pub fn sr3700p(width: TapeWidth) -> Self {
        Self::new(Model::Sr3700p, width)
    }

    /// Tape width in millimeters (see [`TapeWidth::width_mm`]).
    pub fn width_mm(self) -> u8 {
        self.width.width_mm()
    }

    /// Header aux byte (see [`TapeWidth::aux_byte`]).
    pub fn aux_byte(self) -> u8 {
        self.width.aux_byte()
    }

    /// Dot pitch in dots per inch (see [`Model::dpi`]).
    pub fn dpi(self) -> f32 {
        self.model.dpi()
    }

    /// Number of print head dots used for this tape width on this model,
    /// as encoded (little-endian `u16`) in every raster row header, and
    /// where that number comes from. The bitmap that follows the row
    /// header is exactly `dots / 8` bytes long.
    ///
    /// SR3900P (360 dpi), all captured from the SR3900P driver: 4mm -> 48,
    /// 6mm -> 72, 9mm -> 112, 12mm -> 144, 18mm -> 216, 24mm -> 288,
    /// 36mm -> 384. That is `width_mm * 12` rounded up to a multiple of
    /// 8 and capped at the 384-dot head (9mm: 108 -> 112; 36mm: 432 ->
    /// 384).
    ///
    /// SR3700P (270 dpi): 6mm -> 56, 12mm -> 112, 18mm -> 168 captured
    /// (`width_mm * 28 / 3`). The other widths apply the same formula,
    /// rounded up to a multiple of 8 and capped at the 192-dot head:
    /// 4mm -> 40, 9mm -> 88, 24mm -> 192, 36mm -> 192. These are
    /// extrapolated (upstream's 224 for 24mm can't fit on the head).
    pub fn dots_with_source(self) -> (u16, DotsSource) {
        use DotsSource::*;
        match (self.model, self.width) {
            (Model::Sr3700p, TapeWidth::Mm4) => (40, Extrapolated),
            (Model::Sr3700p, TapeWidth::Mm6) => (56, Captured),
            (Model::Sr3700p, TapeWidth::Mm9) => (88, Extrapolated),
            (Model::Sr3700p, TapeWidth::Mm12) => (112, Captured),
            (Model::Sr3700p, TapeWidth::Mm18) => (168, Captured),
            (Model::Sr3700p, TapeWidth::Mm24) => (192, Extrapolated),
            (Model::Sr3700p, TapeWidth::Mm36) => (192, Extrapolated),
            (Model::Sr3900p, TapeWidth::Mm4) => (48, Captured),
            (Model::Sr3900p, TapeWidth::Mm6) => (72, Captured),
            (Model::Sr3900p, TapeWidth::Mm9) => (112, Captured),
            (Model::Sr3900p, TapeWidth::Mm12) => (144, Captured),
            (Model::Sr3900p, TapeWidth::Mm18) => (216, Captured),
            (Model::Sr3900p, TapeWidth::Mm24) => (288, Captured),
            (Model::Sr3900p, TapeWidth::Mm36) => (384, Captured),
        }
    }

    /// Number of print head dots used (see [`Self::dots_with_source`]).
    pub fn dots(self) -> u16 {
        self.dots_with_source().0
    }

    /// Whether [`Self::dots`] is backed by a capture of this model's
    /// driver.
    pub fn dots_source(self) -> DotsSource {
        self.dots_with_source().1
    }

    /// Number of bitmap bytes per raster row (`dots / 8`; every known
    /// dot count is a multiple of 8).
    pub fn bitmap_bytes(self) -> usize {
        (self.bitmap_bits() + 7) / 8
    }

    /// Number of dots (bits) per raster row bitmap. Same as [`Self::dots`]
    /// but as a `usize` for indexing.
    pub fn bitmap_bits(self) -> usize {
        self.dots() as usize
    }

    /// Number of dots per raster row that the renderer prints to by
    /// default: all dots except [`TapeWidth::HEAD_MARGIN_BITS`] at each
    /// edge.
    pub fn head_bits(self) -> usize {
        self.bitmap_bits() - 2 * TapeWidth::HEAD_MARGIN_BITS
    }

    /// Bit offset within the bitmap row at which the default printable
    /// area starts.
    pub fn head_offset(self) -> usize {
        TapeWidth::HEAD_MARGIN_BITS
    }

    /// Converts a length in millimeters to a number of raster rows (feed
    /// direction dots) at this model's dot pitch.
    pub fn mm_to_rows(self, mm: f32) -> u32 {
        (mm / 25.4 * self.dpi()).round() as u32
    }

    /// Converts a number of raster rows (feed direction dots) to
    /// millimeters at this model's dot pitch.
    pub fn rows_to_mm(self, rows: u32) -> f32 {
        rows as f32 * 25.4 / self.dpi()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_match_captures() {
        // SR3700P driver captures (tests/fixtures/a-6mm.prn, a-12mm.prn,
        // a.prn) and SR3900P driver captures (sr3900p-w9-*.prn,
        // sr3900p-w36-*.prn).
        assert_eq!(TapeWidth::Mm6.on(Model::Sr3700p).dots_with_source(), (56, DotsSource::Captured));
        assert_eq!(TapeWidth::Mm12.on(Model::Sr3700p).dots_with_source(), (112, DotsSource::Captured));
        assert_eq!(TapeWidth::Mm18.on(Model::Sr3700p).dots_with_source(), (168, DotsSource::Captured));
        for (width, dots) in [(TapeWidth::Mm4, 48), (TapeWidth::Mm6, 72), (TapeWidth::Mm9, 112), (TapeWidth::Mm12, 144),
                              (TapeWidth::Mm18, 216), (TapeWidth::Mm24, 288), (TapeWidth::Mm36, 384)] {
            assert_eq!(width.on(Model::Sr3900p).dots_with_source(), (dots, DotsSource::Captured), "{width:?}");
            // width * 12, rounded up to a multiple of 8, capped at the head.
            let predicted = ((width.width_mm() as u16 * 12 + 7) / 8 * 8).min(384);
            assert_eq!(dots, predicted, "{width:?}");
        }
        for model in Model::ALL {
            for width in TapeWidth::ALL {
                let tape = width.on(model);
                assert!(tape.dots() <= model.head_dots(), "{tape:?} exceeds the head");
                assert_eq!(tape.bitmap_bits() % 8, 0, "{tape:?}");
                assert_eq!(tape.bitmap_bytes() * 8, tape.bitmap_bits(), "{tape:?}");
            }
        }
        assert_eq!(TapeWidth::Mm24.on(Model::Sr3700p).dots_source(), DotsSource::Extrapolated);
    }

    #[test]
    fn bitmap_bytes_matches_captures() {
        assert_eq!(Tape::sr3700p(TapeWidth::Mm6).bitmap_bytes(), 7);
        assert_eq!(Tape::sr3700p(TapeWidth::Mm12).bitmap_bytes(), 14);
        assert_eq!(Tape::sr3700p(TapeWidth::Mm18).bitmap_bytes(), 21);
        assert_eq!(TapeWidth::Mm4.on(Model::Sr3900p).bitmap_bytes(), 6);
        assert_eq!(TapeWidth::Mm9.on(Model::Sr3900p).bitmap_bytes(), 14);
        assert_eq!(TapeWidth::Mm24.on(Model::Sr3900p).bitmap_bytes(), 36);
        assert_eq!(TapeWidth::Mm36.on(Model::Sr3900p).bitmap_bytes(), 48);
    }

    #[test]
    fn aux_byte_matches_captures() {
        assert_eq!(TapeWidth::Mm4.aux_byte(), 0x12);
        assert_eq!(TapeWidth::Mm6.aux_byte(), 3);
        assert_eq!(TapeWidth::Mm9.aux_byte(), 4);
        assert_eq!(TapeWidth::Mm12.aux_byte(), 5);
        assert_eq!(TapeWidth::Mm18.aux_byte(), 6);
        assert_eq!(TapeWidth::Mm24.aux_byte(), 7);
        assert_eq!(TapeWidth::Mm36.aux_byte(), 8);
    }

    #[test]
    fn model_geometry() {
        assert_eq!(Model::Sr3700p.dpi(), 270.0);
        assert_eq!(Model::Sr3900p.dpi(), 360.0);
        assert_eq!(Model::Sr3700p.head_dots(), 192);
        assert_eq!(Model::Sr3900p.head_dots(), 384);
        // 10mm of feed.
        assert_eq!(Tape::sr3700p(TapeWidth::Mm18).mm_to_rows(10.0), 106);
        assert_eq!(TapeWidth::Mm18.on(Model::Sr3900p).mm_to_rows(10.0), 142);
        assert!((TapeWidth::Mm18.on(Model::Sr3900p).rows_to_mm(142) - 10.0).abs() < 0.05);
        assert_eq!("SR3900P".parse::<Model>().unwrap(), Model::Sr3900p);
        assert_eq!("sr-3700p".parse::<Model>().unwrap(), Model::Sr3700p);
        assert!("sr5900p".parse::<Model>().is_err());
    }

    #[test]
    fn from_detected_index_matches_hardware() {
        // Confirmed against real hardware with 6mm/12mm/18mm tapes loaded
        // (see docs/protocol.md section 1.5).
        assert_eq!(TapeWidth::from_detected_index(1), Some(TapeWidth::Mm6));
        assert_eq!(TapeWidth::from_detected_index(3), Some(TapeWidth::Mm12));
        assert_eq!(TapeWidth::from_detected_index(4), Some(TapeWidth::Mm18));
    }

    #[test]
    fn from_detected_index_maps_lineup_widths() {
        assert_eq!(TapeWidth::from_detected_index(0), Some(TapeWidth::Mm4));
        assert_eq!(TapeWidth::from_detected_index(2), Some(TapeWidth::Mm9));
        assert_eq!(TapeWidth::from_detected_index(5), Some(TapeWidth::Mm24));
        assert_eq!(TapeWidth::from_detected_index(6), Some(TapeWidth::Mm36));
    }

    #[test]
    fn from_detected_index_rejects_unsupported_widths() {
        // Index 7 (50mm) is not implemented.
        for index in [7, 8, 255] {
            assert_eq!(TapeWidth::from_detected_index(index), None);
        }
    }
}

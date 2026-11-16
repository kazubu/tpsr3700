use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use tpsr3700::font::{FontRenderer, TextAlign, DEFAULT_FONT_BYTES, DEFAULT_LEFT_MARGIN_MM, DEFAULT_RIGHT_MARGIN_MM};
use tpsr3700::font::VerticalMode;
use tpsr3700::usb::JobOutcome;
use tpsr3700::tepra::builder::{build_print_job_multi, printer_payload, CutMode, JobOptions};
use tpsr3700::tepra::raster::ROW_HEADER_LEN;
use tpsr3700::tepra::{DotsSource, Model, Tape, TapeWidth};

/// Parse `job` and print a human-readable summary plus an ASCII-art
/// rendering of the raster rows to stdout.
fn dump_explain(job: &[u8], tape: Tape, top_margin: usize, bottom_margin: usize, opts: JobOptions) {
    let bitmap_bytes = tape.bitmap_bytes();
    let bitmap_bits = tape.bitmap_bits();
    let head_offset = bottom_margin;
    let head_bits = tape.bitmap_bits().saturating_sub(top_margin + bottom_margin).max(1);

    // ---- Header block (fixed 2179 bytes) ----
    let tape_mm_byte = job[8];
    println!("=== TEPRA job dump ({} bytes) ===", job.len());
    println!();
    println!("[Header block]  bytes 0-2178 (2179 bytes, fixed)");
    println!("  model                      : {} ({} dpi, {}-dot head)", tape.model.name(), tape.model.dpi(), tape.model.head_dots());
    println!("  tape width byte (offset 8) : 0x{:02X} = {}mm", tape_mm_byte, tape_mm_byte);
    println!("  aux byte (offset 23)       : 0x{:02X}", job[23]);
    println!("  model byte (offset 9)      : 0x{:02X}", job[9]);
    println!("  cut mode (C params)        : {} {:02X?}", opts.cut.name(), opts.cut.c_params());
    println!("  copies                     : {} (first copy shown below)", opts.copies.max(1));

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
    let row_len = ROW_HEADER_LEN + bitmap_bytes; // ESC . 00 0D 0D 01 <dots u16 LE> <bitmap>
    let raster_start = HEADER_LEN + cmd_c_len + cmd_d_len + cmd_g_len + cmd_l_len + cmd_t_len;

    println!();
    println!(
        "[Raster rows]  {} rows × {} bitmap bytes ({} dots{}, printable: bits {}..{})",
        row_count,
        bitmap_bytes,
        bitmap_bits,
        match tape.dots_source() {
            DotsSource::Captured => "",
            DotsSource::Extrapolated => ", extrapolated",
        },
        head_offset,
        head_offset + head_bits - 1,
    );

    // Collect bitmaps.
    let mut bitmaps: Vec<&[u8]> = Vec::new();
    for i in 0..row_count as usize {
        let start = raster_start + i * row_len;
        bitmaps.push(&job[start + ROW_HEADER_LEN..start + ROW_HEADER_LEN + bitmap_bytes]);
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

    /// Print several labels as one job: a file containing the labels
    /// separated by ASCII RS (0x1E) characters (newlines inside a label
    /// are kept). TEXT must be omitted. Every label is rendered with
    /// the same settings; --copies applies to each label and --cut
    /// between all of them, so half cut gives one strip of labels.
    #[arg(long, value_name = "PATH", conflicts_with = "text")]
    labels_file: Option<PathBuf>,

    /// Feed this many millimetres of blank tape instead of printing
    /// text (cut according to --cut). This is what the official
    /// driver's "tape feed" / "feed and cut" do: a job of blank raster
    /// rows. TEXT must be omitted.
    #[arg(long, value_name = "MM", conflicts_with_all = ["text", "labels_file"])]
    feed_mm: Option<f32>,

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

    /// Printer model: sr3700p (270 dpi, 192-dot head; the default) or
    /// sr3900p (360 dpi, 384-dot head). Selects the dot pitch used for
    /// every millimetre conversion, the number of head dots each tape
    /// width uses, and a few model-identifying bytes in the job.
    /// Defaults to the TEPRA_MODEL env var when set, otherwise to the
    /// model named in the printer's USB device ID when the printer is
    /// opened, otherwise sr3700p.
    #[arg(long)]
    model: Option<Model>,

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
    /// Output: {"tape_mm": 18, "index": 4, "supported": true,
    /// "device_id": "...", "model": "sr3900p"}
    /// `index` is the raw tape width index reported by the printer
    /// (see docs/protocol.md section 1.5). `supported` is false when the
    /// loaded width isn't supported by this driver (50mm); in
    /// that case `tape_mm` is the nominal width for the index, or null
    /// if the index itself is unknown. `device_id` is the USB printer
    /// class device ID string and `model` is looked up from the model
    /// name in it (null when unknown). If no printer is found, an error
    /// naming the vendor/product IDs that were searched for is printed
    /// to stderr and the exit status is non-zero.
    #[arg(long)]
    detect_tape: bool,

    /// Lay the label out sideways, for tape applied vertically:
    /// `stacked` is 縦書き proper (one character after another along the
    /// tape, each turned a quarter turn so it stands upright), and
    /// `rotated` turns the whole label a quarter turn (a line runs across
    /// the tape, the next line follows along the feed). The tape width
    /// limits a character in stacked mode and a whole line in rotated
    /// mode. Cannot be combined with --line-ratios or
    /// --max-tape-length-mm.
    #[arg(long, value_name = "MODE", conflicts_with_all = ["line_ratios", "max_tape_length_mm"])]
    vertical: Option<VerticalMode>,

    /// Stop whatever the printer is doing and exit: issue the USB
    /// printer-class SOFT_RESET, which the printer takes as "drop this
    /// job". Used to cancel a long multi-label job that has already been
    /// sent; the tape is left uncut wherever it stopped.
    #[arg(long)]
    abort: bool,

    /// Do not send the job a second time when the printer drops the
    /// first one with a transient fault. Retrying is safe because a
    /// dropped job prints nothing, but a fault raised part way through a
    /// label would leave that part on the tape before the retry.
    #[arg(long)]
    no_retry: bool,

    /// Do not refuse to print when the printer reports a fault in its
    /// status. The fault bits are what stop a job from being reported as
    /// printed when the printer actually dropped it, so skip this only
    /// to get a job out of a printer whose status looks wrong.
    #[arg(long)]
    no_status_check: bool,

    /// Do not check the loaded tape against --tape-mm before printing.
    /// The check is what stops a job from being printed onto the wrong
    /// tape; skip it only when the printer reports a width this driver
    /// cannot read.
    #[arg(long)]
    no_tape_check: bool,

    /// Print the printer's current state and exit, without printing:
    /// the raw 8-byte vendor status (activity byte and tape index) and
    /// the USB printer-class port status byte. Diagnostic only.
    #[arg(long)]
    printer_status: bool,


    /// Tape width in millimeters (4, 6, 9, 12, 18, 24, 36).
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
    /// With --max-tape-length-mm the label is exactly that long and each
    /// line is aligned (--align) inside it; the text is never wrapped,
    /// so it is an error if the widest line does not fit between the
    /// margins.
    #[arg(long)]
    line_ratios: Option<String>,

    /// Blank tape feed margin (in millimeters) added before the printed
    /// content (left side when the label exits the printer).
    #[arg(long, default_value_t = DEFAULT_LEFT_MARGIN_MM)]
    left_margin_mm: f32,

    /// Blank tape feed margin (in millimeters) added after the printed
    /// content (right side).
    #[arg(long, default_value_t = DEFAULT_RIGHT_MARGIN_MM)]
    right_margin_mm: f32,

    /// Non-printing margin at the top edge of the tape, in dots (360 dpi).
    #[arg(long, default_value_t = 2)]
    top_margin: usize,

    /// Non-printing margin at the bottom edge of the tape, in dots (360 dpi).
    #[arg(long, default_value_t = 0)]
    bottom_margin: usize,

    /// How to cut the tape: full (cut through, the default), half (cut
    /// the tape but not the backing paper), or none.
    #[arg(long, default_value = "full")]
    cut: CutMode,

    /// Lay the text out for a cable-marking label: restrict printing to
    /// the printable band at the top of the tape (24mm: top 7.5mm, 36mm:
    /// top 12.5mm — half a millimetre under the nominal 8mm/13mm, which
    /// overflows in practice), leaving the rest clear to wrap around the
    /// cable. Only 24mm and 36mm tape have a cable-marking variant.
    #[arg(long)]
    cable: bool,

    /// Height of the cable label's printable area in millimetres,
    /// overriding the default for the tape width. Only with --cable.
    #[arg(long, value_name = "MM", requires = "cable")]
    cable_mm: Option<f32>,

    /// Number of identical labels to print in this job (1-99). Copies
    /// are sent as one job and cut apart according to --cut.
    #[arg(long, default_value_t = 1)]
    copies: u32,

    /// Also send the 2179-byte header block to the printer. The Windows
    /// driver does not (it only writes it to FILE: ports), and sending it
    /// makes the SR3900P wait ~20s before printing; this is only for
    /// experiments.
    #[arg(long)]
    send_header: bool,

    /// Skip the USB printer-class SOFT_RESET that is normally issued
    /// right before the job (the Windows driver always issues it).
    #[arg(long)]
    no_soft_reset: bool,

    /// Wait for the printer around the job: before sending, wait until
    /// it is idle, so the job is not sent into a printer that is still
    /// printing or cutting (which can drop it); after sending, poll the
    /// status until it is idle again, and fail if the printer never
    /// reported itself busy, because then it did not take the job.
    /// Status changes are printed to stderr.
    #[arg(long)]
    wait: bool,

    /// Total label length in millimeters, including left and right feed
    /// margins.  When the text does not fit on one line at the chosen font
    /// size, it is wrapped character-by-character onto additional lines
    /// (up to the maximum allowed by the tape width; minimum line height
    /// is 3 mm, so a 6 mm tape allows at most 2 lines).  Omitting this
    /// option disables wrapping and prints everything on a single line.
    #[arg(long)]
    max_tape_length_mm: Option<f32>,

    /// Horizontal alignment of each line: within the label length with
    /// --max-tape-length-mm, otherwise within the widest line of a
    /// multi-line text (no effect on single-line text).
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
    /// padding, clamped to the printable band; --frame-mm sets its
    /// width along the label instead.
    #[arg(long)]
    frame: bool,

    /// Frame line thickness in dots (1-3). Only meaningful together
    /// with --frame.
    #[arg(long, default_value_t = 2)]
    frame_width: usize,

    /// Width of the frame in millimetres along the feed direction (the
    /// label's horizontal direction). Without it the frame hugs the
    /// text with about 1mm of padding; with it the frame is that wide
    /// and centred on the label, so labels of the same length get an
    /// identical box however long their text is. Clamped to the label's
    /// length. Only with --frame.
    #[arg(long, requires = "frame")]
    frame_mm: Option<f32>,

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

/// Nominal tape width in mm for a detected tape width index
/// (docs/protocol.md section 1.5). `None` for index 0 (no tape) and for
/// indices whose meaning is unknown.
fn nominal_mm_for_detected_index(index: u8) -> Option<u8> {
    TapeWidth::from_detected_index(index).map(|w| w.width_mm())
}

/// Formats the `--detect-tape` result as one line of JSON:
/// `{"tape_mm": 18, "index": 4, "supported": true}`. For unsupported
/// widths `tape_mm` falls back to the nominal width for the index
/// (or `null` when the index is unknown).
#[cfg(test)]
fn detect_tape_json(detected: Option<TapeWidth>, index: u8) -> String {
    detect_json(detected, index, None)
}

/// Minimal JSON string escaping for the device ID field.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `--detect-tape` output with the printer identification: the USB
/// device ID string and the driver model looked up from the model name
/// in it (null when unknown).
/// Compares the tape the printer reports with the width the job is laid
/// out for.
///
/// `Err` means do not print: the wrong tape is loaded, or none is.
/// `Ok(Some(..))` is a warning to show and carry on with, used when the
/// printer reports an index this driver cannot map to a width. `want` is
/// the width from `--tape-mm`.
fn check_loaded_tape(loaded: Option<TapeWidth>, index: u8, want: TapeWidth) -> Result<Option<String>> {
    if index == 0 {
        anyhow::bail!(
            "no tape is loaded in the printer, but --tape-mm {}mm was given; \
             load the tape and try again",
            want.width_mm()
        );
    }
    match loaded {
        Some(loaded) if loaded != want => anyhow::bail!(
            "the printer has {}mm tape loaded, but this job is laid out for {}mm; \
             load {}mm tape, or print with --tape-mm {} (pass --no-tape-check to \
             print anyway)",
            loaded.width_mm(),
            want.width_mm(),
            want.width_mm(),
            loaded.width_mm()
        ),
        Some(_) => Ok(None),
        None => Ok(Some(format!(
            "the printer reports tape width index {index}, which this driver does not know, \
             so --tape-mm {}mm could not be checked",
            want.width_mm()
        ))),
    }
}

fn detect_json(detected: Option<TapeWidth>, index: u8, device_id: Option<&str>) -> String {
    let supported = detected.is_some();
    let tape_mm = detected
        .map(|t| t.width_mm())
        .or_else(|| nominal_mm_for_detected_index(index));
    let tape_mm_json = tape_mm
        .map(|v| v.to_string())
        .unwrap_or_else(|| "null".to_string());
    let (id_json, model_json) = match device_id {
        Some(id) => (
            json_string(id),
            Model::from_device_name(id).map(|m| format!("\"{}\"", m.name())).unwrap_or_else(|| "null".into()),
        ),
        None => ("null".into(), "null".into()),
    };
    format!(
        "{{\"tape_mm\": {tape_mm_json}, \"index\": {index}, \"supported\": {supported}, \
         \"device_id\": {id_json}, \"model\": {model_json}}}"
    )
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

/// Rust starts programs with SIGPIPE ignored, so writing to a closed
/// stdout returns EPIPE and `println!` panics. A command-line tool piped
/// into `head` should just die quietly instead, which is what the
/// default disposition does.
#[cfg(unix)]
fn restore_sigpipe() {
    // SAFETY: setting a signal disposition to the system default before
    // any other thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

fn main() -> Result<()> {
    restore_sigpipe();
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
        let device_id = printer.get_device_id().ok();
        println!("{}", detect_json(detected, index, device_id.as_deref()));
        return Ok(());
    }

    // --abort: tell the printer to drop the job it is working on.
    if args.abort {
        let printer = open_printer(&args)?;
        printer.soft_reset()?;
        let st = printer.get_status()?;
        println!("abort sent; status {:02x?} idle={}", st.raw, st.is_idle());
        return Ok(());
    }

    // --printer-status: dump what the printer reports about itself and
    // exit. Used to tell "the job was refused" apart from "the job was
    // never sent" when a print silently produces nothing.
    if args.printer_status {
        let printer = open_printer(&args)?;
        let st = printer.get_status()?;
        println!(
            "status {:02x?} activity={:#04x} idle={} tape_index={} fault={}",
            st.raw,
            st.activity(),
            st.is_idle(),
            st.tape_index(),
            st.has_error()
        );
        if st.has_error() {
            println!("fault: {}", st.fault_summary());
        }
        match printer.get_port_status() {
            Ok(ps) => println!("port_status {ps:#04x}"),
            Err(e) => println!("port_status unavailable: {e}"),
        }
        return Ok(());
    }

    let labels: Vec<String> = match &args.labels_file {
        Some(_) if args.feed_mm.is_some() => unreachable!("clap rejects --feed-mm with --labels-file"),
        _ if args.feed_mm.is_some() => vec![String::new()],
        Some(path) => {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read --labels-file {}", path.display()))?;
            let labels: Vec<String> = content
                .split('\x1e')
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty())
                .collect();
            if labels.is_empty() {
                anyhow::bail!("--labels-file {} contains no labels", path.display());
            }
            labels
        }
        None => match &args.text {
            Some(t) => vec![t.replace("\\n", "\n")],
            None => anyhow::bail!("nothing to print: pass TEXT or --labels-file"),
        },
    };
    // The first label stands in for the job in previews and dumps.
    let text = labels[0].clone();

    // Open the printer early if we might need it, either for tape width
    // auto-detection or for actually sending the job, so both can share
    // the same connection.
    let mut printer = if !no_print || args.tape_mm.is_none() {
        Some(open_printer(&args)?)
    } else {
        None
    };

    let model = match &args.model {
        Some(m) => *m,
        None => match std::env::var("TEPRA_MODEL") {
            Ok(s) if !s.trim().is_empty() => s
                .parse::<Model>()
                .map_err(|e| anyhow::anyhow!("TEPRA_MODEL: {e}"))?,
            // No explicit model: ask the printer who it is, if we have it open.
            _ => match printer.as_ref().and_then(|p| p.get_device_id().ok()) {
                Some(id) => match Model::from_device_name(&id) {
                    Some(m) => {
                        eprintln!("printer identifies as {}; using --model {}", id.trim(), m.name());
                        m
                    }
                    None => {
                        eprintln!(
                            "warning: unknown printer '{}'; using --model {} (pass --model to override)",
                            id.trim(),
                            Model::default().name()
                        );
                        Model::default()
                    }
                },
                None => Model::default(),
            },
        },
    };

    let width = match args.tape_mm {
        Some(4) => TapeWidth::Mm4,
        Some(6) => TapeWidth::Mm6,
        Some(9) => TapeWidth::Mm9,
        Some(12) => TapeWidth::Mm12,
        Some(18) => TapeWidth::Mm18,
        Some(24) => TapeWidth::Mm24,
        Some(36) => TapeWidth::Mm36,
        Some(other) => {
            anyhow::bail!("unsupported tape width: {other}mm (only 4, 6, 9, 12, 18, 24, and 36 are supported)")
        }
        None => {
            let printer = printer
                .as_ref()
                .expect("printer connection opened above when --tape-mm is omitted");
            let (detected, index) = printer.get_tape_width_with_index()?;
            match detected {
                Some(width) => {
                    eprintln!("detected {}mm tape loaded in printer", width.width_mm());
                    width
                }
                None => anyhow::bail!(
                    "detected tape width index {index} from the printer, but this width \
                     isn't supported (only 4, 6, 9, 12, 18, 24, and 36mm are); pass --tape-mm explicitly \
                     if you know the width, or see docs/protocol.md section 1.5 for the index \
                     mapping"
                ),
            }
        }
    };

    // An explicit --tape-mm is a promise about what is in the printer, and
    // nothing checked it: a job for the wrong width prints anyway and the
    // tape is wasted. Compare it with what the printer reports, whenever
    // the printer is open (previews and --no-print never open it).
    //
    // Blind spot: cable-marking tape reports the same index as plain tape
    // of the same width, so mixing those two up is not detectable here.
    if args.tape_mm.is_some() && !args.no_tape_check {
        if let Some(printer) = printer.as_ref() {
            match printer.get_tape_width_with_index() {
                Ok((loaded, index)) => {
                    if let Some(warning) = check_loaded_tape(loaded, index, width)? {
                        eprintln!("warning: {warning}");
                    }
                }
                Err(e) => eprintln!("warning: could not check the loaded tape: {e}"),
            }
        }
    }

    let tape = width.on(model);
    if tape.dots_source() == DotsSource::Extrapolated {
        eprintln!(
            "warning: {}mm tape on the {} has not been captured from the official driver; \
             the raster geometry ({} dots) is extrapolated. Capture w{}_full.prn with the {} \
             driver (see capture-kit/README.md) to confirm it.",
            tape.width_mm(),
            tape.model.name(),
            tape.dots(),
            tape.width_mm(),
            tape.model.name()
        );
    }

    let renderer = match &args.font {
        Some(path) => FontRenderer::load(path)?,
        None => FontRenderer::from_bytes(DEFAULT_FONT_BYTES)
            .map_err(|e| anyhow::anyhow!("bundled font failed to load: {e}"))?,
    };

    let line_ratios = match &args.line_ratios {
        Some(s) => Some(parse_line_ratios(s)?),
        None => None,
    };
    if !(1..=3).contains(&args.frame_width) {
        anyhow::bail!("--frame-width must be between 1 and 3, got {}", args.frame_width);
    }
    if !(1..=99).contains(&args.copies) {
        anyhow::bail!("--copies must be between 1 and 99, got {}", args.copies);
    }
    // Cable labels print only in a band at the top of the tape, which is
    // expressed as a larger bottom margin (see effective_margins).
    let (top_margin, bottom_margin) = effective_margins(&args, tape)?;

    let rendered: Vec<(Vec<Vec<u8>>, u32)> = match args.feed_mm {
        Some(mm) => {
            if !(1.0..=500.0).contains(&mm) {
                anyhow::bail!("--feed-mm must be between 1 and 500, got {mm}");
            }
            // A feed is a job of `mm` blank rows. When cutting, the printer
            // also ejects the fixed head-to-cutter lead (~9mm on an SR750)
            // as a separate leading scrap, so "feed and cut" always
            // produces that scrap plus the fed piece — there is no way to
            // suppress it in the job. Feed exactly `mm` so the fed piece
            // matches; to avoid the scrap entirely, feed without cutting.
            let rows = tape.mm_to_rows(mm).max(1) as usize;
            eprintln!("feeding {mm}mm of blank tape ({rows} rows), cut: {}", args.cut.name());
            vec![(vec![vec![0u8; tape.bitmap_bytes()]; rows], 0)]
        }
        None => labels
            .iter()
            .map(|t| render_label(&renderer, &args, tape, line_ratios.as_deref(), t, top_margin, bottom_margin))
            .collect::<Result<_>>()?,
    };
    let (rows, margin) = (&rendered[0].0, rendered[0].1);

    let job_opts = JobOptions { cut: args.cut, copies: args.copies };
    let char_count = text.chars().count() as u32;
    let label_refs: Vec<(&[Vec<u8>], u32)> = rendered.iter().map(|(r, m)| (r.as_slice(), *m)).collect();
    let job = build_print_job_multi(tape, char_count, &label_refs, job_opts);
    if labels.len() > 1 {
        eprintln!("{} labels in one job ({} bytes)", labels.len(), job.len());
    }

    if let Some(path) = &args.dump {
        std::fs::write(path, &job)?;
        eprintln!("wrote {} bytes to {}", job.len(), path.display());
        dump_explain(&job, tape, top_margin, bottom_margin, job_opts);
    }

    // --preview-out: write the SVG preview (rendered from the exact
    // raster rows that would be printed) and exit. Never prompts and
    // never sends the job to the printer. With several labels only the
    // first one is previewed.
    if let Some(path) = &args.preview_out {
        #[cfg(feature = "preview")]
        {
            let right_margin_rows = tape.mm_to_rows(args.right_margin_mm);
            let svg = tpsr3700::preview::build_svg_preview(
                rows,
                tape,
                &text,
                top_margin,
                bottom_margin,
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
            let right_margin_rows = tape.mm_to_rows(args.right_margin_mm);
            match tpsr3700::preview::show_preview(
                rows,
                tape,
                &text,
                top_margin,
                bottom_margin,
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
        let payload = if args.send_header { &job[..] } else { printer_payload(&job) };
        let wait_timeout = std::time::Duration::from_secs(180 + 30 * labels.len() as u64);
        // The first print after the printer has been sitting idle
        // sometimes comes back as a fault with nothing printed (see
        // docs/protocol.md section 1.5). Sending it again has worked
        // every time, and a dropped job leaves no tape behind, so try
        // once more rather than making the operator do it.
        let attempts = if args.no_retry { 1 } else { 2 };
        for attempt in 1..=attempts {
            // Same sequence as the SR3900P Windows driver
            // (docs/protocol.md section 1.6): SOFT_RESET, bulk-write the
            // job without the header block, then poll the status until
            // idle.
            //
            // With one addition at the front: wait until the printer is
            // idle. A job sent while the previous one is still printing
            // or cutting can be dropped, and the SOFT_RESET that
            // precedes it would abort that previous job -- either way a
            // label goes missing while both jobs report success.
            let t0 = std::time::Instant::now();
            if args.wait {
                printer.wait_until_ready(
                    std::time::Duration::from_secs(120),
                    !args.no_status_check,
                    |st| eprintln!("[{:.1}s] waiting for the printer, status {:02x?}", t0.elapsed().as_secs_f32(), st.raw),
                )?;
            }
            match printer.get_port_status() {
                Ok(ps) => eprintln!("port status {ps:#04x}"),
                Err(e) => eprintln!("warning: could not read the port status: {e}"),
            }
            if !args.no_soft_reset {
                printer.soft_reset()?;
            }
            // The printer takes the bytes only as fast as it prints them
            // once its buffer is full, so for a long job these are also
            // progress reports. The web front end parses them to move its
            // progress bar.
            let mut reported = 0usize;
            printer.send_job_with_progress(payload, |sent, total| {
                if sent == total || sent - reported >= 64 * 1024 {
                    reported = sent;
                    eprintln!("progress sent={sent} total={total}");
                }
            })?;
            eprintln!(
                "sent job to printer ({} bytes{}) in {:.2}s{}",
                payload.len(),
                if args.send_header { ", including header" } else { "" },
                t0.elapsed().as_secs_f32(),
                if attempt > 1 { format!(" (attempt {attempt})") } else { String::new() }
            );
            if !args.wait {
                break;
            }
            let outcome = printer.wait_until_idle(
                wait_timeout,
                std::time::Duration::from_secs(10),
                !args.no_status_check,
                |st| eprintln!("[{:.1}s] status {:02x?}", t0.elapsed().as_secs_f32(), st.raw),
            )?;
            match outcome {
                JobOutcome::Printed(st) => {
                    eprintln!("printer idle after {:.1}s (status {:02x?})", t0.elapsed().as_secs_f32(), st.raw);
                    break;
                }
                JobOutcome::NeverStarted(st) => anyhow::bail!(
                    "the printer never reported itself busy after the job was sent \
                     (status {:02x?}); it did not take the job, so nothing was printed. \
                     Check the printer for an error, then try again.",
                    st.raw
                ),
                JobOutcome::Fault(st) => {
                    let retryable = !st.fault_is_persistent() && attempt < attempts;
                    if !retryable {
                        anyhow::bail!(
                            "the printer reported a fault and dropped the job; nothing, or \
                             only part of the label, was printed: {}",
                            st.fault_summary()
                        );
                    }
                    eprintln!(
                        "warning: the printer dropped the job ({}); sending it again",
                        st.fault_summary()
                    );
                }
            }
        }
    }

    Ok(())
}

/// Renders one label's text with the CLI's layout settings (size,
/// ratios, wrap length, alignment, margins, frame) into raster rows and
/// the leading-margin row count.
/// The top/bottom non-printing margins to render with, in dots. In
/// cable mode the printable band is reduced to the top `cable_mm` of the
/// tape by pushing the bottom margin up, so the text lands on the
/// label's printable area and the rest stays clear.
fn effective_margins(args: &Args, tape: Tape) -> Result<(usize, usize)> {
    if !args.cable {
        return Ok((args.top_margin, args.bottom_margin));
    }
    let mm = match args.cable_mm.or_else(|| tape.width.cable_printable_mm()) {
        Some(mm) => mm,
        None => anyhow::bail!(
            "--cable needs a tape width with a cable-marking variant (24mm or 36mm), got {}mm; \
             pass --cable-mm to set the printable height explicitly",
            tape.width_mm()
        ),
    };
    // `mm` is measured from the top edge of the tape, but the head is
    // narrower than the tape and cannot reach the outermost dots, so the
    // usable part of the printable area is shorter by that unreachable
    // margin. Place the band so its lowest dot is the last one inside
    // the printable area.
    let total = tape.bitmap_bits();
    let unreachable_top = tape.unreachable_edge_dots();
    let printable = tape.mm_to_rows(mm) as usize;
    let bottom = (unreachable_top + total).saturating_sub(printable);
    if printable <= unreachable_top + args.top_margin || bottom >= total - args.top_margin {
        anyhow::bail!(
            "cable printable height {mm}mm leaves nothing to print on {}mm tape: the print \
             head cannot reach the top {:.1}mm of the tape",
            tape.width_mm(),
            tape.rows_to_mm(unreachable_top as u32)
        );
    }
    let usable = total - args.top_margin - bottom;
    eprintln!(
        "cable label: printable area is the top {mm}mm of the tape; the head cannot reach its \
         top {:.1}mm, so printing in the remaining {:.1}mm ({usable} dots)",
        tape.rows_to_mm(unreachable_top as u32),
        tape.rows_to_mm(usable as u32)
    );
    Ok((args.top_margin, bottom))
}

fn render_label(
    renderer: &FontRenderer,
    args: &Args,
    tape: Tape,
    line_ratios: Option<&[f32]>,
    text: &str,
    top_margin: usize,
    bottom_margin: usize,
) -> Result<(Vec<Vec<u8>>, u32)> {
    let (rows, margin) = if let Some(mode) = args.vertical {
        let size = match args.size {
            Some(size) => size,
            None => {
                // Across the tape a segment is as wide as the text is long,
                // so the tape width no longer bounds the size the way it
                // does for ordinary labels; start from the same figure and
                // let the fit check report anything too big. Stacked
                // columns share the width, so divide it between them.
                let columns = match mode {
                    VerticalMode::Stacked => text.split('\n').count().max(1),
                    VerticalMode::Rotated => 1,
                };
                let size =
                    renderer.max_size_pt_for_tape(tape, top_margin, bottom_margin) / columns as f32;
                if columns > 1 {
                    eprintln!("no --size given; using {size:.1}pt ({columns} columns on {}mm tape)", tape.width_mm());
                } else {
                    eprintln!("no --size given; using {size:.1}pt (largest that fits {}mm tape)", tape.width_mm());
                }
                size
            }
        };
        renderer.render_text_vertical(
            text,
            size,
            tape,
            args.left_margin_mm,
            args.right_margin_mm,
            top_margin,
            bottom_margin,
            mode,
        )?
    } else if let Some(ratios) = line_ratios {
        // With ratios, --size (when given) is the largest-ratio line's
        // size, so the existing would_clip warning on it bounds every
        // line.
        if let Some(size) = args.size {
            if renderer.would_clip(size, tape, top_margin, bottom_margin) {
                let max = renderer.max_size_pt_for_tape(tape, top_margin, bottom_margin);
                eprintln!(
                    "warning: {size}pt may be too large for {}mm tape and could be \
                     clipped at the top/bottom; try --size {max:.1} or smaller",
                    tape.width_mm()
                );
            }
        }
        renderer.render_text_ratios_aligned(
            text,
            args.size,
            ratios,
            tape,
            args.left_margin_mm,
            args.right_margin_mm,
            top_margin,
            bottom_margin,
            args.align,
            args.max_tape_length_mm,
        )?
    } else if let Some(tape_length_mm) = args.max_tape_length_mm {
        // Multi-line mode: wrap text to fit the specified tape length.
        let size = match args.size {
            Some(size) => {
                // Validate manually since would_clip is tape-width-based,
                // but the per-band check happens inside render_text_multiline.
                if renderer.would_clip(size, tape, top_margin, bottom_margin) {
                    let max = renderer.max_size_pt_for_tape(tape, top_margin, bottom_margin);
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
            text,
            size,
            tape,
            args.left_margin_mm,
            args.right_margin_mm,
            tape_length_mm,
            args.align,
            top_margin,
            bottom_margin,
        )?
    } else {
        // Single-line mode, or implicit multi-line via '\n'.
        let num_lines = text.lines().count().max(1);
        let size = match args.size {
            Some(size) => {
                if renderer.would_clip(size, tape, top_margin, bottom_margin) {
                    let max = renderer.max_size_pt_for_tape(tape, top_margin, bottom_margin);
                    eprintln!(
                        "warning: {size}pt may be too large for {}mm tape and could be \
                         clipped at the top/bottom; try --size {max:.1} or smaller",
                        tape.width_mm()
                    );
                }
                size
            }
            None => {
                let max = renderer.max_size_pt_for_tape(tape, top_margin, bottom_margin);
                let size = max / num_lines as f32;
                if num_lines > 1 {
                    eprintln!("no --size given; using {size:.1}pt ({num_lines} lines on {}mm tape)", tape.width_mm());
                } else {
                    eprintln!("no --size given; using {size:.1}pt (largest that fits {}mm tape)", tape.width_mm());
                }
                size
            }
        };
        renderer.render_text_aligned(text, size, tape, args.left_margin_mm, args.right_margin_mm, top_margin, bottom_margin, args.align)?
    };

    // Optional frame: post-process the rendered rows so the same
    // frame appears in the print job and in every preview path.
    let mut rows = rows;
    if args.frame {
        let frame_rows = match args.frame_mm {
            Some(mm) if mm > 0.0 => Some(tape.mm_to_rows(mm) as usize),
            Some(mm) => anyhow::bail!("--frame-mm must be a positive number, got {mm}"),
            None => None,
        };
        tpsr3700::font::apply_frame_sized(
            &mut rows,
            tape.bitmap_bits(),
            top_margin,
            bottom_margin,
            args.frame_width,
            frame_rows,
        );
    }
    Ok((rows, margin))
}

#[cfg(test)]
mod tests {
    use super::{check_loaded_tape, detect_json, detect_tape_json, nominal_mm_for_detected_index, parse_line_ratios};
    use tpsr3700::tepra::TapeWidth;

    #[test]
    fn nominal_mm_matches_protocol_table() {
        // docs/protocol.md section 1.5
        assert_eq!(nominal_mm_for_detected_index(1), Some(6));
        assert_eq!(nominal_mm_for_detected_index(2), Some(9));
        assert_eq!(nominal_mm_for_detected_index(3), Some(12));
        assert_eq!(nominal_mm_for_detected_index(4), Some(18));
        assert_eq!(nominal_mm_for_detected_index(5), Some(24));
        assert_eq!(nominal_mm_for_detected_index(6), Some(36));
        assert_eq!(nominal_mm_for_detected_index(11), Some(4));
        // 0 = no tape loaded.
        assert_eq!(nominal_mm_for_detected_index(0), None);
        assert_eq!(nominal_mm_for_detected_index(7), None);
        assert_eq!(nominal_mm_for_detected_index(255), None);
    }

    #[test]
    fn loaded_tape_matching_the_job_is_accepted() {
        assert!(check_loaded_tape(Some(TapeWidth::Mm9), 2, TapeWidth::Mm9).unwrap().is_none());
    }

    #[test]
    fn loaded_tape_of_another_width_is_refused() {
        let err = check_loaded_tape(Some(TapeWidth::Mm9), 2, TapeWidth::Mm12).unwrap_err().to_string();
        assert!(err.contains("9mm tape loaded"), "{err}");
        assert!(err.contains("laid out for 12mm"), "{err}");
    }

    #[test]
    fn no_tape_loaded_is_refused() {
        let err = check_loaded_tape(None, 0, TapeWidth::Mm9).unwrap_err().to_string();
        assert!(err.contains("no tape is loaded"), "{err}");
    }

    #[test]
    fn unknown_tape_index_only_warns() {
        let warning = check_loaded_tape(None, 7, TapeWidth::Mm9).unwrap();
        assert!(warning.unwrap().contains("index 7"));
    }

    #[test]
    fn detect_json_supported_width() {
        assert_eq!(
            detect_tape_json(Some(TapeWidth::Mm18), 4),
            "{\"tape_mm\": 18, \"index\": 4, \"supported\": true, \"device_id\": null, \"model\": null}"
        );
        assert_eq!(
            detect_json(Some(TapeWidth::Mm6), 1, Some("MFG:KING JIM;CMD:ESCPL2;MDL:TEPRA PRO SR750;CLS:PRINTER;")),
            "{\"tape_mm\": 6, \"index\": 1, \"supported\": true, \"device_id\": \"MFG:KING JIM;CMD:ESCPL2;MDL:TEPRA PRO SR750;CLS:PRINTER;\", \"model\": \"sr3900p\"}"
        );
        assert_eq!(
            detect_json(None, 7, Some("MDL:XYZ \"q\"")),
            "{\"tape_mm\": null, \"index\": 7, \"supported\": false, \"device_id\": \"MDL:XYZ \\\"q\\\"\", \"model\": null}"
        );
    }

    #[test]
    fn detect_json_9mm_and_36mm_are_supported() {
        assert_eq!(
            detect_tape_json(Some(TapeWidth::Mm9), 2),
            "{\"tape_mm\": 9, \"index\": 2, \"supported\": true, \"device_id\": null, \"model\": null}"
        );
        assert_eq!(
            detect_tape_json(Some(TapeWidth::Mm36), 6),
            "{\"tape_mm\": 36, \"index\": 6, \"supported\": true, \"device_id\": null, \"model\": null}"
        );
    }

    #[test]
    fn detect_json_reports_4mm_and_no_tape() {
        // 4mm is index 11, confirmed on hardware.
        assert_eq!(
            detect_tape_json(Some(TapeWidth::Mm4), 11),
            "{\"tape_mm\": 4, \"index\": 11, \"supported\": true, \"device_id\": null, \"model\": null}"
        );
        // 0 = no tape loaded.
        assert_eq!(
            detect_tape_json(None, 0),
            "{\"tape_mm\": null, \"index\": 0, \"supported\": false, \"device_id\": null, \"model\": null}"
        );
    }

    #[test]
    fn detect_json_unknown_index_reports_null() {
        assert_eq!(
            detect_tape_json(None, 9),
            "{\"tape_mm\": null, \"index\": 9, \"supported\": false, \"device_id\": null, \"model\": null}"
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

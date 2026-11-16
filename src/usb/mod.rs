//! USB transport: sends a TEPRA print job to the printer over its USB
//! Printer-class bulk endpoints.
//!
//! Despite Windows' USBPcap capture showing what looked like a USB Mass
//! Storage Bulk-Only Transport exchange (`USBC`/`USBS` signatures used for
//! `TEST UNIT READY` / `REQUEST SENSE` polling), the device's USB
//! descriptors advertise a standard USB Printer class interface
//! (bInterfaceClass=7, bInterfaceSubClass=1, bInterfaceProtocol=2
//! "Bidirectional"), and `GET_DEVICE_ID` confirms this:
//!
//! ```text
//! <MFG:KING JIM;CMD:ESCPL2;MDL:TEPRA PRO SR3700P;CLS:PRINTER;
//! ```
//!
//! Empirically, sending the raw TEPRA job bytes (see [`crate::tepra`])
//! directly to the bulk OUT endpoint — with no Mass Storage command
//! wrapping at all — is sufficient to print. The BOT-like exchange seen in
//! the Windows capture appears to be either a red herring (from an
//! entirely different, coincidentally similar looking sequence) or
//! optional status-polling behavior that isn't required for the printer
//! to accept a job.

use anyhow::{bail, Context, Result};
use rusb::{Device, DeviceHandle, GlobalContext};
use std::time::Duration;

/// KING JIM TEPRA SR3700P's USB vendor ID.
///
/// Confirmed via `lsusb`: `ID 0d8a:0103 King Jim Co., Ltd SR3700P`.
pub const VENDOR_ID: u16 = 0x0d8a;

/// KING JIM TEPRA SR3700P's USB product ID.
///
/// Confirmed via `lsusb`: `ID 0d8a:0103 King Jim Co., Ltd SR3700P`.
pub const PRODUCT_ID: u16 = 0x0103;

/// Parses a USB ID given as a CLI/env string. USB IDs are written in
/// hex by `lsusb` and everywhere else, so both `0x`-prefixed and bare
/// values are interpreted as hexadecimal (e.g. "0103" means 0x0103).
pub fn parse_usb_id(s: &str) -> Result<u16> {
    let s = s.trim();
    let hex = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u16::from_str_radix(hex, 16)
        .map_err(|e| anyhow::anyhow!("invalid hex USB ID '{s}': {e}"))
}

fn env_usb_id(name: &str, default: u16) -> u16 {
    match std::env::var(name) {
        Ok(v) => parse_usb_id(&v).unwrap_or(default),
        Err(_) => default,
    }
}

/// Effective vendor ID: `TEPRA_USB_VID` env var, else [`VENDOR_ID`].
///
/// This fork makes the IDs overridable so other TEPRA PRO models (e.g.
/// the SR3900P, whose USB product ID is not yet confirmed) can be
/// targeted without recompiling: run `lsusb` with the printer attached,
/// find the `0d8a:xxxx` entry, and pass the product part via `--pid` /
/// `TEPRA_USB_PID`.
pub fn vendor_id() -> u16 {
    env_usb_id("TEPRA_USB_VID", VENDOR_ID)
}

/// Effective product ID: `TEPRA_USB_PID` env var, else [`PRODUCT_ID`].
pub fn product_id() -> u16 {
    env_usb_id("TEPRA_USB_PID", PRODUCT_ID)
}

/// Finds the TEPRA printer on the USB bus using the effective
/// (env-overridable) vendor/product IDs.
pub fn find_device() -> Result<Device<GlobalContext>> {
    find_device_with(vendor_id(), product_id())
}

/// Finds a USB device with the given vendor/product IDs.
pub fn find_device_with(vid: u16, pid: u16) -> Result<Device<GlobalContext>> {
    for device in rusb::devices()?.iter() {
        let desc = device.device_descriptor()?;
        if desc.vendor_id() == vid && desc.product_id() == pid {
            return Ok(device);
        }
    }
    bail!(
        "TEPRA printer not found (looking for vendor={vid:#06x} product={pid:#06x}); \
         run `lsusb` to find the correct IDs and pass them via --pid / TEPRA_USB_PID \
         (defaults are the SR3700P's 0d8a:0103)"
    );
}

/// How a sent job ended, as seen through the printer's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// The printer went busy and came back idle: it printed.
    Printed(PrinterStatus),
    /// The printer never reported itself busy within the grace period.
    /// It did not take the job.
    NeverStarted(PrinterStatus),
    /// The printer raised a fault instead of finishing the job.
    Fault(PrinterStatus),
}

/// An open connection to the printer's bulk endpoints.
pub struct Printer {
    handle: DeviceHandle<GlobalContext>,
    interface: u8,
    endpoint_out: u8,
    #[allow(dead_code)]
    endpoint_in: u8,
}

/// Decoded printer status, from the 8-byte vendor status response (the
/// same request the "detect tape width" feature uses; see
/// [`Printer::get_tape_info_raw`]). Byte 1 is the activity state
/// (observed on hardware: `0x00` idle, `0x02` printing, `0x05` cutting),
/// and byte 3 is the tape index (see [`crate::tepra::TapeWidth::from_detected_index`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrinterStatus {
    pub raw: [u8; 8],
}

impl PrinterStatus {
    /// Activity byte (byte 1): 0 = idle.
    pub fn activity(&self) -> u8 {
        self.raw[1]
    }

    pub fn is_idle(&self) -> bool {
        self.activity() == 0
    }

    /// Tape width index (byte 3); see `TapeWidth::from_detected_index`.
    ///
    /// Latched when the cover closes rather than sampled live: taking
    /// the cassette out with the cover open leaves the previous width
    /// here. That only matters while the cover is open, which is itself
    /// reported as a fault, so a printable state always has an index
    /// that matches the cassette.
    pub fn tape_index(&self) -> u8 {
        self.raw[3]
    }

    /// Whether the printer is reporting a fault.
    ///
    /// Bytes 2 and 5 are zero in every healthy sample taken so far --
    /// idle, printing and cutting, on hardware and in the Windows
    /// driver's own capture. They were set in a job that printed nothing
    /// and gave up after 2.4s while reporting success to the host
    /// (`08 02 20 02 00 10 00 00`, 2026-11-17). What each bit means is
    /// not decoded; what matters is that zero means healthy, so anything
    /// else is a job that should not be reported as printed.
    pub fn has_error(&self) -> bool {
        self.raw[2] != 0 || self.raw[5] != 0
    }

    /// What the fault bits are known to mean, for the error message.
    ///
    /// Samples taken on an SR750 (2026-11-17), with the printer put into
    /// each state by hand except the last:
    ///
    /// ```text
    /// 08 00 00 02 00 00 00 00   healthy, 9mm tape loaded
    /// 08 00 06 00 00 00 00 00   no tape cassette (tape index 0 as well)
    /// 08 00 21 02 00 80 00 00   cover open (the cassette is still seen)
    /// 08 00 21 00 00 80 00 00   cover open with the cassette out
    /// 08 02 20 02 00 10 00 00   job dropped mid-print, cause unknown
    /// ```
    ///
    /// So byte 2 is a set of condition bits -- `0x01` cover, `0x06` no
    /// cassette -- and `0x20` marks the states that stopped a job, while
    /// byte 5 names that cause: `0x80` cover, `0x10` whatever dropped
    /// the job above. The missing cassette is reported through `0x06`
    /// only while the cover is shut; with it open the tape index going
    /// to 0 is the only sign, which is why that is checked as well.
    /// None of this is from documentation, so the raw bytes are always
    /// included.
    /// Whether a fault needs someone to go to the printer, as opposed to
    /// one that may clear by itself. Only the two conditions that have
    /// been reproduced by hand -- an open cover and a missing cassette --
    /// count as persistent; anything else is treated as worth one more
    /// try, since the job that produced `0x10` printed nothing and the
    /// same job went through minutes later.
    pub fn fault_is_persistent(&self) -> bool {
        self.raw[2] & 0x01 != 0
            || self.raw[5] & 0x80 != 0
            || self.raw[2] & 0x06 != 0
            || self.tape_index() == 0
    }

    pub fn fault_summary(&self) -> String {
        let mut causes: Vec<&str> = Vec::new();
        if self.raw[2] & 0x01 != 0 || self.raw[5] & 0x80 != 0 {
            causes.push("the cover is open");
        }
        if self.raw[2] & 0x06 != 0 || self.tape_index() == 0 {
            causes.push("no tape cassette is loaded");
        }
        if causes.is_empty() {
            causes.push("cause not decoded yet");
        }
        format!("{}; status {:02x?}", causes.join(", "), self.raw)
    }
}

impl Printer {
    /// Open the printer and claim its Printer-class interface, using
    /// the effective (env-overridable) vendor/product IDs.
    pub fn open() -> Result<Self> {
        Self::open_with(vendor_id(), product_id())
    }

    /// Open the printer with explicit USB vendor/product IDs.
    pub fn open_with(vid: u16, pid: u16) -> Result<Self> {
        let device = find_device_with(vid, pid)?;
        let config = device.active_config_descriptor()?;
        let interface = config
            .interfaces()
            .next()
            .context("device has no interfaces")?;
        let descriptor = interface
            .descriptors()
            .next()
            .context("interface has no descriptors")?;

        let mut endpoint_out = None;
        let mut endpoint_in = None;
        for ep in descriptor.endpoint_descriptors() {
            use rusb::Direction;
            use rusb::TransferType;
            if ep.transfer_type() != TransferType::Bulk {
                continue;
            }
            match ep.direction() {
                Direction::Out => endpoint_out = Some(ep.address()),
                Direction::In => endpoint_in = Some(ep.address()),
            }
        }

        let endpoint_out = endpoint_out.context("no bulk OUT endpoint found")?;
        let endpoint_in = endpoint_in.context("no bulk IN endpoint found")?;

        let handle = device.open().with_context(|| format!(
            "failed to open the printer's USB device (permission denied?)\n\n\
             This usually means your user doesn't have read/write permission to\n\
             access the printer's USB device node. Try one of:\n\
             \n\
             1. Create a udev rule (recommended — survives reboots):\n\
             \n\
             \x20   echo 'SUBSYSTEM==\"usb\", ATTRS{{idVendor}}==\"{vid:04x}\", ATTRS{{idProduct}}==\"{pid:04x}\", GROUP=\"lp\", MODE=\"0660\"' \\\n\
             \x20       | sudo tee /etc/udev/rules.d/99-tepra.rules\n\
             \x20   sudo usermod -aG lp $USER\n\
             \x20   sudo udevadm control --reload && sudo udevadm trigger\n\
             \n\
             \x20   Then unplug/re-plug the printer and start a new login session\n\
             \x20   (or run `newgrp lp` in the current shell).\n\
             \n\
             2. Add your user to the `lp` group and start a new login session\n\
             (log out/in, or run `newgrp lp` in a new shell):\n\
             \n\
             \x20   sudo usermod -aG lp $USER\n\
             \n\
             3. Or, run this command with a group override for the current\n\
             shell only (no logout required):\n\
             \n\
             \x20   sg lp -c '<the command you just ran>'\n\
             \n\
             4. Or, run as root (not recommended for routine use):\n\
             \n\
             \x20   sudo <the command you just ran>"))?;

        // On Linux the printer's interface is normally bound to the
        // kernel's `usblp` driver (it advertises USB Printer class 7/1/2),
        // which would otherwise make `claim_interface` fail with "Resource
        // busy". Ask libusb to detach it automatically while we hold the
        // interface, and reattach it when the handle is dropped.
        // This API is Linux-only; macOS has no equivalent kernel driver to
        // detach (IOKit releases the interface automatically).
        #[cfg(target_os = "linux")]
        handle
            .set_auto_detach_kernel_driver(true)
            .context("failed to enable kernel driver auto-detach")?;

        handle.claim_interface(interface.number())?;

        Ok(Self {
            handle,
            interface: interface.number(),
            endpoint_out,
            endpoint_in,
        })
    }

    /// Returns the (OUT, IN) bulk endpoint addresses in use. Exposed for
    /// diagnostics.
    pub fn endpoints(&self) -> (u8, u8) {
        (self.endpoint_out, self.endpoint_in)
    }

    /// USB Printer class `GET_DEVICE_ID` request (bRequest=0). Returns the
    /// device ID string (e.g.
    /// `<MFG:KING JIM;CMD:ESCPL2;MDL:TEPRA PRO SR3700P;CLS:PRINTER;`).
    pub fn get_device_id(&self) -> Result<String> {
        let mut buf = [0u8; 256];
        let n = self
            .handle
            .read_control(0xA1, 0, 0, 0, &mut buf, Duration::from_secs(3))
            .context("GET_DEVICE_ID control request failed")?;
        // First 2 bytes are a big-endian length prefix per the USB Printer
        // class spec; skip them if present and just decode what we can.
        let text = if n >= 2 { &buf[2..n] } else { &buf[..n] };
        Ok(String::from_utf8_lossy(text).into_owned())
    }

    /// Vendor-specific control request (`bmRequestType=0xC1`,
    /// `bRequest=0x01`, `wValue=0`, `wIndex=0`, `wLength=8`) observed being
    /// sent by the TEPRA desktop app's "detect tape width" feature
    /// (captured via USBPcap on Windows). Returns the raw 8-byte response.
    ///
    /// The exact encoding of the response is not fully understood yet; see
    /// `docs/protocol.md` for analysis notes.
    pub fn get_tape_info_raw(&self) -> Result<[u8; 8]> {
        let mut buf = [0u8; 8];
        let n = self
            .handle
            .read_control(0xC1, 0x01, 0, 0, &mut buf, Duration::from_secs(3))
            .context("tape info control request failed")?;
        if n != 8 {
            bail!("unexpected response length: got {n} bytes, expected 8");
        }
        Ok(buf)
    }

    /// Detects the currently loaded tape's width by querying the printer
    /// (see [`Printer::get_tape_info_raw`]) and decoding the tape width
    /// index in byte 3 of the response via
    /// [`crate::tepra::TapeWidth::from_detected_index`].
    ///
    /// Returns `Ok(None)` if the printer reports a tape width index that
    /// isn't currently supported by this crate (see
    /// `TapeWidth::from_detected_index`'s docs for which indices those
    /// are), along with the raw index value for diagnostics.
    pub fn get_tape_width(&self) -> Result<Option<crate::tepra::TapeWidth>> {
        let raw = self.get_tape_info_raw()?;
        let index = raw[3];
        Ok(crate::tepra::TapeWidth::from_detected_index(index))
    }

    /// Same as [`Printer::get_tape_width`], but also returns the raw
    /// detected index (useful for diagnostics/error messages when the
    /// index isn't a currently-supported width).
    pub fn get_tape_width_with_index(&self) -> Result<(Option<crate::tepra::TapeWidth>, u8)> {
        let raw = self.get_tape_info_raw()?;
        let index = raw[3];
        Ok((crate::tepra::TapeWidth::from_detected_index(index), index))
    }

    /// Reads the printer's 8-byte status (same request as
    /// [`Self::get_tape_info_raw`]).
    pub fn get_status(&self) -> Result<PrinterStatus> {
        Ok(PrinterStatus { raw: self.get_tape_info_raw()? })
    }

    /// USB Printer Class SOFT_RESET (bmRequestType 0x21, bRequest 2) to
    /// the printer interface. The SR3900P Windows driver issues this
    /// right before every job (see `docs/protocol.md` section 1.6).
    pub fn soft_reset(&self) -> Result<()> {
        self.handle
            .write_control(0x21, 2, 0, self.interface as u16, &[], Duration::from_secs(3))
            .context("SOFT_RESET failed")?;
        Ok(())
    }

    /// Waits for the printer to be idle, so a job is never sent into a
    /// printer that is still printing or cutting the previous one.
    ///
    /// This matters because the job is preceded by a SOFT_RESET: sending
    /// that while the printer is still working aborts whatever it is
    /// doing, and a job sent while it is busy can be dropped outright.
    pub fn wait_until_ready(
        &self,
        timeout: Duration,
        check_faults: bool,
        mut on_change: impl FnMut(PrinterStatus),
    ) -> Result<PrinterStatus> {
        let start = std::time::Instant::now();
        let mut last: Option<PrinterStatus> = None;
        loop {
            let st = self.get_status()?;
            if last != Some(st) {
                on_change(st);
                last = Some(st);
            }
            if check_faults && st.has_error() {
                bail!(
                    "the printer is reporting a fault: {}. Clear it at the printer and try \
                     again (--no-status-check sends the job anyway)",
                    st.fault_summary()
                );
            }
            if st.is_idle() {
                return Ok(st);
            }
            if start.elapsed() >= timeout {
                bail!(
                    "printer is still busy after {:?}, not sending the job (status {:02x?})",
                    timeout,
                    st.raw
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    ///
    /// Polls every 200ms until the printer first goes busy and every
    /// 500ms after that (the Windows driver polls at 500ms throughout);
    /// the faster initial poll makes a very short job harder to miss.
    /// Waits for a sent job to finish, and says how it went.
    ///
    /// The distinction that matters is between a job that printed and
    /// one the printer quietly dropped: if the printer never reported
    /// itself busy, or reported a fault, calling it printed would be a
    /// lie. See [`JobOutcome`].
    pub fn wait_until_idle(
        &self,
        timeout: Duration,
        grace: Duration,
        check_faults: bool,
        mut on_change: impl FnMut(PrinterStatus),
    ) -> Result<JobOutcome> {
        let start = std::time::Instant::now();
        let mut last: Option<PrinterStatus> = None;
        let mut seen_busy = false;
        loop {
            let st = self.get_status()?;
            if last != Some(st) {
                on_change(st);
                last = Some(st);
            }
            if check_faults && st.has_error() {
                return Ok(JobOutcome::Fault(st));
            }
            if !st.is_idle() {
                seen_busy = true;
            } else if seen_busy {
                return Ok(JobOutcome::Printed(st));
            } else if start.elapsed() >= grace {
                return Ok(JobOutcome::NeverStarted(st));
            }
            if start.elapsed() >= timeout {
                bail!("printer still busy after {:?} (status {:02x?})", timeout, st.raw);
            }
            let poll = if seen_busy { 500 } else { 200 };
            std::thread::sleep(Duration::from_millis(poll));
        }
    }

    /// Bytes per bulk write. The printer only accepts what fits in its
    /// buffer and makes room as it prints, so a whole multi-label job
    /// cannot be handed over in one transfer: the write would block for
    /// as long as the printing takes. Writing in chunks keeps every
    /// single transfer short enough to finish well inside
    /// [`Self::CHUNK_TIMEOUT`].
    ///
    /// It must stay a multiple of the bulk endpoint's maximum packet
    /// size (512 at high speed, 64 at full speed), or every chunk would
    /// end with a short packet, which a printer may read as the end of
    /// the document. 32KiB is a multiple of both, so only the last chunk
    /// of a job is short -- which is exactly what should mark its end.
    const CHUNK_LEN: usize = 32 * 1024;

    /// Timeout for one [`Self::CHUNK_LEN`] write. It has to cover the
    /// time the printer needs to free that much buffer, which means
    /// printing roughly that many rows of tape, so it is generous.
    const CHUNK_TIMEOUT: Duration = Duration::from_secs(120);

    /// Sends a raw byte stream directly to the printer's bulk OUT
    /// endpoint, with no additional framing. Callers normally pass
    /// [`crate::tepra::builder::printer_payload`] of a job rather than
    /// the whole job (see there).
    pub fn send_job(&mut self, job: &[u8]) -> Result<()> {
        self.send_job_with_progress(job, |_, _| {})
    }

    /// Same as [`Self::send_job`], but calls `on_progress(sent, total)`
    /// after every chunk. Because the printer takes the bytes only as
    /// fast as it prints them, this doubles as a progress signal for
    /// long multi-label jobs.
    pub fn send_job_with_progress(
        &mut self,
        job: &[u8],
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<()> {
        let mut sent = 0;
        while sent < job.len() {
            let end = (sent + Self::CHUNK_LEN).min(job.len());
            let n = self
                .handle
                .write_bulk(self.endpoint_out, &job[sent..end], Self::CHUNK_TIMEOUT)
                .with_context(|| {
                    format!(
                        "failed to send print job after {sent} of {} bytes",
                        job.len()
                    )
                })?;
            if n == 0 {
                bail!("USB bulk write made no progress after {sent} of {} bytes", job.len());
            }
            sent += n;
            on_progress(sent, job.len());
        }
        Ok(())
    }

    /// USB Printer Class GET_PORT_STATUS (bmRequestType 0xA1, bRequest
    /// 1): one status byte (`0x18` = selected, paper not empty). The
    /// Windows class driver reads it after SOFT_RESET.
    pub fn get_port_status(&self) -> Result<u8> {
        let mut buf = [0u8; 1];
        self.handle
            .read_control(0xA1, 1, 0, self.interface as u16, &mut buf, Duration::from_secs(3))
            .context("GET_PORT_STATUS failed")?;
        Ok(buf[0])
    }
}

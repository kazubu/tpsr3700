# TEPRA PRO SR3700P / SR3900P Printer Protocol

This document describes the proprietary print job format used by the
KING JIM TEPRA PRO SR3700P and SR3900P label printers, as reverse
engineered for the Rust implementation in this repository. It is the
result of capturing the raw bytes that Windows' vendor drivers send to
the printer, without any access to official KING JIM documentation.

The two models share the job format. They differ in the print head
(SR3700P: 270 dpi, 192 dots; SR3900P: 360 dpi, 384 dots), and therefore
in the number of dots each tape width uses, in a few model-identifying
bytes, and in the physical size of every "dot" quantity below. The
SR3700P findings (6/12/18mm, hardware-verified) come from upstream; the
SR3900P findings (9mm and 36mm, every cut mode, one and two copies)
come from captures of the SR3900P driver taken on 2026-11-15
(`tests/fixtures/sr3900p-*.prn`) and have not been printed yet.

All findings here are empirical: derived by diffing several captured
print jobs against each other (different text, font sizes, and tape
widths) and, for the USB transport layer, by testing directly against
real hardware. Fields whose meaning could not be determined are called
out explicitly.

## 1. Transport: USB

### 1.1 Device identification

```
$ lsusb -d 0d8a:0103
Bus 001 Device 059: ID 0d8a:0103 King Jim Co., Ltd SR3700P
```

- Vendor ID: `0x0d8a` (King Jim Co., Ltd)
- Product ID: `0x0103` (SR3700P)
- USB 1.10, Full Speed (12 Mbps), bus-powered (100 mA)

### 1.2 USB descriptors

The device exposes a single configuration with a single interface
using the standard **USB Printer class**:

```
Interface Descriptor:
  bInterfaceClass         7   (Printer)
  bInterfaceSubClass      1   (Printer)
  bInterfaceProtocol      2   (Bidirectional)
  bNumEndpoints           2
  Endpoint 0x81  IN,  Bulk, wMaxPacketSize=64
  Endpoint 0x02  OUT, Bulk, wMaxPacketSize=64
```

The standard USB Printer class `GET_DEVICE_ID` control request
(`bmRequestType=0xA1, bRequest=0`) returns:

```
<MFG:KING JIM;CMD:ESCPL2;MDL:TEPRA PRO SR3700P;CLS:PRINTER;
```

Note the `CMD:ESCPL2` field — the vendor calls its command language
"ESCPL2", though it bears no relation to Epson's ESC/P or ESC/P2.

Because the interface advertises the standard Printer class, Linux's
kernel automatically binds the `usblp` driver to it. Any userspace
program that wants to talk to the printer directly over
`libusb`/`rusb` must ask libusb to detach the kernel driver first
(`set_auto_detach_kernel_driver(true)` before `claim_interface`),
otherwise `claim_interface` fails with "Resource busy".

### 1.3 Sending a job

**Sending a print job is a single USB bulk write.** The full job byte
stream (see section 2) is written directly to the bulk OUT endpoint
(`0x02`), with **no additional framing, no handshake, and no response
expected on the bulk IN endpoint**. libusb/the OS automatically split
the write into 64-byte USB transactions as needed; there is no need to
chunk the write manually.

```rust
handle.write_bulk(0x02, &job_bytes, timeout)?;
```

This was confirmed by printing successfully from Linux with exactly
this sequence (see `examples/probe.rs` and `src/usb/mod.rs`).

### 1.4 A dead end: USB Mass Storage Bulk-Only Transport

An earlier capture of the **Windows** driver's USB traffic (via
USBPcap) showed what looked exactly like a USB Mass Storage
Bulk-Only Transport (BOT) exchange: 31-byte Command Block Wrappers
starting with the `USBC` signature, 13-byte Command Status Wrappers
starting with `USBS`, and SCSI-like opcodes (`TEST UNIT READY` =
`0x00`, `REQUEST SENSE` = `0x03`) used to poll whether tape was
loaded.

This looked like a plausible transport (and would explain why the
printer shows up as a "disk-like" device to some tools), so an initial
implementation replicated this BOT protocol faithfully. **It did not
work**: sending a `TEST UNIT READY` CBW to the real printer over Linux
resulted in the printer never responding on the bulk IN endpoint at
all (confirmed both with `rusb` and with Python's `pyusb`, ruling out
a Rust/library-specific bug).

Once real device probing revealed the actual USB Printer class
descriptors and the `CMD:ESCPL2`/`CLS:PRINTER` device ID string, the
BOT hypothesis was abandoned in favor of just writing the raw job
bytes to the bulk OUT endpoint directly — which worked immediately.
The `USBC`/`USBS` exchange seen in the Windows capture remains
unexplained (possibly an artifact of how the Windows class driver or
a status-monitoring utility polls tape presence through some other
mechanism entirely unrelated to submitting the job), but is not
required to print.

### 1.5 Detecting tape width

The TEPRA desktop app has a "detect tape width" feature that queries
the printer for the currently loaded tape's width, rather than relying
on the user to select it manually. This is a single vendor-specific
USB control transfer, captured via USBPcap on Windows
(`captures/obtain-tape-width.pcap`) and confirmed against real
hardware for three tape widths:

```
bmRequestType = 0xC1  (Device-to-Host, Vendor, Interface)
bRequest      = 0x01
wValue        = 0x0000
wIndex        = 0x0000
wLength       = 8
```

The response is always 8 bytes long. Byte 3 (0-indexed) is a tape
width *index*, not the width in mm directly:

| Tape loaded | Response bytes (hex)      | Byte 3 (index) |
|------------:|----------------------------|:---------------:|
| 6mm         | `08 00 00 01 00 00 00 00`  | 1                |
| 12mm        | `08 00 00 03 00 00 00 00`  | 3                |
| 18mm        | `08 00 00 04 00 00 00 00`  | 4                |

The mapping, established by loading each tape and running
`--detect-tape`:

| Index | Tape |
|:-----:|------|
| 0     | no tape loaded |
| 1     | 6mm |
| 2     | 9mm |
| 3     | 12mm |
| 4     | 18mm |
| 5     | 24mm |
| 6     | 36mm |
| 11    | 4mm |

Confirmed on hardware: 0, 1, 2, 3, 4 and 11 (6/12/18mm on an SR3700P;
no tape, 9mm and 4mm on an SR750). 5 and 6 continue the 6..36mm run and
are assumed. Note that 4mm sits at 11, not at 0, so the indices are not
simply positions in KING JIM's published width lineup; no index is known
to mean 50mm. The other 7 bytes are not fully understood;
byte 0 is a constant `0x08` and byte 1 is an activity state (`00` idle,
`02` printing, `05` cutting, observed while printing on hardware).

The tape feed operations ("tape feed" / "feed and cut") are plain print
jobs of blank raster rows, cut per the `C` command; `--feed-mm` sends
one. A full/half cut also ejects the fixed head-to-cutter lead (~9mm on
an SR750: a 10mm feed came out as 19mm) as a separate leading scrap, so
feed-and-cut produces that scrap plus the fed piece; feed without a cut
to avoid it.

### Identifying the printer

The model is identified by name from the USB printer-class
`GET_DEVICE_ID` string (`MFG:KING JIM;CMD:ESCPL2;MDL:TEPRA PRO
SR3700P;CLS:PRINTER;` on the SR3700P, `...MDL:TEPRA PRO SR750;...` on an
SR750). `--detect-tape` prints that string and the model looked up from
it (`KNOWN_PRINTERS` in `src/tepra/mod.rs`), and `--model` defaults to
it.

### 1.6 Job submission sequence (SR3900P Windows driver)

A USBPcap capture of the SR3900P Windows driver printing one 9mm
half-cut label (`capture-kit/usbcap_9mm_half.pcapng`, 2026-11-15)
shows this sequence on the wire:

| t (s)  | Transfer | Bytes | Notes |
|-------:|----------|------:|-------|
| 7.3    | `C1 01` vendor status (IN, 8 B) | `08 00 00 02 00 00 00 00` | tape index 2 (9mm), activity byte 1 = `00` idle |
| 10.459 | `21 02 0000 0000` **SOFT_RESET** (USB printer class, to interface) | — | issued right before the job |
| 10.459 | `A1 01` GET_PORT_STATUS (IN, 1 B) | `18` | printer class port status |
| 10.478 | bulk OUT | 4089 | **starts at the `C` command** — the 2179-byte header block is not sent |
| 10.491 | bulk OUT | 4092 | raster rows |
| 10.497 | bulk OUT | 1833 | rest of the job, ending with `0C` + `@` frame |
| 10.51–18.7 | `C1 01` every ~0.5 s | `08 02 00 02 …` | activity `02` = printing, from immediately after the data |
| 19.19  | `C1 01` | `08 05 00 02 …` | activity `05`, one sample (cutting?) |
| 19.70  | `C1 01` | `08 00 00 02 …` | idle again |

Total bulk data: 10014 bytes = the 12193-byte `FILE:` capture of the
same job minus its 2179-byte header block. No bulk IN reads, no
zero-length packets, and no other requests occur. The driver also
interleaves a `C1 01` status read between its ~4 KB bulk chunks.

Consequences for this implementation:

- `send_job` is given [`printer_payload`](../src/tepra/builder.rs) of the
  job, i.e. everything after the header block. Sending the header too
  (what every earlier version of this crate did) made an SR3900P wait
  about 20 seconds before starting to print, and dropping it was
  confirmed on an SR3900P to make printing start immediately; the
  SR3700P printed with it, but that may simply have been the same
  timeout going unnoticed.
  `--send-header` restores the old behaviour for experiments.
- `SOFT_RESET` is issued before every job (`--no-soft-reset` to skip).
- `--wait` polls the status until the activity byte returns to `00`,
  which is how the Web frontend serialises consecutive jobs.

## 2. Job byte stream format

A complete print job is a flat byte stream with four parts, back to
back, in this order:

```
+----------------+------------------------+------------------+--------+
| Header block   | Configuration commands | Raster rows (xN) | Footer |
| (fixed 2179 B) | C, D, G, L, T           | one per column   |        |
+----------------+------------------------+------------------+--------+
```

### 2.1 Common framing: `ESC { }` command frames

Most parts of the protocol other than the raster rows use a common
"command frame" wrapper:

```
0x1B 0x7B <len> <cmd> <params...> <checksum> 0x7D
 ESC  '{'                                     '}'
```

- `cmd` is a single ASCII byte identifying the command (`C`, `D`, `G`,
  `L`, `T`, `@`, or — for the header block only — a fixed marker byte
  `0x80` that does not follow this rule, see 2.2).
- `len` is the number of bytes from `cmd` (inclusive) through the
  checksum (inclusive), i.e. `1 (cmd) + params.len() + 1 (checksum)`.
- `checksum` is the sum of `cmd` and all `params` bytes, truncated to
  a `u8` (`sum mod 256`). This checksum formula is used consistently
  by every frame in the protocol, including the large header block
  (section 2.2).

Implemented in `src/tepra/frame.rs`.

### 2.2 Header block

The header block is a single large frame, structurally similar to the
command frames above but with a fixed length marker instead of a
computed one:

```
0x1B 0x7B 0x80 <2174 bytes of payload> <checksum> 0x7D
```

It is always exactly 2179 bytes (`2 + 1 + 2174 + 1 + 1`). Diffing
several captured jobs (varying tape width, font size, and character
count) shows the block is almost entirely constant. Known fields
(offsets are relative to the start of the frame, i.e. including the
leading `ESC {` and the `0x80` length byte):

| Offset | Field                        | Notes |
|-------:|-------------------------------|-------|
| 0–2    | `1B 7B 80`                     | Frame start + fixed length marker |
| 8      | Tape width in mm               | `0x06`=6mm, `0x09`=9mm, `0x0C`=12mm, `0x12`=18mm, `0x18`=24mm (experimental), `0x24`=36mm |
| 9      | Model / driver byte            | `0x0E` in every SR3700P driver capture, `0x68` in every SR3900P driver capture; meaning unknown (`Model::header_byte_9`). |
| 0x21   | `"SPC10"` (UTF-16LE)            | Unknown purpose; possibly a format/protocol version tag |
| 0x61   | `"306"` / `"500"` (UTF-16LE)    | Unknown purpose; `"306"` from the SR3700P driver, `"500"` from the SR3900P driver (`Model::header_version_tag`). Looks like a software version. |
| 32     | Unknown flag                   | `0x01` in every SR3700P capture and in the SR3900P two-copy 9mm capture; `0x00` in the other SR3900P captures (including the two-copy 36mm one). Not tape width, cut mode or copy count. Kept at `0x01`. |
| 0x81   | `"obj_barcode\t\nprint_form\tGeneral\ncatalog_desc\t"` (ASCII) | Unknown purpose; looks like leftover template/object metadata from the desktop app's internal document model |
| 20     | Grows by 4 per extra character  | `value = 12 + 4 * char_count`. Confirmed for char_count 1 and 2; likely related to some per-character/per-object table size, but the underlying table itself has not been located. |
| 23     | Tape-width-dependent byte, unknown meaning | 6mm→3, 9mm→4, 12mm→5, 18mm→6, 36mm→8 (captured), 24mm→7 (interpolated). An ordinal into the tape lineup; same on both models (see 3.2). |
| 2177   | Checksum                       | `sum(bytes[3..2177]) mod 256` |
| 2178   | `0x7D`                          | Frame end |

Everything else in the 2174-byte payload has been observed to be
constant (mostly zero-filled) across every capture taken so far and is
kept verbatim as a template (`assets/header_18mm_template.bin`); only
the fields above are patched before recomputing the checksum.

Implemented in `src/tepra/header.rs`.

### 2.3 Configuration commands

Three `ESC { }` command frames follow the header block, then one
`L`/`T` pair per copy (see 2.6):

| Cmd | Params (hex)         | Meaning |
|-----|----------------------|---------|
| `C` | 4 bytes               | **Cut mode** (SR3900P captures that differed only in the driver's cut setting): full cut `03 01 01 01`, half cut `02 02 01 01`, no cut `00 00 00 00`. Every SR3700P capture used full cut. Individual byte meanings unknown; the three observed combinations are used verbatim (`CutMode` in `src/tepra/builder.rs`). |
| `D` | `05`                  | Unknown. Constant in every capture. |
| `G` | *(none)*              | Unknown. Constant in every capture. |
| `L` | `<u32 LE>`            | Number of raster rows that follow. |
| `T` | `<u16 LE>`            | Number of leading blank raster rows (the print content's offset from the start of the raster data). With the app's default margin setting the SR3900P captures have 48 blank rows (3.4mm) before the first ink and 66 (4.7mm) after the last; `T` is the first inked row, so it is 59 on 4/6mm tape where the top of the "T" is clipped off. |

Example (from a captured 36pt "A" on 18mm tape, 170 total raster
rows, content starting at row 36, full cut):

```
1B 7B 07 43 03 01 01 01 49 7D   ESC{ len=7 'C' 03 01 01 01 checksum=0x49 }
1B 7B 04 44 05 49 7D            ESC{ len=4 'D' 05          checksum=0x49 }
1B 7B 03 47 47 7D               ESC{ len=3 'G'             checksum=0x47 }
1B 7B 07 4C AA 00 00 00 F6 7D   ESC{ len=7 'L' 170 (LE32)   checksum=0xF6 }
1B 7B 05 54 24 00 78 7D         ESC{ len=5 'T' 36  (LE16)   checksum=0x78 }
```

Implemented in `src/tepra/builder.rs` (frame ordering/assembly) and
`src/tepra/frame.rs` (frame encoding).

### 2.4 Raster rows

After `T`, exactly `L` raster rows follow back to back, with **no
separators and no per-row checksum**:

```
0x1B 0x2E 0x00 0x0D 0x0D 0x01 <dots: u16 LE> <bitmap: dots/8 bytes>
 ESC  '.'  <- 4 fixed bytes ->  ^ tape-width dependent
```

Each row corresponds to one column of dots along the tape feed
direction; the bits within a row's bitmap correspond to dot positions
across the tape's width. Rendering text is therefore done glyph
column by glyph column, with **no explicit rotation needed**: a
glyph's X axis maps directly to the row index (feed direction) and its
Y axis maps directly to the bit position within a row (tape width
direction). This was confirmed by reconstructing readable "A" and "B"
glyphs pixel-for-pixel from captured jobs.

The 4 fixed bytes after `ESC .` are `00 0D 0D 01` in every SR3700P
driver capture and `00 0A 0A 01` in every SR3900P driver capture
(`Model::row_prefix`). The middle pair is most likely the dot pitch
in 1/3600 inch (`3600 / 270 = 13.3 → 0x0D`, `3600 / 360 = 10 → 0x0A`).

`dots` is the number of head dots used for the tape width on that
model (section 3.1), as a 16-bit little-endian value, and the bitmap
is exactly `dots / 8` bytes, packed MSB-first. (Upstream read this
field as a single byte followed by a `dots + 8`-bit bitmap; both
readings produce the same bytes for every width below 256 dots. The
SR3900P 36mm capture — `80 01` followed by 48 bytes — settles it as a
16-bit field.)

Implemented in `src/tepra/raster.rs`.

### 2.5 Footer

After the last raster row:

```
0x0C  1B 7B 03 40 40 <checksum> 7D
      \_______________ ESC{ len=3 '@' checksum='@' '}' _______________/
```

- A single `0x0C` byte (unknown purpose; constant in every capture).
- A final `ESC { }` frame with command `@` (0x40) and no parameters
  (its "checksum" therefore just equals `0x40` again, since the sum of
  a single `0x40` byte mod 256 is `0x40`).

Implemented as `TRAILING_BYTE` + `build_command_frame(b'@', &[])` in
`src/tepra/builder.rs`.

### 2.6 Multiple copies

When the SR3900P driver is asked for two copies of a label it emits
one job in which the `L`/`T`/raster/`0x0C` block is repeated once per
copy, with the header and `C`/`D`/`G` sent only once
(`tests/fixtures/sr3900p-*-x2.prn`):

```
header, C, D, G, { L, T, rows..., 0x0C } × copies, '@'
```

The cut mode in `C` applies between copies as well as at the end. This
is what `--copies` / `JobOptions::copies` produce.

## 3. Tape width parameters

Captured and verified byte-for-byte: 6mm, 12mm and 18mm from the SR3700P
driver (printed on hardware), and every width from 4mm to 36mm from the
SR3900P driver (9mm and 36mm in every cut mode and with one and two
copies; 9mm printed on hardware). 4/9/24/36mm on the SR3700P are
extrapolated.

### 3.1 Raster row dot count and bitmap size

| Model   | Tape width | Header byte 8 | Row `dots` (u16 LE) | Bitmap | Source |
|---------|-----------:|:-------------:|:-------------------:|:------:|--------|
| SR3700P | 4mm        | `0x04`        | 40  (`28 00`)        | 5 bytes  | extrapolated |
| SR3700P | 6mm        | `0x06`        | 56  (`38 00`)        | 7 bytes  | captured |
| SR3700P | 9mm        | `0x09`        | 88  (`58 00`)        | 11 bytes | extrapolated |
| SR3700P | 12mm       | `0x0C`        | 112 (`70 00`)        | 14 bytes | captured |
| SR3700P | 18mm       | `0x12`        | 168 (`A8 00`)        | 21 bytes | captured |
| SR3700P | 24mm       | `0x18`        | 192 (`C0 00`)        | 24 bytes | extrapolated: the whole 192-dot head (upstream's `224` cannot fit on the head) |
| SR3700P | 36mm       | `0x24`        | 192 (`C0 00`)        | 24 bytes | extrapolated: the whole head |
| SR3900P | 4mm        | `0x04`        | 48  (`30 00`)        | 6 bytes  | captured |
| SR3900P | 6mm        | `0x06`        | 72  (`48 00`)        | 9 bytes  | captured |
| SR3900P | 9mm        | `0x09`        | 112 (`70 00`)        | 14 bytes | captured |
| SR3900P | 12mm       | `0x0C`        | 144 (`90 00`)        | 18 bytes | captured |
| SR3900P | 18mm       | `0x12`        | 216 (`D8 00`)        | 27 bytes | captured |
| SR3900P | 24mm       | `0x18`        | 288 (`20 01`)        | 36 bytes | captured |
| SR3900P | 36mm       | `0x24`        | 384 (`80 01`)        | 48 bytes | captured: the whole 384-dot head |

On the SR3900P every captured value is `width_mm * 12` rounded up to a
multiple of 8 and capped at the 384-dot head (9mm: 108 → 112; 36mm:
432 → 384), i.e. a printable band of 0.847 × the tape width at 360 dpi
(12mm tape → 10.2mm, 18mm → 15.2mm, 24mm → 20.3mm).

On the SR3700P the three captured values fit `dots = width_mm * 28 / 3`
(0.878 × the tape width at 270 dpi: 5.3 / 10.5 / 15.8mm); the
extrapolated rows apply the same formula rounded up to a multiple of 8
and capped at the 192-dot head.

Note that 9mm tape on the SR3900P and 12mm tape on the SR3700P both use
112 dots (different physical widths).

### 3.2 Header offset 23 ("aux byte")

| Tape width | Value | Source |
|-----------:|------:|--------|
| 4mm        | 18 (`0x12`) | SR3900P capture |
| 6mm        | 3     | SR3700P + SR3900P captures |
| 9mm        | 4     | SR3900P capture |
| 12mm       | 5     | SR3700P + SR3900P captures |
| 18mm       | 6     | SR3700P + SR3900P captures |
| 24mm       | 7     | SR3900P capture |
| 36mm       | 8     | SR3900P capture |

6mm..36mm look like an ordinal into KING JIM's tape lineup (3..=8), but
4mm is 18, so the byte is not simply that. It does not depend on the
model (6/12/18mm agree between the two drivers). Stored as a lookup
table (`TapeWidth::aux_byte` in `src/tepra/mod.rs`).

## 4. Resolution and font size

### 4.1 Dot pitch

| Model   | Head          | Dot pitch (both directions) |
|---------|---------------|-----------------------------|
| SR3700P | 192 dots      | 270 dpi (0.094mm)           |
| SR3900P | 384 dots      | 360 dpi (0.0706mm)          |

Feed-direction dots are square: glyphs in the SR3900P captures have
normal proportions when one raster row is treated as one head dot.
Every millimetre ↔ rows conversion (`--left-margin-mm`,
`--max-tape-length-mm`, the Web UI's label length) uses the model's
dpi (`Model::dpi`, `Tape::mm_to_rows`). Upstream used 208 dpi for these
conversions (see 4.2), which made SR3700P labels about 23% shorter than
the requested length; the SR3900P fixtures (384 dots on 36mm tape)
rule out anything coarser than ~270 dpi.

### 4.2 Point size → pixels

The TEPRA desktop app renders a font size of `pt` points with an em of

```
em_dots = pt * 1.1 * dpi / 72        (APP_PT_SCALE = 1.1 in src/font/mod.rs)
```

i.e. the app's "point" is 1.1 real points. This was measured from the
half-width character pitch, which is exactly 0.5 em in MS Gothic (the
app's default font) and independent of glyph shapes:

| Capture | Model / dpi | Text | Half-width pitch | em | dots per pt | factor |
|---------|-------------|------|-----------------:|---:|------------:|-------:|
| `tests/fixtures/aa.prn` (upstream) | SR3700P, 270 | 36pt "AA" | 74 rows | 148 | 4.11 | 1.097 |
| `tests/fixtures/sr3900p-w*.prn` | SR3900P, 360 | MS Gothic 16pt "TEST 123" | 43.8 rows (T→E→S→T 44/43/45, 2→3 43) | 87.6 | 5.47 | 1.095 |

The cap height in the same captures (104 dots at 36pt/270 dpi, 61 dots
at 16pt/360 dpi) is 0.70 em in both, consistent with MS Gothic.

Upstream's earlier constant (208 "dpi", `px = pt * 208 / 72`) came from
matching the feed-direction width of BIZ UDPGothic's "A" (a proportional
glyph ~0.6 em wide) to the 63-dot-wide MS Gothic "A" (~0.43 em) in the
capture; it made the same nominal point size print about 30% smaller
than the app does.

## 5. Open questions

The following aspects of the protocol remain unresolved. They do not
prevent printing, but would need further captures or hardware
experimentation to pin down:

- **`C` command parameters** (section 2.3): only the three observed
  combinations (full / half / none) are known; what the individual
  bytes mean is not.
- **`D`, `G` command parameters**: constant in every capture.
- **Header block offset 20** (section 2.2): the `12 + 4 * char_count`
  formula is confirmed for 1 and 2 characters only. The SR3900P
  captures of `TEST 123` (8 characters) carry `0x20` (= 12 + 4 × 5),
  so the field counts something other than plain characters (objects?
  runs?). The SR3700P accepted our value for every text tried so far.
- **Header block offsets 9, 32 and the version tag** (section 2.2):
  model/driver dependent; whether the SR3900P firmware requires its
  driver's values is unknown (we emit them per `--model`).
- **SR3700P dot counts for 4/9/24/36mm** (section 3.1): extrapolated;
  no SR3700P driver captures for those widths.
- **Most of the 2174-byte header payload**: mostly zero-filled and
  treated as an opaque template; likely contains fields relevant to
  features not yet exercised (barcodes, print density, …).
- **The `USBC`/`USBS`-like exchange seen in the Windows USBPcap
  capture** (section 1.4): not required for printing, but its actual
  purpose (if any) on real hardware is unexplained.

## 6. References / raw data

- `tests/fixtures/*.prn` — raw job byte streams captured by redirecting
  the Windows driver's output to a `FILE:` port, for various text/tape
  width/font size combinations. Used as golden files in byte-exact
  reconstruction tests.
- `../capture-kit/usbcap_9mm_half.pcapng` — USBPcap capture of the
  SR3900P driver printing one 9mm half-cut label (section 1.6).
- `captures/obtain-tape-width.pcap` — USBPcap capture of the driver
  probing the tape width.
- `src/tepra/` — the Rust implementation described above.
- `examples/probe.rs` — opens the printer over USB and reports its
  device ID / tape info, without printing anything.
- `examples/send_raw.rs` — sends a raw job file (e.g. one produced via
  `--dump`, or one of the `tests/fixtures/*.prn` files above) directly
  to the printer, bypassing font rendering.

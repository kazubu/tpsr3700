# tpsr3700

Linux and macOS driver for the **KING JIM TEPRA PRO SR3700P** label printer.

Print text labels directly from the command line — no Windows, no vendor software required.

> **Japanese / 日本語版は下部にあります。** → [日本語 README](#日本語)

> **Disclaimer:** This project is independent and is **not affiliated with, endorsed by, or supported by KING JIM Co., Ltd.** Please do **not** contact KING JIM regarding this software.

---

## Features

- Print arbitrary text on 4 / 6 / 9 / 12 / 18 / 24 / 36 mm tapes (SR3700P: 6/12/18 mm verified, others extrapolated; SR3900P: every width from driver captures, see `docs/protocol.md` §3)
- SR3700P (270 dpi) and SR3900P (360 dpi) via `--model`
- Full cut / half cut / no cut (`--cut`) and multiple copies per job (`--copies`)
- Cable-marking label layout (`--cable`): print only in the band at the top of the tape
- Auto-detect the loaded tape width by querying the printer
- Auto-select the largest font size that fits the tape
- Multi-line printing via explicit `\n` line breaks — works with or without `--max-tape-length-mm`
- Wrap-to-length: wrap text to a fixed label length, with a configurable maximum line count derived from the tape width (minimum line height: 3 mm)
- Left / center / right alignment per line (within the label length with `--max-tape-length-mm`, otherwise within the widest line of a multi-line text)
- Reliable job submission with `--wait`: wait for the printer to be idle before sending, write the job in chunks the printer can absorb as it prints, and fail rather than claim success when the printer never starts (see `docs/protocol.md` §1.6)
- Print several different labels as one job (`--labels-file`), so the cut mode applies between them
- Dump the raw job bytes to a file (useful for debugging without hardware)
- SVG print preview before sending the job to the printer (shown by default; `--skip-preview` to bypass it)

## Requirements

- Rust toolchain (edition 2024)
- `libusb`:
  - Debian/Ubuntu: `sudo apt-get install libusb-1.0-0-dev`
  - macOS: `brew install libusb`

No font installation is required — **BIZ UDPゴシック Regular** is bundled
directly in the binary. To use a different font, pass `--font <PATH>` with
any `.ttf`/`.otf`/`.ttc` file.

## Build

```sh
cargo build --release
```

The `preview` feature (SVG print preview, see below) is enabled by
default. To build without it, use:

```sh
cargo build --release --no-default-features
```

## Usage

```
tpsr3700 [OPTIONS] <TEXT>
```

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--model <MODEL>` | from the printer, else `sr3700p` | Printer model: `sr3700p` (270 dpi, 192-dot head) or `sr3900p` (360 dpi, 384-dot head). Sets the dot pitch for every mm conversion, the dots per tape width, and model bytes in the job. When omitted (and `TEPRA_MODEL` is unset) the model is taken from the printer's USB device ID (SR3700P → `sr3700p`; SR3900P, SR750 → `sr3900p`). |
| `--tape-mm <MM>` | auto-detect | Tape width in mm: `4`, `6`, `9`, `12`, `18`, `24`, `36`. Omit to query the printer. Widths not captured for the selected model print with a warning (see `docs/protocol.md` §3.1). |
| `--cut <MODE>` | `full` | `full` (cut through), `half` (cut the tape, not the backing paper), or `none`. |
| `--cable` | off | Lay out a cable-marking label: print only inside the label's printable area, measured from the top edge of the tape (24mm: 7.5mm, 36mm: 12.5mm — 0.5mm below the nominal 8mm/13mm, which overflows in practice), leaving the rest clear to wrap around the cable. The head cannot reach the outermost ~1.8mm (24mm) / ~4.4mm (36mm) of the tape, so the text height is that much less again (5.5mm / 7.9mm). |
| `--cable-mm <MM>` | per width | Height of the cable label's printable area measured from the top edge of the tape, overriding the default. Requires `--cable`. |
| `--copies <N>` | `1` | Print N identical labels in one job, cut apart per `--cut`. |
| `--labels-file <PATH>` | — | Print several different labels as one job: the file holds the labels separated by ASCII RS (0x1E). Replaces TEXT. Each label gets `--copies` copies; `--cut` applies between all of them. |
| `--feed-mm <MM>` | — | Feed that much blank tape instead of printing, then cut per `--cut`. A full/half cut also ejects the fixed head-to-cutter lead (~9mm on an SR750) as a separate leading scrap, so feed-and-cut yields that scrap plus the fed piece; feed without a cut to avoid the scrap. Replaces TEXT. |
| `--size <PT>` | auto | Font size in points (as shown in the TEPRA app, e.g. `36`). Omit for the largest size that fits. |
| `--left-margin-mm <MM>` | `4.0` | Blank feed margin (mm) added before the printed content (left side as the label exits the printer). 1mm more than the right by default because the SR3900P trims the leading edge slightly. |
| `--right-margin-mm <MM>` | `3.0` | Blank feed margin (mm) added after the printed content (right side). |
| `--top-margin <DOTS>` | `2` | Non-printing margin at the top edge of the tape, in dots. |
| `--bottom-margin <DOTS>` | `0` | Non-printing margin at the bottom edge of the tape, in dots. |
| `--max-tape-length-mm <MM>` | — | Total label length in mm (including margins). Enables multi-line wrapping when the text does not fit in one line. |
| `--align <ALIGN>` | `left` | Per-line alignment: `left`, `center`, or `right`. Within the label length with `--max-tape-length-mm`, otherwise within the widest line of a multi-line text (no effect on single-line text). |
| `--font <PATH>` | bundled BIZ UDPゴシック | Path to a `.ttf`/`.otf`/`.ttc` font file. Overrides the bundled font. |
| `--dump <PATH>` | — | Write the raw job bytes to a file (in addition to, or instead of, printing). |
| `--no-print` | — | Skip sending the job to the printer. Useful with `--dump`. |
| `--skip-preview` | — | Skip the SVG print preview and print immediately. A preview is shown by default (requires the `preview` build feature, enabled by default). |
| `--wait` | off | Wait for the printer around the job: block until it is idle before sending, then poll the status until it is idle again. Fails if the printer never reported itself busy, which means it did not take the job. Status changes go to stderr. |
| `--preview-out <PATH>` | — | Write an SVG preview of the job to this path and exit without printing or prompting. Rendered from the raster rows that would be sent. For web front ends. |
| `--detect-tape` | — | Query the loaded tape width, print one line of JSON on stdout, and exit: `{"tape_mm": 18, "index": 4, "supported": true, "device_id": "...", "model": "sr3900p"}`. |
| `--printer-status` | — | Print the printer's current state and exit, without printing: the raw 8-byte vendor status (activity byte and tape index) and the printer-class port status byte. Diagnostic only. |
| `--line-ratios <R,R>` | — | Per-line font size ratios for multi-line text (e.g. `7,3`). The count must equal the number of lines. See [Multi-line rules](#multi-line-rules). |
| `--frame` | off | Draw a rectangular frame around the printed content, hugging the ink with ~1mm of padding. Composes with multi-line text, `--line-ratios`, `--align` and wrapping, and appears in previews. |
| `--frame-width <DOTS>` | `2` | Frame line thickness in dots (1-3). Only with `--frame`. |
| `--frame-mm <MM>` | hug the text | Frame width in mm along the label (the feed direction), centred on the label, so labels of the same length get an identical box however long their text is. Clamped to the label. Only with `--frame`. |
| `--vid <HEX>` / `--pid <HEX>` | `0d8a` / `0103` | USB vendor/product ID override, in hex. Default to `TEPRA_USB_VID` / `TEPRA_USB_PID`, else the SR3700P's IDs. Find them with `lsusb`. |
| `--send-header` | off | Also send the 2179-byte header block. The Windows driver does not, and sending it makes the printer stall ~20s before printing. Experiments only. |
| `--no-soft-reset` | off | Skip the printer-class SOFT_RESET normally issued right before the job. Experiments only. |

### Examples

```sh
# Print "Hello" on the loaded tape (width auto-detected)
tpsr3700 "Hello"

# Print on 12 mm tape, explicit font size
tpsr3700 --tape-mm 12 --size 24 "Hello"

# Two lines with \n — no --max-tape-length-mm needed; font size is
# automatically halved so each line fits its band
tpsr3700 --tape-mm 18 "Line1\nLine2"

# Multi-line: wrap to a 50 mm label, center-aligned
tpsr3700 --tape-mm 18 --max-tape-length-mm 50 --align center "Hello World"

# Explicit line break with \n inside a fixed-length label
tpsr3700 --tape-mm 18 --max-tape-length-mm 40 "Line1\nLine2"

# Dump job bytes without printing (useful for debugging)
tpsr3700 --tape-mm 12 --no-print --skip-preview --dump job.prn "Test"
```

### Print preview

By default, before sending a job to the printer, an SVG preview file is
written to the system temp directory and opened with the OS default viewer
when possible. It shows exactly what will be printed — the feed direction
runs left-to-right (as the label exits the printer), and the non-printing
top/bottom/left/right margins are shaded gray so the printable area is easy
to spot.

- Confirm in the terminal with `y` / `yes` to send the job to the printer.
- Press Enter or type anything else to abort without printing.
- Pass `--skip-preview` to print immediately without showing the preview.
- The preview requires the crate to be built with the `preview` Cargo
  feature, which is **enabled by default**. If built without it (`cargo
  build --no-default-features`), you must always pass `--skip-preview`.

### Multi-line rules

- The minimum height of one printed line is **3 mm**.
  A 6 mm tape fits at most **2 lines**; 12 mm fits 4; 18 mm fits 6.
- **`\n` in the text always forces a line break**, regardless of whether `--max-tape-length-mm` is set.
  Each line is rendered into an equal vertical band.
- When `--size` is omitted and the text contains `\n`, the font size is automatically divided by the number of lines so each band gets the largest size that fits.
- When `--max-tape-length-mm` is given and the text overflows one line, the tool automatically increases the line count (up to the tape's maximum) until everything fits, wrapping character by character.
- `--align` applies to each line independently (within the label length, or within the widest line when no length is given).

## USB permissions (Linux)

The tool communicates with the printer via libusb. On most Linux systems, regular users do not have **read/write** access to USB device nodes by default. Without it you will get a "permission denied" error when running the tool.

> **macOS:** IOKit grants userspace access to USB devices automatically — no extra configuration is needed.

The recommended fix on Linux is a persistent udev rule that grants read/write access to members of the `lp` group. Create the file:

```
# /etc/udev/rules.d/99-tepra.rules
SUBSYSTEM=="usb", ATTRS{idVendor}=="0d8a", ATTRS{idProduct}=="0103", GROUP="lp", MODE="0660"
```

`GROUP="lp"` restricts access to the `lp` group and `MODE="0660"` grants read/write to that group only (safer than opening it to all users).

Or create it in one command:

```sh
echo 'SUBSYSTEM=="usb", ATTRS{idVendor}=="0d8a", ATTRS{idProduct}=="0103", GROUP="lp", MODE="0660"' \
    | sudo tee /etc/udev/rules.d/99-tepra.rules
```

Then add your user to the `lp` group, reload the rules, and re-trigger the device:

```sh
sudo usermod -aG lp $USER
sudo udevadm control --reload && sudo udevadm trigger
```

After that, unplug and re-plug the printer and start a new login session (or run `newgrp lp` in the current shell). The rule persists across reboots.

## Project layout

```
src/
  main.rs           CLI entry point (clap argument parsing)
  font/mod.rs       Font rendering: text → raster rows (fontdue)
  preview.rs        SVG print preview (`preview` feature)
  tepra/
    mod.rs          TapeWidth enum and tape geometry constants
    builder.rs      Assembles the full print job byte stream
    frame.rs        ESC { } command frame encoder
    header.rs       Fixed 2179-byte job header builder
    raster.rs       Single raster row encoder
  usb/              libusb/rusb transport (bulk write + tape detection)
assets/
  header_18mm_template.bin   Base template for the job header
captures/           Raw USB capture of the tape-width probe (.pcap)
docs/protocol.md    Protocol documentation
src/font/
  BIZUDPGothic-Regular.ttf   BIZ UDPゴシック Regular, bundled in the binary
  OFL.txt                    Font license (SIL Open Font License 1.1)
tests/fixtures/     Golden .prn files used in byte-exact reconstruction tests
```

## Running the tests

```sh
cargo test
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

---

<a name="日本語"></a>

# tpsr3700（日本語）

Linux・macOS 用 **KING JIM TEPRA PRO SR3700P** ラベルプリンタードライバーです。

Windows も純正ソフトも不要で、コマンドラインから直接テキストラベルを印刷できます。

> **免責事項:** 本プログラムは KING JIM 株式会社とは無関係です。本ソフトウェアについて KING JIM 社へお問い合わせをしないでください。

---

## 機能

- 4 / 6 / 9 / 12 / 18 / 24 / 36 mm テープへの任意テキスト印刷（SR3700P: 6/12/18 mm は実機確認済み、他は推定。SR3900P: 全幅が公式ドライバのキャプチャ由来。`docs/protocol.md` §3）
- `--model` で SR3700P（270 dpi）/ SR3900P（360 dpi）を切り替え
- フルカット / ハーフカット / カットなし（`--cut`）と 1 ジョブ複数枚（`--copies`）
- プリンターへの問い合わせによるテープ幅自動検出
- テープ幅に収まる最大フォントサイズの自動選択
- **複数行印刷**: テキスト中の `\n` で改行 — `--max-tape-length-mm` の有無に関わらず使用可能
- ラベル長指定による折り返し（テープ幅から最大行数を自動計算、最低行高さ 3 mm）
- 行ごとの左寄せ / 中央揃え / 右寄せ（`--max-tape-length-mm` 使用時）
- ケーブル表示ラベルの配置（`--cable`）: テープ上端の印刷エリアだけに印字
- `--labels-file` で内容の異なる複数ラベルを 1 ジョブとして印刷（カット設定がラベル間にも効く）
- `--wait` による確実なジョブ送信: 送信前にプリンタが idle になるのを待ち、プリンタが印刷しながら受け取れる大きさに分けて書き込み、動作開始を確認できなければ成功と報告せずエラーにする（`docs/protocol.md` §1.6）
- 生のジョブバイト列をファイルにダンプ（ハードウェアなしでのデバッグに便利）
- 印刷前の SVG プレビュー表示（デフォルトで表示。`--skip-preview` で無効化可能）

## 必要なもの

- Rust ツールチェーン（edition 2024）
- `libusb`:
  - Debian/Ubuntu: `sudo apt-get install libusb-1.0-0-dev`
  - macOS: `brew install libusb`

フォントのインストールは不要です。**BIZ UDPゴシック Regular** をバイナリに直接埋め込んでいます。
別のフォントを使いたい場合は `--font <PATH>` で `.ttf`/`.otf`/`.ttc` ファイルを指定してください。

## ビルド

```sh
cargo build --release
```

`preview` フィーチャー（SVG 印刷プレビュー、後述）はデフォルトで有効です。無効にしてビルドする場合は以下を使用してください:

```sh
cargo build --release --no-default-features
```

## 使い方

```
tpsr3700 [オプション] <テキスト>
```

### オプション一覧

| オプション | デフォルト | 説明 |
|-----------|-----------|------|
| `--model <MODEL>` | プリンタから判定、無ければ `sr3700p` | 機種: `sr3700p`（270 dpi、192 ドット）か `sr3900p`（360 dpi、384 ドット）。mm 換算の解像度、テープ幅ごとのドット数、ジョブ内の機種バイトが切り替わる。省略時（`TEPRA_MODEL` も未設定）は USB デバイス ID の機種名から判定（SR3700P → `sr3700p`、SR3900P / SR750 → `sr3900p`）。 |
| `--tape-mm <MM>` | 自動検出 | テープ幅（mm）: `4`・`6`・`9`・`12`・`18`・`24`・`36`。省略時はプリンターに問い合わせ。選択した機種でキャプチャ未取得の幅は警告付きで印刷（`docs/protocol.md` §3.1）。 |
| `--cut <MODE>` | `full` | `full`（フルカット）、`half`（ハーフカット: 台紙を残す）、`none`（カットしない）。 |
| `--cable` | 無効 | ケーブル表示ラベルの配置にする。テープ上端から測った印刷エリア内だけに印字し（24mm は 7.5mm、36mm は 12.5mm。公称 8mm / 13mm では実機ではみ出すため 0.5mm 控えた値）、残りはケーブルに巻く透明部分として空ける。ヘッドはテープ最外部（24mm で約 1.8mm、36mm で約 4.4mm）に届かないため、文字が入る高さはさらにその分狭い（5.5mm / 7.9mm）。 |
| `--cable-mm <MM>` | 幅ごとの既定 | ケーブルラベルの印刷可能エリアの高さ（テープ上端から）を上書きする。`--cable` と併用。 |
| `--copies <N>` | `1` | 同じラベルを 1 ジョブで N 枚印刷（`--cut` の設定で切り分け）。 |
| `--labels-file <PATH>` | — | 内容の異なる複数ラベルを 1 ジョブで印刷。ファイル内のラベルは ASCII RS (0x1E) 区切り。TEXT の代わりに指定。各ラベルに `--copies`、ラベル間に `--cut` が効く。 |
| `--feed-mm <MM>` | — | 印刷せずに指定長の空白テープを送って `--cut` でカット。フル／ハーフカットではヘッド〜カッター間（SR750 で約 9mm）が先頭の切れ端として別に排出されるため、送ってカットすると切れ端＋指定長の 2 枚になる。切れ端を出したくなければカットなしで送る。TEXT の代わりに指定。 |
| `--size <PT>` | 自動 | フォントサイズ（ポイント、TEPRA アプリの表示と同じ。例: `36`）。省略時はテープに収まる最大サイズ。 |
| `--left-margin-mm <MM>` | `4.0` | 印字内容の前（ラベルがプリンターから出る側＝左端）に追加する余白（mm）。SR3900P は先端側が少し短く出るため、既定で右より 1mm 大きい。 |
| `--right-margin-mm <MM>` | `3.0` | 印字内容の後（右端）に追加する余白（mm）。 |
| `--top-margin <DOTS>` | `2` | テープ上端の非印字マージン（ドット数）。 |
| `--bottom-margin <DOTS>` | `0` | テープ下端の非印字マージン（ドット数）。 |
| `--max-tape-length-mm <MM>` | — | ラベル全長（mm、マージン込み）。指定すると、1 行に収まらないテキストを複数行に折り返す。 |
| `--align <ALIGN>` | `left` | 各行の横方向揃え: `left`（左寄せ）・`center`（中央）・`right`（右寄せ）。`--max-tape-length-mm` 指定時はラベル長の中で、未指定時は複数行の最長行の中で揃える（1 行のみの場合は効果なし）。 |
| `--font <PATH>` | バンドル済み BIZ UDPゴシック | フォントファイルのパス（`.ttf`/`.otf`/`.ttc`）。バンドルフォントを上書きします。 |
| `--dump <PATH>` | — | 生のジョブバイト列をファイルに書き出す（印刷と併用可）。 |
| `--no-print` | — | プリンターへの送信をスキップ。`--dump` と組み合わせて使用。 |
| `--skip-preview` | — | SVG 印刷プレビューを表示せず、すぐに印刷する。デフォルトではプレビューを表示する（`preview` ビルドフィーチャーが必要。デフォルトで有効）。 |
| `--wait` | 無効 | ジョブの前後でプリンタを待つ。送信前は idle になるまで待ち（印刷中やカット中に送るとジョブが捨てられるため）、送信後は idle に戻るまで状態を見る。プリンタが一度も動作中にならなかった場合は、ジョブを受け取っていないとみなしてエラーにする。状態の変化は標準エラー出力に出る。 |
| `--preview-out <PATH>` | — | ジョブの SVG プレビューを指定パスに書き出して終了する（印刷も確認プロンプトもしない）。実際に送るラスタ行から描画する。Web フロントエンド用。 |
| `--detect-tape` | — | 装着テープ幅を問い合わせ、JSON 1 行を標準出力に出して終了する: `{"tape_mm": 18, "index": 4, "supported": true, "device_id": "...", "model": "sr3900p"}`。 |
| `--printer-status` | — | 印刷せずにプリンタの現在の状態を出して終了する。8 バイトの生ステータス（動作状態バイトとテープインデックス）と、プリンタクラスのポート状態バイト。診断用。 |
| `--line-ratios <R,R>` | — | 複数行テキストの行ごとの文字サイズ比率（例: `7,3`）。個数は行数と一致する必要がある。[複数行印刷のルール](#複数行印刷のルール)を参照。 |
| `--frame` | 無効 | 印字内容の周囲に枠線を描く（インクの外周に約 1mm の余白を取る）。複数行・`--line-ratios`・`--align`・折り返しと併用でき、プレビューにも出る。 |
| `--frame-width <DOTS>` | `2` | 枠線の太さ（ドット、1〜3）。`--frame` と併用。 |
| `--frame-mm <MM>` | 文字を囲む | 枠線の横幅（mm、送り方向）。ラベルの中央にその幅で描くので、同じ長さのラベルなら文字の長さに関わらず同じ大きさの枠になる。ラベル長を超える指定は切り詰める。`--frame` と併用。 |
| `--vid <HEX>` / `--pid <HEX>` | `0d8a` / `0103` | USB の vendor/product ID を 16 進で上書きする。未指定なら `TEPRA_USB_VID` / `TEPRA_USB_PID`、それも無ければ SR3700P の値。`lsusb` で確認する。 |
| `--send-header` | 無効 | 2179 バイトのヘッダブロックも送る。公式ドライバは送らず、送ると印刷開始が 20 秒ほど遅れる。実験用。 |
| `--no-soft-reset` | 無効 | ジョブ直前に送るプリンタクラスの SOFT_RESET を省く。実験用。 |

### 使用例

```sh
# テープ幅を自動検出して "Hello" を印刷
tpsr3700 "Hello"

# 12 mm テープ、フォントサイズ 24pt を指定して印刷
tpsr3700 --tape-mm 12 --size 24 "Hello"

# \n で 2 行印刷 — --max-tape-length-mm 不要
# --size 省略時はフォントサイズを行数で自動分割
tpsr3700 --tape-mm 18 "1行目\n2行目"

# 50 mm のラベルに複数行・中央揃えで折り返し印刷
tpsr3700 --tape-mm 18 --max-tape-length-mm 50 --align center "Hello World"

# ラベル長を指定して \n で明示的に改行
tpsr3700 --tape-mm 18 --max-tape-length-mm 40 "1行目\n2行目"

# 印刷せずにジョブバイト列をファイルにダンプ（デバッグ用）
tpsr3700 --tape-mm 12 --no-print --skip-preview --dump job.prn "Test"
```

### 印刷プレビュー

デフォルトでは、プリンターにジョブを送信する前に SVG プレビューファイルをシステムの一時ディレクトリへ書き出し、可能であれば OS の既定ビューアで開きます。実際に印刷される内容がそのまま表示されます。送り方向は左から右（ラベルがプリンターから出てくる方向）で、上下左右の非印字マージン部分はグレーで表示されるため、印字可能な範囲が一目でわかります。

- 端末で `y` / `yes` を入力すると、プリンターにジョブを送信します。
- Enter のみ、またはそれ以外の入力では印刷を中止します。
- `--skip-preview` を指定すると、プレビューを表示せずに即座に印刷します。
- プレビュー機能を使うには `preview` Cargo フィーチャー（**デフォルトで有効**）が必要です。無効にしてビルドした場合（`cargo build --no-default-features`）は、常に `--skip-preview` を指定する必要があります。

### 複数行印刷のルール

- 1 行の最低高さは **3 mm** です。
  6 mm テープは最大 **2 行**、12 mm は 4 行、18 mm は 6 行まで印字できます。
- **テキスト中の `\n` は常に改行**として扱われます（`--max-tape-length-mm` の有無を問いません）。
  各行はテープの縦方向を等分したバンドに描画されます。
- `--size` を省略した場合、`\n` の行数に応じてフォントサイズを自動的に分割し、各バンドに収まる最大サイズを使用します。
- `--max-tape-length-mm` を指定した場合、1 行に収まらないテキストは自動的に行数を増やして（最大行数まで）文字単位で折り返します。`--line-ratios` と併用した場合は折り返さず、指定長の固定長ラベルに各行を `--align` で揃えます（最長行が余白の内側に収まらなければエラー）。
- `--align` は各行に個別に適用されます（ラベル長の中、または長さ未指定なら最長行の中で揃えます）。

## USB アクセス権の設定（Linux）

このツールは libusb 経由でプリンターと通信します。多くの Linux 環境では、一般ユーザーはデフォルトで USB デバイスノードへの**読み書きアクセス権**を持っていません。権限がない場合、ツール実行時に "permission denied" エラーが発生します。

> **macOS:** IOKit がユーザー空間への USB アクセスを自動で許可するため、追加設定は不要です。

推奨される対処法は Linux での udev ルール設定です。`lp` グループのメンバーに読み書きアクセスを付与する永続的なルールを作成してください：

```
# /etc/udev/rules.d/99-tepra.rules
SUBSYSTEM=="usb", ATTRS{idVendor}=="0d8a", ATTRS{idProduct}=="0103", GROUP="lp", MODE="0660"
```

`GROUP="lp"` でアクセスを `lp` グループに限定し、`MODE="0660"` でそのグループにのみ読み書き権限を付与します（全ユーザーに開放するより安全です）。

次のコマンド 1 行で作成することもできます：

```sh
echo 'SUBSYSTEM=="usb", ATTRS{idVendor}=="0d8a", ATTRS{idProduct}=="0103", GROUP="lp", MODE="0660"' \
    | sudo tee /etc/udev/rules.d/99-tepra.rules
```

続けて、自分のユーザーを `lp` グループに追加し、ルールを反映してデバイスを再トリガーします：

```sh
sudo usermod -aG lp $USER
sudo udevadm control --reload && sudo udevadm trigger
```

プリンターを抜き差しし、新しいログインセッションを開始してください（または現在のシェルで `newgrp lp` を実行）。ルールは再起動後も有効です。

## プロジェクト構成

```
src/
  main.rs           CLI エントリーポイント（clap 引数解析）
  font/mod.rs       フォントレンダリング: テキスト → ラスタ行（fontdue 使用）
  preview.rs        SVG 印刷プレビュー（`preview` フィーチャー）
  tepra/
    mod.rs          TapeWidth 列挙型とテープのジオメトリ定数
    builder.rs      印刷ジョブのバイト列を組み立て
    frame.rs        ESC { } コマンドフレームエンコーダー
    header.rs       固定長 2179 バイトのジョブヘッダービルダー
    raster.rs       ラスタ行エンコーダー
  usb/              libusb/rusb トランスポート（バルク書き込みとテープ幅検出）
assets/
  header_18mm_template.bin   ジョブヘッダーのベーステンプレート
captures/           テープ幅検出時の USB キャプチャ（.pcap）
docs/protocol.md    プロトコル仕様書
src/font/
  BIZUDPGothic-Regular.ttf   バイナリに埋め込まれる BIZ UDPゴシック Regular
  OFL.txt                    フォントのライセンス（SIL Open Font License 1.1）
tests/fixtures/     バイト完全一致テスト用ゴールデン .prn ファイル
```

## テストの実行

```sh
cargo test
```

## ライセンス

[Apache License, Version 2.0](LICENSE) の下で提供されます。

# Gource

<https://gource.io>

Gource visualizes source-control history as an animated directory tree. This
checkout contains the preserved C++ application and a native Rust successor.
The Rust application is the current implementation described below; the
legacy C++ build remains available and is documented in [`INSTALL`](INSTALL).

## Rust native application

The successor is a six-crate Cargo workspace:

- `gource-core` — identifiers, events, hierarchy, replay configuration;
- `gource-ingest` — bounded custom-log and local Git ingestion, indexing, and
  the optional history cache;
- `gource-sim` — fixed-step playback, layout, contributors, and snapshots;
- `gource-render` — native `wgpu` rendering and WGSL;
- `gource-export` — deterministic frame scheduling, readback, PNG/FFmpeg sinks;
- `gource-app` — the native window, CLI, diagnostics, and command dispatch.

The workspace is pinned to **Rust 1.98.1** in `rust-toolchain.toml` and uses
`wgpu`, `winit`, and `egui`. A compatible native graphics adapter and driver
are required for `view` and for non-empty `export`; headless means that no
window is created, not that a GPU or graphics driver is unnecessary.

Target wrappers are provided for Windows, macOS, and Linux. A successful build
or a smoke run on one host does not establish support for every OS, GPU,
driver, architecture, or backend. Target-specific evidence and the current
packaging boundary are recorded in [`docs/releasing.md`](docs/releasing.md).

## Build

Install the pinned toolchain and build the native binary:

```sh
rustup toolchain install 1.98.1 --profile minimal --no-self-update
cargo build --locked --release --package gource-app
```

The resulting binary is `target/release/gource-app` (or
`target/release/gource-app.exe` on Windows). A development command is:

```sh
cargo run --locked -p gource-app -- --help
```

No command downloads fonts, images, shaders, Git data, or FFmpeg at runtime.

Runtime prerequisites are:

- a native `wgpu` adapter/driver for `view` and non-empty frame export;
- a local `git` executable only when the input is a repository directory; and
- a user-provided `ffmpeg` executable on `PATH` only when `export --video` is
  selected.

FFmpeg is not bundled, fetched, or invoked through a shell. If it is absent,
video export fails before publishing a final file. Frame-directory export does
not require FFmpeg.

## Dogfood

Run the committed end-to-end local workflow against this repository:

```sh
./scripts/dogfood.sh
```

The script runs formatting, strict Clippy, the workspace tests, and a locked
release build. It ingests the selected Git repository through serial and
parallel replay over a bounded diagnostic window, validates a persistent-cache
miss and hit using the deterministic custom-log fixture, and exports that
fixture as 130 PNG reference frames. It then renders the selected repository's
complete checked-out `HEAD` history to
`target/dogfood/run.*/repository-history.mkv`, verifies the FFV1 stream with
`ffprobe`, and invokes the native package wrapper for supported Unix hosts.
Each run keeps all evidence in its unique `target/dogfood/run.*` directory.

The full-history video defaults to 1280x720, 30 FPS, `0.1` seconds per
repository day, track camera, and deterministic seed 31. Override
`DOGFOOD_VIDEO_VIEWPORT`, `DOGFOOD_VIDEO_FRAME_RATE`, or
`DOGFOOD_VIDEO_SECONDS_PER_DAY` when a different output size, cadence, or
duration is required. `DOGFOOD_THREADS` controls diagnostics, while
`DOGFOOD_HISTORY_SECONDS` changes only the bounded diagnostic replay window.
The video export itself always covers the first through final event.

Pass another local Git repository as the sole positional argument. Add `--view`
to launch the interactive viewer after every automated check passes:

```sh
./scripts/dogfood.sh --view /path/to/local/repository
```

The Unix dogfood runner requires Bash, Cargo, Git, Python 3, FFmpeg, and
`ffprobe`, plus a working native `wgpu` adapter. It never fetches repository
history or runtime assets.

## Commands

The Rust binary has three subcommands. Legacy Gource flags are not accepted by
this CLI; an unknown option is an error rather than an ignored compatibility
alias.

```text
gource-app [--config FILE] view [options]
gource-app [--config FILE] export [options]
gource-app [--config FILE] diagnose [options]
```

Run `gource-app --help` or `gource-app <command> --help` for the generated
syntax. The most useful common options are:

| Option | Meaning |
| --- | --- |
| `--input PATH` | Read a regular custom-log file, `-` for finite stdin, or a local Git repository directory. |
| `--cache-dir PATH` | Opt in to the private persistent indexed-history cache. Omitted means no cache. |
| `--cache-bytes BYTES` | Finite aggregate cache-entry quota (default 4 GiB when caching is enabled). |
| `--viewport WIDTHxHEIGHT` | Physical viewport; default `1280x720`, with a 32,768-pixel edge limit. |
| `--width PIXELS --height PIXELS` | Alternate viewport spelling; both values are required. |
| `--seconds-per-day SECONDS` | Repository time represented by one simulated day (default `10`). |
| `--realtime` / `--no-realtime` | Select or explicitly disable real-time repository playback. |
| `--auto-skip-seconds SECONDS` | Skip idle repository gaps longer than the value (default `3`). Alias: `--auto-skip`. |
| `--time-scale FACTOR` | Multiply canonical playback speed (default `1`). |
| `--file-idle-seconds SECONDS` | Keep deleted files visible for the configured duration; unset or `0` disables expiry. Alias: `--file-idle`. |
| `--camera overview|track` | Select the automatic camera policy. |
| `--seed SEED` | Select the deterministic layout seed (default `31`). |
| `--filter GLOB` | Keep events whose path matches a glob. Repeat to replace the configured filter list; any match is retained. |
| `--max-record-bytes BYTES` | Maximum physical record size (default 1 MiB). |
| `--max-input-bytes BYTES` | Maximum finite input size (default 8 GiB). |
| `--max-path-bytes BYTES` | Maximum normalized path size (default 64 KiB). |
| `--max-contributor-bytes BYTES` | Maximum contributor size (default 4 KiB). |
| `--max-path-components COUNT` | Maximum lexical path depth (default 256). |
| `--max-events COUNT` | Maximum indexed events (the app default is `u64::MAX`; choose a smaller finite bound when required). |
| `--working-memory-bytes BYTES` | Total parser/index working-memory budget (default 128 MiB). |
| `--working-disk-bytes BYTES` | Temporary external-sort disk budget (default 16 GiB). |
| `--run-fan-in COUNT` | Maximum sorted runs merged at once (default `32`). |

All limits must be non-zero. Limit failures are errors, not EOF or silent
truncation. The parser consumes a finite source to EOF before publishing an
indexed history.

### `view`

Open the interactive native window:

```sh
./target/release/gource-app view \
  --input tests/fixtures/visual-hierarchy-activity.log \
  --viewport 1280x720
```

The current input controls are deliberately small and deterministic:

- `Space` or `P` pauses/resumes;
- `Right Arrow` or `N` advances to the next event; `Left Arrow` seeks one
  repository second backward;
- `+`/`=` and `-` increase or halve playback rate;
- keypad `+`/`-` zoom the presentation camera;
- `O`, `T`, and `M` select overview, track, and manual camera modes;
- `R` resets the camera; `Esc` or `Q` quits;
- left-click selects a scene point, right-drag pans, and the mouse wheel zooms.

### `export`

A frame export writes a new directory of PNG frames and a `manifest.toml`; it
does not require FFmpeg:

```sh
./target/release/gource-app export \
  --input tests/fixtures/single-event.log \
  --output /tmp/gource-frames \
  --viewport 320x180 \
  --frame-rate 60 \
  --start 0 \
  --end 1
```

`--output` is required. `--frame-rate` accepts an exact FPS or a rational such
as `30000/1001` (alias: `--fps`). `--start` and `--end` are non-negative
repository timestamps; the end is exclusive. If `--end` is omitted, the app
uses one second after the last event. Existing output paths are rejected, and
failed or cancelled exports remove their private staging artifact.

Use `--video` to select the supervised FFmpeg sink:

```sh
./target/release/gource-app export \
  --input tests/fixtures/single-event.log \
  --output /tmp/gource.mkv \
  --video \
  --frame-rate 60 \
  --start 0 \
  --end 1
```

The sink passes raw RGBA frames to `ffmpeg` through stdin and requests FFV1
level 3, intra frames, Matroska output, and the configured dimensions/rate. It
uses a three-frame queue, a three-frame byte cap, a 64 KiB stderr bound, and a
30-second deadline. Backpressure is applied instead of dropping frames. A
non-zero exit, timeout, cancellation, or writer failure kills and reaps the
child and leaves no final video. See [`docs/export.md`](docs/export.md) and
[`docs/security.md`](docs/security.md).

FFV1 is the Rust fork's deterministic lossless archival choice. The original
C++ Gource does not select FFV1 or directly manage an encoder: its
`--output-ppm-stream` option emits a PPM frame stream or file for the user to
pipe into an external encoder.

### `diagnose`

`diagnose` parses/indexes once and measures a complete serial replay and a
bounded parallel replay without creating a window or GPU context:

```sh
./target/release/gource-app diagnose \
  --input tests/fixtures/single-event.log \
  --threads 1
```

It prints one JSON object to stdout with `ingest_ms`, `serial_ms`,
`parallel_ms`, `events`, `final_tick`, `snapshots_equal`, and
`configured_threads`. The command exits successfully only when serial and
parallel snapshots compare equal. `--end SECONDS` (alias: `--target`) limits
both replays to a repository timestamp. This is a replay diagnostic, not an
end-to-end renderer or encoder benchmark.

The measured CPU replay evidence and its exact workload/host description are in
[`docs/performance.md#measured-cpu-replay-evidence-2026-09-13`](docs/performance.md#measured-cpu-replay-evidence-2026-09-13).

## Input formats and local Git directories

The first custom-log format is finite and pipe-delimited:

```text
timestamp|contributor|A|path
timestamp|contributor|M|path|#rrggbb
timestamp|contributor|D|path/
```

Timestamps may be signed epoch seconds or the documented UTC/offset date
forms. A record has exactly four fields, or five fields with a six-digit
hexadecimal colour. Input is UTF-8; paths are lexical repository names using
`/`, not host filesystem paths. Records are globally ordered by
`(timestamp, source_sequence)`, so equal timestamps retain source order and
duplicates remain events. Empty contributors normalize to `Unknown`, and an
empty action normalizes to `A`; malformed records, traversal components, and
unsupported actions fail with a bounded diagnostic.

A directory supplied through `--input` selects the built-in local Git adapter:

```sh
./target/release/gource-app view --input /path/to/local/repository
./target/release/gource-app export \
  --input /path/to/local/repository \
  --output /tmp/repository-frames \
  --start 0 --end 60
```

The adapter accepts a local directory, validates and snapshots one revision,
then invokes the local `git` executable with a fixed argument vector and raw
NUL-delimited output. It disables pagers, replacement objects, external diffs,
text conversion, notes, signatures, rename detection, and lazy fetch. The
repository path and Git ref data are arguments, never shell text. Git is not
bundled or downloaded, and the adapter performs no remote fetch. A missing Git
executable, non-repository directory, malformed output, timeout, cancellation,
limit failure, or unsuccessful child exits without publishing a partial
history. See [`docs/security.md`](docs/security.md).

## Configuration and cache

`--config FILE` loads one TOML file. If `--config` is omitted,
`GOURCE_CONFIG` may select the file. Precedence is built-in defaults, TOML,
known `GOURCE_*` environment values, then explicit command-line options. Paths
inside the file are interpreted relative to the process working directory; the
process directory is never changed. Command-specific `[view]`, `[export]`,
and `[diagnose]` tables override shared keys for that command. There is no
legacy C++ `.conf`/`.ini` loader and no `--load-config` or `--save-config`
option in the Rust CLI. See [`docs/configuration.md`](docs/configuration.md)
for the complete key and environment table.

The cache is opt-in and applies to regular custom-log files only. `--cache-dir`
creates or opens a private directory; stdin and Git-directory inputs are parsed
without persistent cache lookup. The default aggregate quota is 4 GiB, and
`--cache-bytes` must be finite and non-zero. Cache entries are keyed by exact
input bytes, ingest schema/options, and limits; complete entries are checksummed,
validated before use, and atomically staged before publication. Cache roots are
owner-private (mode `0700`/equivalent ACL) and entry files are private. Corrupt
or stale entries are discarded as misses. See [`docs/cache.md`](docs/cache.md)
for caps, eviction, privacy, and failure behavior.

## Native packaging

The repository includes target-qualified packaging scripts. They build with the
pinned toolchain, run packaged `--help` and `diagnose` smokes, verify required
archive entries, and emit a SHA-256 sidecar:

```sh
bash packaging/linux-x86_64.sh --output-dir dist
bash packaging/macos.sh --target x86_64-apple-darwin --output-dir dist
bash packaging/macos.sh --target arm64-apple-darwin --output-dir dist
# Windows PowerShell:
.\\packaging\\windows-x86_64.ps1 -OutputDirectory dist
```

A native archive contains `bin/gource-app`, `COPYING`,
`THIRD_PARTY_NOTICES`, this README, `data/gource.style`, the font notice,
`data/gource.1`, and the single-event fixture. It does not bundle FFmpeg,
Git, or the unresolved FreeSans/sprite assets. Packaging smoke proves the
archive and CPU diagnostic are self-contained; it does not prove interactive
window behavior, GPU compatibility, or video export on every target. Read
[`docs/releasing.md`](docs/releasing.md) before publishing an archive.

## Visual reference and evidence limits

The corrected headless reference export is kept at
`target/gource-smoke-frames-2/`. The canonical reference manifest is
`tests/reference/manifest.toml`; it pins
`tests/fixtures/visual-hierarchy-activity.log`, 8 events, `320x180`, realtime
playback, 2 FPS, repository range `[0,65)`, overview camera, auto-skip `3`,
time-scale `1`, seed `31`, and 130 frames. Review points are frames 4, 64,
and 129 on the recorded llvmpipe Vulkan adapter. They are visual evidence for
that host/configuration, not a cross-GPU or cross-platform support claim.
Same-backend comparison uses `max_channel_abs_delta = 8` and
`max_differing_pixel_fraction = 0.02` (2%); a pixel differs when any RGBA
channel exceeds the 8-level bound. `exact_image_hash = false`, and
cross-backend review is structural/manual. The reproducibility boundary is
documented in [`docs/export.md`](docs/export.md).

## Legacy C++ application and license

The original C++ application, assets, build scripts, and historical options
remain in the tree. See [`INSTALL`](INSTALL) for its dependency and autotools
instructions. Do not apply the Rust subcommand syntax to the legacy binary.

Gource - software version control visualization
Copyright (C) 2009 Andrew Caudwell <acaudwell@gmail.com>

This program is free software; you can redistribute it and/or modify it under
the terms of the GNU General Public License as published by the Free Software
Foundation; either version 3 of the License, or (at your option) any later
version.

This program is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
FOR A PARTICULAR PURPOSE. See the GNU General Public License for more details.

You should have received a copy of the GNU General Public License along with
this program. If not, see <http://www.gnu.org/licenses/>.

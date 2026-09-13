# Native release and packaging

The Rust successor is released as target-qualified archives. Packaging is
implemented by the scripts under `packaging/`; it is not the preserved C++
autotools release path. Every native release must name the target, source
revision, Rust toolchain, graphics evidence (if any), and external-runtime
assumptions rather than implying universal platform support.

## Release inputs

Start from a clean source revision that contains:

- `Cargo.lock` and `rust-toolchain.toml`, pinned to Rust **1.98.1** with the
  minimal profile;
- the workspace version in `[workspace.package]` and all six crates:
  `gource-core`, `gource-ingest`, `gource-sim`, `gource-render`,
  `gource-export`, and `gource-app`;
- `COPYING`, `THIRD_PARTY_NOTICES`, `README.md`, `data/gource.style`,
  `data/fonts/README`, `data/gource.1`, and
  `tests/fixtures/single-event.log`; and
- the source/runtime documentation and `ChangeLog` describing the current
  CLI, limits, cache, Git, export, and evidence boundary.

The packaging helper reads the literal workspace version from `Cargo.toml` and
rejects unsafe version/target names. It does not query Cargo metadata or the
network.

## Target matrix and prerequisites

| Wrapper | Rust target | Archive | Required build tools | Runtime smoke |
| --- | --- | --- | --- | --- |
| `packaging/linux-x86_64.sh` | `x86_64-unknown-linux-gnu` | `.tar.gz` | Cargo, rustup, Python 3 | packaged `--help`, CPU `diagnose` |
| `packaging/macos.sh --target x86_64-apple-darwin` | `x86_64-apple-darwin` | `.tar.gz` | Cargo, rustup, Python 3 | packaged `--help`, CPU `diagnose` |
| `packaging/macos.sh --target arm64-apple-darwin` | `arm64-apple-darwin` | `.tar.gz` | Cargo, rustup, Python 3 | packaged `--help`, CPU `diagnose` |
| `packaging/windows-x86_64.ps1` | `x86_64-pc-windows-msvc` | `.zip` | Cargo, rustup, Python launcher or Python | packaged `--help`, CPU `diagnose` |

The wrappers call `rustup target add TARGET` and
`cargo build --locked --release --package gource-app --target TARGET`.
Unix wrappers require `python3`; Windows prefers `py -3` and falls back to
`python`. FFmpeg and Git are **not** required for package creation or the
packaged CPU diagnostic because the fixture is a custom log. They are required
only when a release operator separately exercises local Git input or
`export --video`.

The macOS wrapper derives a host default but accepts an explicit target. Run an
arm64 build on an arm64 host or use an appropriate cross-compilation SDK and
then record that fact; the wrapper's target name alone is not runtime evidence.

## Build commands

From the repository root:

```sh
bash packaging/linux-x86_64.sh --output-dir dist
bash packaging/macos.sh --target x86_64-apple-darwin --output-dir dist
bash packaging/macos.sh --target arm64-apple-darwin --output-dir dist
```

On Windows PowerShell:

```powershell
.\packaging\windows-x86_64.ps1 -OutputDirectory dist
```

The Windows target can be overridden with `-Target`, but the published wrapper
name and default are x86_64 MSVC. Unix `--output-dir` is relative to the
project root unless absolute. The scripts create a private temporary staging
root and leave only the archive and checksum sidecar under `dist` (or the
requested output directory).

## Archive layout

A package root is named `gource-VERSION-TARGET` and contains:

```text
bin/gource-app                  # .exe on Windows
COPYING
THIRD_PARTY_NOTICES
README.md
assets/gource.style
assets/fonts/README
share/man/man1/gource.1
examples/fixtures/single-event.log
```

The archive helper requires the executable, `COPYING`,
`THIRD_PARTY_NOTICES`, `assets/gource.style`, the man page, and the fixture.
The staging scripts also require and copy `README.md` and the font notice. It
rejects staging symlinks and unsupported filesystem entries, writes normalized
archive metadata, and creates deterministic tar/gzip or zip member ordering
for a given staged tree.

The package intentionally does not contain a Git executable, FFmpeg, the
unresolved FreeSans binary, or uncleared legacy sprites. The binary uses the
user's local Git and FFmpeg only when those optional runtime paths are selected.

## Automated packaging checks

Each wrapper performs these checks before writing its archive:

1. install the target with rustup;
2. build `gource-app` locked and in release mode for that target;
3. stage required binary, legal, documentation, style, font-notice, man-page,
   and fixture files;
4. run the staged binary with `--help` and require non-empty output;
5. run the staged binary with
   `diagnose --input examples/fixtures/single-event.log --threads 1`;
6. parse the diagnostic JSON and require `snapshots_equal = true`, a positive
   event count, and non-negative timing fields;
7. create a target-qualified tar.gz or zip;
8. reopen the archive and verify required members and a non-empty file; and
9. write a SHA-256 digest sidecar named `<archive>.sha256`.

These checks prove that the package has a runnable binary, required common
assets, and the CPU ingestion/replay diagnostic. They do **not** exercise
`view`, winit window creation, input/resize, a native graphics adapter,
headless frame export, FFmpeg, or a real Git repository. A release record must
not turn the package smoke into those stronger claims.

The helper can be used directly for inspection:

```sh
python3 packaging/archive.py --print-version
python3 packaging/archive.py --verify-archive dist/gource-VERSION-TARGET.tar.gz --format tar.gz
python3 packaging/archive.py --sha256 dist/gource-VERSION-TARGET.tar.gz
```

Use the `.zip` path and `--format zip` for Windows. `--verify-diagnose FILE`
is intended for the wrapper's JSON output and rejects malformed/non-object
JSON, unequal snapshots, no events, or invalid timings.

## Evidence and support language

Record exact evidence in a release note or build artifact:

| Evidence | What it establishes | What it does not establish |
| --- | --- | --- |
| Packaged `--help` + `diagnose` on a target | Archive integrity and CPU startup/replay on that target | GPU/window/export/FFmpeg/Git portability |
| `target/gource-smoke-frames-2` | Corrected 320x180 headless render on the recorded Linux llvmpipe Vulkan adapter, fixture, and config | Other adapters, backends, OSes, architectures, or video output |
| `docs/performance.md` diagnostic | Measured serial vs bounded Rayon replay on the named Linux host and fixture | End-to-end export, GPU, encoder, or cross-platform performance |
| An operator-run Git/FFmpeg smoke | Behavior of the exact local executables and repository | A guarantee for different executable versions or hostile repositories |

The corrected reference export uses the exact fixture/configuration recorded in
`tests/reference/manifest.toml`: `tests/fixtures/visual-hierarchy-activity.log`,
`320x180`, real-time playback, 2 FPS, `[0,65)` repository time, overview
camera, seed 31, auto-skip 3, time-scale 1, and frames 4, 64, and 129. The
recorded adapter is Vulkan llvmpipe (`llvmpipe (LLVM 21.1.8, 256 bits)`).
Same-backend visual review uses the manifest policy
`max_channel_abs_delta = 8` and `max_differing_pixel_fraction = 0.02` (2%);
a pixel differs when any RGBA channel exceeds the 8-level bound.
`exact_image_hash = false`. Cross-backend review compares structural
invariants and recognizable scene state, never exact PNG hashes.

Do not call a target “supported” solely because its archive can be built. Use
“packaged” for archive checks, “CPU-diagnostic smoke” for the wrapper's
`diagnose`, and “headless render verified” only with the exact adapter/backend
and fixture record. Interactive and video support require an explicit native
run on the target or must remain unclaimed.

## Legal and provenance checklist

Before publishing:

- carry `COPYING` and `THIRD_PARTY_NOTICES` unchanged into every archive;
- verify the Rust dependency notices correspond to the locked dependency graph;
- retain the GPL attribution and acknowledgements in `data/gource.1` and the
  source documentation;
- record the source revision, workspace version, Rust 1.98.1, target triple,
  linker/SDK where relevant, and archive SHA-256;
- do not add proprietary fonts, Git, FFmpeg, or uncleared art to the package;
- if a runtime asset is added later, record its license and provenance in the
  notice files before packaging; and
- ensure release notes distinguish implementation from measured evidence.

## Release-note template

A native release note should state:

```text
Version: VERSION
Source revision: REVISION
Rust/toolchain: 1.98.1 (minimal)
Target: TARGET
Archive/checksum: ARCHIVE / SHA256
Package smoke: --help + diagnose (host/OS)
GPU evidence: exact adapter/backend, or "not run"
FFmpeg/Git smoke: exact executable/repository, or "not run"
Known limits: no universal GPU/window/video claim without native evidence
```

The command, fixture, dimensions, frame rate, range, and visual tolerance must
be recorded for any reference images. Never replace that context with a
brittle exact image hash assertion.

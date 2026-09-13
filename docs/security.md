# Security and trust boundaries

The Rust native application treats repository paths, imported log bytes,
contributor names, repository paths inside records, filter patterns,
configuration values, cache files, Git output, and FFmpeg diagnostics as
untrusted data. It bounds managed input and working state and rejects malformed
or incomplete operations, but it is **not a sandbox**. The process, operating
system, filesystem permissions, graphics driver, Git executable, FFmpeg
executable, and kernel remain trusted boundaries.

Target wrappers exist for Windows, macOS, and Linux, but a source build or one
native smoke run does not establish security or support behavior for every OS,
architecture, driver, or graphics backend. Target packaging and evidence
limits are recorded in [`releasing.md`](releasing.md).

## Finite custom-log input

`--input FILE` reads a regular finite custom log to EOF. `--input -` reads
finite stdin to EOF. The parser validates and indexes the complete source
before exposing an immutable history; it does not provide tailing/live-stream
semantics and never publishes a valid prefix after a later failure.

The strict record grammar has exactly four fields or four fields plus one
non-empty six-digit hexadecimal colour:

```text
timestamp|contributor|A|path
timestamp|contributor|M|path|#rrggbb
timestamp|contributor|D|path/
```

The parser enforces the following boundaries:

- physical records, complete finite input, paths, contributors, path depth,
  event count, parser/index memory, temporary sort disk, and merge fan-in are
  bounded before managed allocations grow;
- LF and CRLF are accepted; lone CR, NUL, blank records, and embedded line
  terminators are rejected;
- UTF-8 is validated, and a BOM is accepted only at byte zero;
- timestamps are complete signed epoch values or supported UTC/explicit-offset
  date forms; overflow, malformed dates, and ambiguous forms fail;
- only empty, `A`, `M`, and `D` actions are accepted, with empty action
  normalizing to `A`;
- paths are lexical repository names using `/`; one leading virtual-root slash
  is removed, while empty components, `.`, `..`, NUL, CR, LF, pipes, empty
  paths, and excessive depth fail; and
- add/modify actions against explicit directory targets fail. A directory
  delete expands over current descendants in deterministic order.

Records are ordered by `(timestamp, source_sequence)`. Equal timestamps retain
their source order and duplicates remain separate events. A limit failure is a
typed error, not EOF, empty history, event dropping, or permission to publish a
shortened prefix. Diagnostics include a stable error category, physical line
and byte offset where available, and bounded escaped context rather than an
unbounded copy of attacker-controlled input.

Repository paths inside records are never opened, stat'ed, canonicalized,
symlink-followed, or used as output filenames. The input path itself is passed
to filesystem APIs; normal OS permissions, symlink behavior, mount policies,
and TOCTOU races still apply to a regular-file source.

## Resource limits

The default ingest limits are:

| Resource | Default | Failure |
| --- | ---: | --- |
| Physical record, including delimiter | 1 MiB | typed record-size error |
| Complete finite source | 8 GiB | typed input-size error |
| Path bytes | 64 KiB | typed path-size error |
| Contributor bytes | 4 KiB | typed contributor-size error |
| Path components | 256 | typed depth error |
| Working memory | 128 MiB total | typed working-memory error |
| Temporary sort-run disk | 16 GiB | typed working-disk error |
| Merge fan-in | 32 runs/pass | typed fan-in error |

The configured working-memory budget is split between parser and index
construction. External sort runs live in private temporary directories and are
removed when their state is dropped. The application `ReplayConfig` default
for `max_events` is `u64::MAX`; set `--max-events` when a smaller event bound
is required. The direct ingest-library default is 8 GiB for event count.

The renderer validates dimensions, row strides, source/destination lengths,
resource limits, and single-sample target requirements. Default renderer caps
include 64 MiB per buffer, one million triangle vertices, 500,000 node
instances, one million label vertices, 100,000 labels, and a 4,096-label
export budget. These are safety bounds, not a promise that every adapter can
render every allowed workload.

## Configuration and environment

The resolver composes built-in defaults, one TOML file, recognized `GOURCE_*`
environment variables, and explicit CLI values in that order. `--config` wins
over `GOURCE_CONFIG` as the file selector. Config paths and ordinary relative
input/cache/output paths use the process working directory; no shell expansion
or command execution is performed.

TOML deserialization currently does not enable `deny_unknown_fields`, so an
unknown file key is ignored rather than serving as a security control. Invalid
types, malformed finite numbers, invalid viewports/frame rates/times, zero
limits, and replay conflicts fail before a source or subprocess is opened.
Secrets should not be placed in config or environment variables unnecessarily;
config values are not authorization or sandbox boundaries.

The Rust CLI has no arbitrary command option. Legacy spellings such as
`--git-log-command` are rejected by `clap`; they are never executed. An
`--input` value is data passed to filesystem APIs or to the fixed Git adapter,
not a shell fragment.

## Local Git-directory ingestion

When `--input` names a directory, `gource-ingest` uses the local Git adapter.
This is the fixed local adapter, not a shell-command compatibility path. The
adapter:

1. rejects an empty, non-directory, or final-symlink path;
2. canonicalizes the directory and records its device/inode or Windows file
   identity;
3. revalidates that identity before each child operation and before parsing
   output;
4. resolves `HEAD` (or one API-supplied, validated non-option revision) to one
   full object ID before reading history;
5. invokes `git` with `Command`, explicit arguments, and no shell;
6. requests a reverse root-inclusive raw `-z` log with a fixed format and
   disables rename detection, external diffs, text conversion, colour,
   signatures, notes, diff merges, and optional submodule omission;
7. passes `--end-of-options` before the validated object ID/ref; and
8. drains stdout and stderr concurrently, validates the complete protocol, and
   builds the same immutable indexed history as custom input only after a
   successful child exit.

The child environment is cleared except for `PATH` needed to resolve a bare
executable, then receives fixed `C` locale and Git safety settings including
`GIT_CONFIG_NOSYSTEM=1`, null global/system config paths, no pager, no lazy
fetch, no replacement objects, no terminal prompt, and no optional locks. The
adapter requests no network operation and does not bundle or download Git.
Git hooks, external diff/textconv, pagers, notes, signatures, and shell
aliases are not part of the protocol.

Git output and diagnostics are bounded by the ingest limits and a default
64 KiB stderr cap. The default complete-child deadline is 30 seconds. A
missing executable, invalid repository, malformed output, invalid UTF-8,
stdout/stderr limit, memory/event limit, timeout, cancellation, or non-zero
exit is an error; no partial history is returned. Child and reader resources
are joined/reaped on every failure path. An unborn `HEAD` is represented by a
valid empty history.

The current app CLI exposes directory selection through `--input`; it does not
expose Git executable, revision, or author-time switches. Those controls exist
only for API callers and remain validated fixed-argument values.

## Persistent cache privacy and integrity

The cache is opt-in with `--cache-dir` and is used by the app only for regular
custom-log files. Git directories and stdin bypass it. The cache stores
normalized paths, contributor names, and canonical events in an unencrypted
binary entry, so treat it as repository-sensitive data.

The root is created with owner-only permissions (`0700` on supported Unix
systems or an owner-only Windows ACL). Existing roots are checked for owner,
mode/ACL, directory identity, and final symlink safety. Entry files use private
permissions (`0600` or equivalent ACL). Cache operations hold a bounded lock
and use directory capabilities where available; privacy failure is an error,
not a fallback to a world-readable directory.

Entries are keyed by exact input digest/size, schema versions, ingest options,
and limit envelope. A complete checksum, header/count/length validation, and
bounded decode are required for a hit. Corrupt, stale, malformed, oversized,
symlinked, or concurrently replaced entries are treated as misses or removed
only when the inspected file identity still matches. A cache hit is never
returned from a partial payload.

The default aggregate quota is 4 GiB, the application also caps one entry at
the selected `--cache-bytes`, and the cache API bounds directory scans to 4,096
entries and 16 MiB metadata. Zero and `u64::MAX` quotas are invalid. Stores
write a private temporary file, synchronize it, atomically rename it to a
key-derived entry, and synchronize the directory. Oldest entries are evicted
within quota; unknown names, excessive scans, lock timeout, or quota failure
are reported. Removing or disabling the cache changes reuse only, not the
source result.

The cache does not encrypt data, protect against a privileged local reader,
prevent denial of service by deleting the directory, or provide a crash-
recovery journal. Use a dedicated private parent for sensitive repositories.
See [`cache.md`](cache.md) for the full quota and publication policy.

## FFmpeg process boundary

`export --video` starts the configured FFmpeg executable directly with a fixed
argument vector. The CLI uses `ffmpeg` resolved through `PATH`; API callers may
supply a different executable path and thereby intentionally trust that
program. No shell string, `system`, parent-directory change, repository path,
or arbitrary command fragment is assembled.

The child receives raw RGBA frames on stdin. The default arguments select raw
video input, configured dimensions and exact reduced frame rate, FFV1 level 3,
`-g 1`, configured colour metadata, and Matroska output. Stdout is discarded;
stderr is drained concurrently into a 64 KiB tail. The frame queue is bounded
to three tight packets and a matching byte cap. A full queue applies
backpressure rather than dropping frames or allocating without bound.

FFmpeg is trusted native code with the caller's OS privileges; direct argv
handling prevents shell injection but is not a sandbox. A missing executable
is a spawn error. Non-zero exit, writer failure, queue disconnect, I/O error,
timeout, or cancellation kills and waits for the child, joins worker threads,
and removes the private staging file. FFmpeg is not bundled, downloaded, or
claimed to have a particular license configuration; users must audit the
executable they choose.

## Output publication and cancellation

PNG and video sinks reject an existing final path and publish through private
staging artifacts:

- PNG frames and `manifest.toml` are written in a hidden staging directory and
  the complete directory is renamed into place;
- FFmpeg writes a hidden sibling staging file, which is renamed only after all
  frames, writer completion, clean child exit, deadline, and cancellation
  checks succeed; and
- a video manifest is written atomically beside the final video. If that
  manifest fails, the just-published video is removed.

Cancellation is checked between PNG operations and before scheduled export
frames. It cannot interrupt an individual synchronous image encode already in
progress. Atomic rename is a publication boundary, not a durability or
multi-process lock guarantee; a hostile process with write access to the output
parent can race creation, rename, or deletion. Use a private output directory
when that matters.

## Native evidence limits

The repository records corrected Linux headless frame evidence at
`target/gource-smoke-frames-2/` and the canonical comparison policy at
`tests/reference/manifest.toml`: `tests/fixtures/visual-hierarchy-activity.log`,
`320x180`, real-time playback, 2 FPS, repository range `[0,65)`, and review
frames 4, 64, and 129 on the recorded llvmpipe Vulkan adapter. This is evidence
for that exact host, backend, fixture, and configuration. It does not prove
cross-backend pixel identity, GPU-driver portability, window/input behavior,
video export, or Windows/macOS support.

Same-backend RGBA8 comparison uses the canonical
`max_channel_abs_delta = 8` and `max_differing_pixel_fraction = 0.02` (2%);
a pixel differs when any RGBA channel exceeds the 8-level bound.
`exact_image_hash = false`. Cross-backend review is structural/manual, and no
documentation should turn a reference image hash into a portability claim.

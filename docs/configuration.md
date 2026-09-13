# Runtime configuration and precedence

This document describes the configuration path implemented by the Rust native
application (`gource-app`). The preserved C++ program has a separate historical
configuration format; it is not loaded by the Rust CLI.

## Commands and configuration source

The Rust CLI has three subcommands:

```text
gource-app [--config FILE] view [options]
gource-app [--config FILE] export [options]
gource-app [--config FILE] diagnose [options]
```

`--config FILE` selects one TOML file. If that option is absent,
`GOURCE_CONFIG` may select the file. There is no automatic home/repository
search, extension-based legacy config detection, include directive, shell
expression, or remote URL evaluation. There is no `--load-config` or
`--save-config` option in the Rust CLI.

A TOML file may contain shared keys at the top level and command-specific
`[view]`, `[export]`, or `[diagnose]` tables. The active command's table is
merged after the shared top-level keys. Paths in a config file are passed as
paths relative to the process working directory; the application does not
change directory to implement config resolution. A directory selected as Git
input is validated and canonicalized by the Git adapter after configuration
composition.

The TOML decoder currently uses serde defaults without `deny_unknown_fields`.
Unknown keys are therefore ignored by the file decoder; a typo must not be
relied on to produce a diagnostic. Invalid types, malformed values, duplicate
TOML keys rejected by the parser, invalid combinations, and zero limits fail
before input or renderer startup. CLI unknown options are rejected by `clap`.

## Precedence

Configuration is composed in this order, with later layers replacing an
existing scalar value:

1. built-in defaults;
2. one file selected by `--config`, or by `GOURCE_CONFIG` when `--config` is
   absent;
3. recognized `GOURCE_*` environment variables; and
4. explicit options on the selected subcommand.

A repeated `--filter` on the command line replaces the file/environment filter
list in argument order. A `GOURCE_FILTER` value is split on commas. Scalar
options do not depend on accidental duplicate-option behavior. For viewport
configuration, a higher layer's `viewport = "WIDTHxHEIGHT"` replaces a lower
layer's `width`/`height` pair, and a higher layer's complete `width`/`height`
pair replaces a lower shorthand.

The effective values are validated after all layers have been merged. No
window, GPU context, Git/FFmpeg child, cache directory, or replay session is
created from a rejected configuration.

## TOML keys

The following keys are accepted at the top level and, where useful, in the
active command table. Values shown are the Rust field names, not legacy C++
spellings:

```toml
input = "tests/fixtures/single-event.log" # file, directory, or "-"
cache_dir = "/private/gource-cache"        # omitted disables cache
cache_bytes = 4294967296                    # finite aggregate cache quota
viewport = "1280x720"                      # or { width = 1280, height = 720 }
# width = 1280                            # use width and height together
# height = 720
seconds_per_day = 10.0
realtime = false
auto_skip_seconds = 3.0
time_scale = 1.0
# file_idle_seconds = 30.0                # omitted/0 means no expiry
camera = "overview"                      # overview or track
seed = 31
filters = ["src/**", "tests/**"]
max_record_bytes = 1048576
max_input_bytes = 8589934592
max_path_bytes = 65536
max_contributor_bytes = 4096
max_path_components = 256
max_events = 18446744073709551615
working_memory_bytes = 134217728
working_disk_bytes = 17179869184
run_fan_in = 32

# Export-only values may be top-level or under [export].
output = "/tmp/gource-frames"
video = false
frame_rate = "60"                         # FPS or NUMERATOR/DENOMINATOR
start = "0"
# end = "65"

# Diagnose-only values may be top-level or under [diagnose].
threads = 8

[export]
# output = "/tmp/gource.mkv"
# video = true
# frame_rate = "30000/1001"
# start = "0"
# end = "60"

[diagnose]
# threads = 8
# end = "60"
```

The `viewport` shorthand accepts `x`, `X`, or `×` between positive unsigned
width and height values. The alternate `width` and `height` form requires both
values. Viewport edges above 32,768 are rejected.

`frame_rate`, `start`, and `end` are strings in TOML because the CLI accepts
exact rational spellings. A frame rate may be an FPS value or
`NUMERATOR/DENOMINATOR`; repository times may be a non-negative integer,
finite number, or `NUMERATOR/DENOMINATOR`. Export `end` must not precede
`start`. `output` is required for `export`; `video = true` selects the
external FFmpeg sink.

## Built-in defaults

| Value | Default |
| --- | --- |
| `input` | stdin (`-`), consumed to EOF |
| `viewport` | `1280x720` |
| `seconds_per_day` | `10.0` |
| `realtime` | `false` |
| `auto_skip_seconds` | `3.0` |
| `time_scale` | `1.0` |
| `file_idle_seconds` | unset; no expiry |
| `camera` | `overview` |
| `seed` | `31` |
| `cache_dir` | unset; persistent cache disabled |
| `cache_bytes` | `4294967296` (4 GiB, used only if a cache directory is selected) |
| `frame_rate` | `60/1` |
| `start` | `0` |
| `end` | unset; export chooses one second after the final event |
| `video` | `false` |
| `run_fan_in` | `32` |
| `threads` | available parallelism for `diagnose` |
| `max_record_bytes` | `1 MiB` |
| `max_input_bytes` | `8 GiB` |
| `max_path_bytes` | `64 KiB` |
| `max_contributor_bytes` | `4 KiB` |
| `max_path_components` | `256` |
| `max_events` | `u64::MAX` in `AppConfig` |
| `working_memory_bytes` | `128 MiB` |
| `working_disk_bytes` | `16 GiB` |

The ingest library's direct `IngestOptions::default()` uses an 8 GiB event
count bound, but the application deliberately exposes `u64::MAX` as its
`ReplayConfig` event-count default. Use `--max-events` or `max_events` when an
application-level event bound is required. All other listed ingest limits are
still finite defaults, and zero values are invalid.

## Command-line mapping

The following table is the user-facing mapping. Every option is available on
`view` and `export` unless marked diagnose-only or export-only; common options
are also available on `diagnose`.

| CLI option | Configuration key | Notes |
| --- | --- | --- |
| `--config FILE` | file selection | Global; `GOURCE_CONFIG` is the fallback selector. |
| `--input PATH` | `input` | File, `-`, or local Git directory. |
| `--cache-dir PATH` | `cache_dir` | Enables cache only for regular custom-log files. |
| `--cache-bytes BYTES` | `cache_bytes` | Finite non-zero aggregate cache quota. |
| `--viewport WIDTHxHEIGHT` | `viewport` | Mutually exclusive with `--width`/`--height`. |
| `--width PIXELS --height PIXELS` | `width`, `height` | Must be supplied as a pair. |
| `--seconds-per-day SECONDS` | `seconds_per_day` | Conflicts with `--realtime` unless 86,400. |
| `--realtime`, `--no-realtime` | `realtime` | Explicit CLI booleans. |
| `--auto-skip-seconds`, `--auto-skip` | `auto_skip_seconds` | Finite non-negative seconds. |
| `--time-scale FACTOR` | `time_scale` | Finite positive factor. |
| `--file-idle-seconds`, `--file-idle` | `file_idle_seconds` | Finite non-negative seconds. |
| `--camera MODE` | `camera` | `overview` or `track`. |
| `--seed SEED` | `seed` | Deterministic layout seed. |
| `--filter GLOB` | `filters` | Repeatable; CLI list replaces lower layers. |
| `--max-record-bytes` | `max_record_bytes` | Physical-record cap. |
| `--max-input-bytes` | `max_input_bytes` | Complete finite-source cap. |
| `--max-path-bytes` | `max_path_bytes` | Lexical path cap. |
| `--max-contributor-bytes` | `max_contributor_bytes` | Contributor cap. |
| `--max-path-components` | `max_path_components` | Lexical depth cap. |
| `--max-events` | `max_events` | Event-count cap. |
| `--working-memory-bytes` | `working_memory_bytes` | Parser/index working-state cap. |
| `--working-disk-bytes` | `working_disk_bytes` | Temporary sort-run cap. |
| `--run-fan-in` | `run_fan_in` | Merge fan-in cap. |
| `--output PATH` | `output` | Export-only destination. |
| `--video` | `video` | Export-only FFmpeg selection. |
| `--frame-rate RATE`, `--fps RATE` | `frame_rate` | Export-only exact rate. |
| `--start SECONDS` | `start` | Export-only repository start. |
| `--end SECONDS` | `end` | Export or diagnose end; exclusive for export. Alias `--target` is diagnose-only. |
| `--threads COUNT` | `threads` | Diagnose-only non-zero Rayon worker count. |

Legacy names such as `--path`, `--log-format`, `--load-config`,
`--save-config`, `--git-branch`, `--author-time`, and `--output-ppm-stream`
are not aliases for these fields. They are rejected by the Rust parser rather
than silently approximated. Git directory input uses the current adapter's
fixed policy; its API-only revision and author-time controls are not exposed as
CLI options.

## Environment variables

Recognized environment variables provide the middle precedence layer:

| Variable | Value |
| --- | --- |
| `GOURCE_CONFIG` | TOML file selector when `--config` is absent |
| `GOURCE_INPUT` | `input` |
| `GOURCE_CACHE_DIR`, `GOURCE_CACHE_BYTES` | Cache path and quota |
| `GOURCE_VIEWPORT`, or `GOURCE_WIDTH` + `GOURCE_HEIGHT` | Viewport |
| `GOURCE_SECONDS_PER_DAY`, `GOURCE_REALTIME` | Repository speed policy |
| `GOURCE_AUTO_SKIP_SECONDS`, `GOURCE_TIME_SCALE`, `GOURCE_FILE_IDLE_SECONDS` | Replay controls |
| `GOURCE_CAMERA`, `GOURCE_SEED`, `GOURCE_FILTER` | Camera, seed, comma-separated filters |
| `GOURCE_MAX_RECORD_BYTES`, `GOURCE_MAX_INPUT_BYTES`, `GOURCE_MAX_PATH_BYTES` | Input limits |
| `GOURCE_MAX_CONTRIBUTOR_BYTES`, `GOURCE_MAX_PATH_COMPONENTS`, `GOURCE_MAX_EVENTS` | Identity/event limits |
| `GOURCE_WORKING_MEMORY_BYTES`, `GOURCE_WORKING_DISK_BYTES`, `GOURCE_RUN_FAN_IN` | Working-state limits |
| `GOURCE_THREADS` | Diagnose worker count |
| `GOURCE_OUTPUT`, `GOURCE_VIDEO`, `GOURCE_FRAME_RATE` | Export output/video/rate |
| `GOURCE_START`, `GOURCE_END` | Export/diagnose range |

Boolean values accept `1`, `true`, `yes`, or `on` for true and `0`, `false`,
`no`, or `off` for false. Numeric values must be finite and parseable. There
is no environment expansion inside TOML strings, and no arbitrary environment
variable is treated as a command or authorization setting.

## Validation and operational boundaries

Configuration is validated before history loading. In particular:

- all floating-point controls must be finite and within their documented
  ranges;
- replay speed, camera mode, viewport, frame rate, times, and all limits must
  be valid;
- export requires `--output`/`output`, and `end >= start`;
- no command accepts a positional path; and
- invalid input/configuration is an error, never an empty history, EOF, default,
  or partial export.

A regular file selected with `--input` may use the opt-in persistent cache. A
Git directory and stdin are always parsed through their source adapter instead
of the persistent cache. See [`cache.md`](cache.md) and
[`security.md`](security.md) for the security and privacy consequences of those
boundaries.

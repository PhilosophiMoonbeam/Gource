# Deterministic playback and export

The Rust application exposes one replay path to its interactive window and its
headless exporter. The exporter uses a fixed 120 Hz simulation, exact rational
time conversion, a supplied offscreen `wgpu` target, bounded readback, and
transactional output sinks. The implementation is native-first: a headless
run still needs an adapter and driver, and one host's result does not establish
support for every target.

## 1. Run the export command

A PNG frame-directory export does not require FFmpeg:

```sh
cargo run --locked -p gource-app -- export \
  --input tests/fixtures/single-event.log \
  --output /tmp/gource-frames \
  --viewport 320x180 \
  --frame-rate 60 \
  --start 0 \
  --end 1
```

A video export uses the external `ffmpeg` executable resolved through `PATH`:

```sh
cargo run --locked -p gource-app -- export \
  --input tests/fixtures/single-event.log \
  --output /tmp/gource.mkv \
  --video \
  --frame-rate 60 \
  --start 0 \
  --end 1
```

`--output` is required and an existing final path is rejected. `--viewport`
defaults to `1280x720`; `--frame-rate` accepts FPS or an exact rational such as
`30000/1001` (alias `--fps`); and `--start`/`--end` are non-negative repository
timestamps, with an exclusive end. If `--end` is omitted, the app uses one
second after the final event. A zero-length range is valid and publishes an
empty frame directory without requesting a GPU context.

`--input` accepts a regular custom-log file, `-` for finite stdin, or a local
Git repository directory. Input is consumed and indexed before replay; live or
tailing input is not an export source. Directory input uses the fixed-argument
local Git adapter described in [`security.md`](security.md). It does not fetch
or use a persistent cache; regular custom-log files may use the opt-in
`--cache-dir`/`--cache-bytes` cache described in [`cache.md`](cache.md).

## 2. Repository time and exact replay ticks

`ReplaySession` is the mutable replay owner. `view`, `export`, and `diagnose`
share the same event ordering and simulation implementation; export does not
maintain a second physics or event scheduler.

The canonical simulation frequency is **120 ticks per playback second**. Let
`origin` be the first canonical event timestamp and `r` the validated
repository-time rate. Tick `k` represents:

```text
repository_time(k) = origin + k * r / 120
```

`r` is rational:

```text
r = 1                                      when --realtime is enabled
r = (86_400 / seconds_per_day) * time_scale otherwise
```

The resolver sets the real-time spelling to 86,400 seconds/day and rejects a
conflicting explicit `seconds_per_day`. Values are reduced and advanced with
integer numerator/denominator arithmetic. Elapsed wall time is converted to
whole ticks while retaining a fractional remainder; a partial tick does not
replace the last snapshot.

Events due at a tick are applied in canonical `(timestamp, source_sequence)`
order before that tick is published. Events at the origin are present at tick
zero. Input timestamps may be zero or negative; the origin is the first event,
so the default export start is playback time zero.

For the app command, a start value of zero means the first event timestamp,
whether zero was omitted or supplied. A non-zero `--start` is used as the
explicit repository start. An omitted end means one second after the last event
(or the start for an empty history). Repository timestamps are mapped to
playback-relative time with checked rational arithmetic before the export
schedule is built.

## 3. End-exclusive frame schedule

`ExportSchedule` accepts a non-negative start, an end not before start, and a
positive reduced rational frame rate. It emits no endpoint frame:

```text
frame_count = ceil((end - start) * fps_num / fps_den)
t_i         = start + i * fps_den / fps_num
             for i = 0 .. frame_count - 1
```

An exact endpoint contributes no frame. Every operation is checked for
negative values, reversed ranges, and rational overflow. The simulation tick
sampled for frame `i` is integer floor sampling:

```text
tick_i = floor(t_i * 120)
```

Multiple frames may sample one tick when the output rate exceeds 120 FPS; they
are still rendered and sent to the sink separately. The loop seeks once to the
first non-zero target and advances one replay session for subsequent targets;
it never rescans history per frame.

Interactive next-event navigation intentionally uses a checked ceiling to land
on the first tick where an event applies. That navigation rule differs from
export's floor sample rule without changing event order or allowing time to
move backward.

## 4. Headless rendering and readback

A non-empty export lazily requests one no-surface `wgpu` context. It creates one
single-sample `Rgba8Unorm` texture with `RENDER_ATTACHMENT | COPY_SRC` usage
and the requested dimensions. The scene renderer records commands into the
caller-owned encoder; the export job submits, copies to a mapped readback
buffer, waits for mapping, packs the rows, and sends tight RGBA8 bytes to the
sink. No window or swapchain is created.

WebGPU requires row-copy alignment. For width `w` and height `h`:

```text
unpadded_row = w * 4
padded_row   = align_up(unpadded_row, 256)
GPU bytes    = padded_row * h
sink bytes   = unpadded_row * h
```

`pack_rgba8_rows` copies only the first `w * 4` bytes of each mapped row and
checks source/destination lengths and dimension arithmetic. The current loop
uses one synchronous `GpuReadback` and `device.poll(Wait)` per frame. The
`ReadbackSlotPool` validates bounded slot policies for API callers but is not
an active overlapped readback pipeline.

The canonical target policy is gamma-space premultiplied `Rgba8Unorm` with
`ONE, ONE_MINUS_SRC_ALPHA` blending and one sample. Export reads that target
directly. No byte-identical PNG or video result is promised across GPU vendors,
backends, drivers, CPU architectures, Rust/image versions, or FFmpeg builds.
A successful non-empty manifest records the selected backend, adapter, driver,
and driver version.

## 5. PNG frame-directory sink

`PngFrameSink` accepts tight RGBA8 frames in increasing index order and writes:

```text
frame_00000000.png
frame_00000001.png
...
```

It creates a hidden staging directory beside the requested destination. Frames
and `manifest.toml` are written there, then the complete directory is renamed
into place on `finish`. The destination must not exist before the run. Wrong
frame order, wrong byte length, image failure, cancellation, or publication
failure prevents final-directory publication and removes staging where
possible. A successful manifest is at `<frames-directory>/manifest.toml`.

## 6. FFmpeg / FFV1 video sink

`FfmpegSink` starts the configured executable directly with an argument vector;
no shell command is assembled. The default CLI configuration requests:

- raw RGBA input on stdin;
- configured width and height;
- the exact reduced frame rate;
- FFV1 level 3 and `-g 1` intra frames;
- configured colour-space, primaries, transfer, and range metadata; and
- a Matroska output container.

FFmpeg is external and is not bundled or downloaded. The default queue holds
three tight frame packets with a matching byte cap, stderr is drained into a
64 KiB tail, and the stall timeout is 30 seconds. Total render duration is not
limited. The producer starts the timeout only while a full queue prevents a
pending frame from being accepted; each accepted frame ends that backpressure
interval. It checks cancellation, writer state, and child status while waiting,
and it does not drop frames or grow without bound.

The encoder writes a private staging file. After the producer closes stdin, a
fresh stall timeout bounds draining the accepted queue, closing the writer,
encoder exit, and stderr EOF. The file is renamed to the requested final path
only after the writer completes, FFmpeg exits successfully, and cancellation
is clear. Non-zero exit, spawn/I/O failure, writer failure, queue disconnect,
stall timeout, or cancellation kills and reaps the child, joins worker threads,
and removes the staging file. A video manifest is written atomically beside
the final output with the final extension replaced by `.manifest.toml`; a
manifest failure removes the just-published video.

The direct argument boundary prevents shell injection but does not sandbox a
custom executable supplied by an API caller. A missing `ffmpeg` on `PATH` is a
spawn error. The project does not make a license/configuration claim about the
user's FFmpeg build.

## 7. Backpressure, cancellation, and publication

Both sinks require frame indices to start at zero and increase by one. They
reject missing, repeated, or out-of-order frames and reject any buffer whose
length is not exactly `width * height * 4`. Export never silently drops a
scheduled frame.

Cancellation is checked before every scheduled frame and is shared with the
FFmpeg writer. PNG encoding is synchronous, so a request is observed between
filesystem/image operations rather than interrupting an encode already in
progress. FFmpeg cancellation is supervised while the queue is blocked and
between frames. `ExportReport` is returned only after final output and manifest
publication succeed.

Atomic rename is a publication boundary, not a durability or multi-process
lock guarantee. A process with write access to the output parent can race a
creation, rename, or deletion; use a private output directory for hostile
multi-process environments.

## 8. Manifest and reproducibility envelope

Every successful export writes `ExportManifestV1` (`schema = 1`). It records:

- **input:** history schema, digest of catalog/paths/contributors and canonical
  event debug values, event count, path count, and contributor count;
- **config:** dimensions, reduced frame rate, mapped playback start/end, full
  `ReplayConfig`, speed/idle policy, camera mode, seed, algorithm version, and
  limits;
- **seed/revision/toolchain:** replay seed, `GOURCE_REVISION` when supplied,
  Rust version when supplied, package version, and revision;
- **backend:** `wgpu` backend, adapter, driver, and driver version (or
  `headless-requested`/`unknown` for an empty schedule);
- **render:** `Rgba8Unorm`, gamma-space transfer, premultiplied blend, and
  sample count 1;
- **encoder:** PNG frame-directory or FFmpeg/FFV1/Matroska identity, executable
  where applicable, RGBA input, and colour metadata; and
- **frame_count:** the exact end-exclusive schedule count.

With filters, the original catalog is retained while the event slice is
filtered. Manifest catalog counts therefore describe the retained catalog,
while event count describes the filtered stream; the digest is not a raw source
file hash. The ingest layer also computes raw input and dataset identities for
cache keys, but the generic `HistorySource` manifest does not copy those
fields through.

The reproducibility envelope is the exact finite input, history/parser schema,
replay/filter configuration, seed and algorithm version, mapped time range,
dimensions, frame-rate rational, target/blending policy, package revision,
toolchain, backend/adapter/driver, and encoder configuration. Repeating those
values supports a comparison; it is not a cross-platform bit-identity promise.

## 9. Corrected headless reference export

The corrected reference artifact is `target/gource-smoke-frames-2/`. The exact
fixture and CLI configuration are:

```sh
cargo run --locked -p gource-app -- export \
  --input tests/fixtures/visual-hierarchy-activity.log \
  --output target/gource-smoke-frames-2 \
  --viewport 320x180 \
  --realtime \
  --frame-rate 2 \
  --start 0 \
  --end 65
```

The fixture content is pinned as these eight records (the file has no blank
records):

```text
1700001000|Ada Lovelace|A|src/main.rs
1700001000|Grace Hopper|A|src/lib/parser.rs
1700001001|Ada Lovelace|M|src/main.rs
1700001002|Linus Torvalds|A|tests/unit/parser_test.rs
1700001003|Grace Hopper|D|src/lib/parser.rs
1700001063|Ada Lovelace|A|docs/README.md
1700001063|Linus Torvalds|M|src/main.rs
1700001064|Linus Torvalds|A|tools/bench/driver.rs
```

The canonical reference policy is recorded in
`tests/reference/manifest.toml`; the generated export manifest in
`target/gource-smoke-frames-2/` records the backend and output identity. The
reference manifest pins 8 input events, 130 frames, seed `31`, overview camera,
auto-skip `3.0`, time scale `1.0`, `Rgba8Unorm`, one sample, and the recorded
llvmpipe Vulkan adapter. Review points are
`frame_00000004.png`, `frame_00000064.png`, and `frame_00000129.png`. This is
recognizability evidence for the exact fixture/configuration/backend, not a
cross-GPU or cross-platform support claim.

The visual regression policy is deliberately tolerant rather than hash-based.
For same-backend RGBA8 comparison, the manifest sets
`max_channel_abs_delta = 8` and `max_differing_pixel_fraction = 0.02` (2%).
A pixel differs when any RGBA channel exceeds the 8-level bound.
`exact_image_hash = false`; cross-backend comparisons use structural
invariants and human review, not exact pixel equality. If a backend requires a
wider documented tolerance, record that backend-specific policy alongside its
manifest.

## 10. Diagnostics and performance evidence

The headless `diagnose` command uses the same finite loader and replay state but
runs a serial reference and bounded Rayon execution without a GPU:

```sh
target/release/gource-app diagnose --input <log> --threads 8
```

It reports `ingest_ms`, `serial_ms`, `parallel_ms`, `events`, `final_tick`,
`snapshots_equal`, and `configured_threads`. It is a replay measurement and
excludes rendering, readback, image encoding, FFmpeg, and publication. The
recorded CPU workload, host, exact fixture configuration, raw timings, and
interpretation are maintained in
[`docs/performance.md#measured-cpu-replay-evidence-2026-09-13`](performance.md#measured-cpu-replay-evidence-2026-09-13).
No CPU diagnostic number should be presented as an end-to-end export or GPU
throughput result.

## 11. Native evidence limits

The packaging scripts build target-qualified archives and smoke the packaged
`--help` and `diagnose` paths. Those checks establish archive contents and a
CPU replay path; they do not exercise interactive window creation, input,
resize, GPU rendering, headless export, or FFmpeg on every target. Native
support language must identify the exact OS, architecture, adapter/driver,
source revision, toolchain, fixture, and command that was exercised. See
[`releasing.md`](releasing.md) for the release checklist.

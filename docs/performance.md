# Export performance

## Status and evidence

The export path and native packaging boundary are implemented for this phase.
The renderer is recognizable, batched, and uses embedded text. This document
now records a headless CPU replay diagnostic; it is not an end-to-end export
throughput result or a GPU-timing result.

The repository currently provides:

- a deterministic benchmark-history generator and a documented fixture configuration;
- exact rational playback and export scheduling at a fixed 120 Hz simulation rate;
- one replay session per export job;
- a serial reference force solver plus a grouped, bounded Rayon diagnostic path;
- a supplied offscreen render target, blocking GPU readback, and bounded sink queues;
- PNG-directory and FFV1/Matroska output sinks with atomic final publication;
- native release packaging with a packaged-binary `diagnose` smoke check.

The current CI gate is `cargo check --workspace --all-targets`. That check is green after the application/export integration. An earlier workspace test run reached 67 passing tests before the latest additions. The latest workspace run did not complete: `ffmpeg_nonzero_fixture_reaps_and_removes_partial_output` hit `ETXTBSY` while setting up its temporary executable. Therefore this document does not call the workspace test suite green. End-to-end export throughput and GPU measurements remain separate gates; the CPU replay diagnostic below is the current measured performance evidence.

## Cost model

The following is a description of the current implementation, not a performance estimate.

### Ingest and indexing

Ingest reads the input to EOF with bounded record, input, path, contributor, and event accounting. It validates finite numeric values and canonical event ordering. The index is built either in memory or through external sorted runs when the configured working-memory limit requires it. External runs consume the configured temporary working-disk budget and are removed on completion or cancellation. Sorting and indexing therefore add an input-size-dependent pass before replay; no ingest throughput claim is made here.

### Replay and sampling

Simulation advances on an exact fixed 120 Hz tick grid. The export schedule uses a positive reduced rational frame rate, counts the half-open interval `[start, end)` with a ceiling, and samples each frame with an exact rational time followed by floor conversion to a simulation tick. A sample is rendered after the session has advanced to that tick. Consecutive samples that map to the same tick are still separate scheduled frames and are rendered and encoded separately; the implementation does not deduplicate them.

For a frame at schedule index `i`, the schedule time is:

$$
 t_i = start + i / f,
$$

where `f` is the requested frame rate. The simulation tick is the floor of `t_i` in 120 Hz ticks. For non-realtime playback, the repository-time range is mapped to playback time using the exact rational playback rate derived from `seconds_per_day` and `time_scale`; this conversion is separate from the schedule's floor-to-tick sample rule.

The normal view/export path uses the serial reference solver. The headless
diagnostic can run the same grouped solver with a bounded session-local Rayon
pool. In each canonical tick, layout points are partitioned into complete
same-parent groups and pair work is performed only within those groups; a
group of `n` points has `n(n-1)/2` candidate pairs. The largest sibling group
therefore remains quadratic in its visible point count. The serial grouped
walk is the behavioral reference used for the serial-versus-Rayon comparison.

### Rendering and readback

Each non-empty export creates one headless context, one renderer, one single-sample `Rgba8Unorm` offscreen target, and a bounded readback pool. The render target uses the same supplied-target renderer path as other callers rather than a window or surface path. CPU staging allocations are reused by the renderer, while the encoder path creates the GPU upload/readback resources needed for each export.

The readback copy uses the backend's 256-byte row-alignment requirement. For width `w`, the tight RGBA8 row is `4w` bytes and the mapped row is padded to `align_up(4w, 256)` bytes. Packing strips that padding before a sink receives exactly `4w * height` bytes. Readback currently waits for the submitted copy before the frame is pushed; there is no active render/readback overlap pipeline.

Geometry is batched. Resource limits bound vertex, node-instance, label-vertex, label-count, label-budget, and buffer sizes. These are safety and correctness limits, not capacity or performance results. Renderer output is premultiplied gamma-space RGBA8 with `ONE, ONE_MINUS_SRC_ALPHA` blending; different backends or GPUs are not promised to produce bit-identical pixels.

### Output sinks

The PNG sink writes deterministic `frame_{index:08}.png` files into a hidden staging directory and atomically renames the completed directory into place. The FFmpeg sink starts an external `ffmpeg` process, writes raw RGBA frames through stdin, and encodes FFV1 in Matroska. Its synchronous bounded queue is limited by both frame count and bytes (by default, three frames and three frame payloads). When the queue is full, the producer waits while checking cancellation, the deadline, and child status; frames are not silently dropped.

PNG encoding, pipe writes, FFmpeg processing, and final publication are therefore part of end-to-end export time. A video benchmark must report whether FFmpeg process time is included; the recommended report includes it because it is on the user-visible export path.

## Measurement methodology

Run measurements only after the headless runtime smoke path is known to work. Record the exact revision and toolchain for every run. The minimum reproducibility envelope is:

- operating system and kernel;
- CPU model and thread count;
- GPU model, driver, backend, and adapter selection;
- Rust toolchain and package version;
- benchmark generator revision and fixture seed;
- input file identity and event/path/contributor counts;
- viewport dimensions, output kind, frame rate, playback rate, start/end range, filters, and every relevant limit;
- whether the run is cold or warm, and whether a persistent cache is present (there is no persistent export cache currently);
- for video, the exact FFmpeg executable and version/configuration.

The repository's deterministic fixture generator is invoked in the form:

```text
python3 benchmarks/fixtures/generate_large_history.py --config <fixture-config> --output <history>
```

The checked-in fixture configuration uses seed `424242`, 1,536 events, 32 contributors, six top-level directories, and three phases: 8 visible files/256 events, 128 visible files/1,024 events, and 16 visible files/256 events. Equal timestamps are generated with probability `0.35`, and an idle gap of `7,200` is inserted every 256 events. The generator fixes the input shape; it does not provide a measured result by itself.

Measure the phases separately so an ingest regression is not hidden by rendering or encoding:

1. generate or obtain the exact fixture and record its identity;
2. measure cold ingest/index construction;
3. measure a fixed replay-only or headless smoke range when such a harness is available;
4. measure PNG export with a fixed frame count and range;
5. measure FFV1 export with the same frame sequence and record FFmpeg version/configuration.

For each phase, use a stable host state and collect wall time, user/system CPU time, maximum resident set, temporary-disk usage, output bytes, and produced frame count. `/usr/bin/time -v` is sufficient for the process-level wall/CPU/RSS measurements; an implementation-specific GPU timestamp or profiler MAY be added when the selected backend exposes one. Repeat cold and warm runs, report the median and spread, and retain the raw command lines and environment. Do not compare runs across changed fixture, dimensions, frame rate, range, backend, driver, or toolchain as if they were a speedup measurement.

The acceptance checks around a performance change are behavioral first:

- frame count and deterministic frame naming remain correct;
- frame order is contiguous and no frame is dropped under sink backpressure;
- manifests continue to identify the input/config/toolchain/backend/render/encoder envelope;
- partial output is absent after cancellation, timeout, or encoder failure;
- any serial-versus-optimized comparison uses a documented pixel or state tolerance, rather than assuming cross-backend bit identity.

A cache comparison is meaningful only after cache persistence, invalidation, quota, and atomic publication are implemented. A Git-ingestion comparison is meaningful only after the safe bounded Git boundary is implemented. Neither is a current benchmark result.

## Current optimization status

Implemented behavior that affects the cost model:

- exact schedule arithmetic avoids floating-point frame-time drift;
- replay owns one mutable session for an export job;
- renderer CPU staging is reused;
- external ingest runs are bounded by configured temporary-disk limits;
- FFmpeg backpressure is bounded and cancellation-aware;
- output publication is staged and atomic;
- the layout solver groups complete same-parent sets, using stable `(parent, id)`
  ordering and a bounded collision/repulsion pass;
- the diagnostic execution mode keeps one bounded Rayon pool per session and
  recursively splits only at group boundaries into disjoint point/scratch
  slices;
- native packaging includes the binary, assets, notices, fixture, and a
  packaged-binary diagnostic smoke.

Not implemented, and therefore not a result to benchmark or claim:

- a GPU-compute force stage or spatial force approximation;
- overlapping readback/render work with multiple in-flight frames;
- a worker-thread simulation pipeline or three-buffer snapshot handoff;
- safe bounded Git ingestion;
- cross-backend pixel identity or a cross-platform performance baseline.

The current path favors bounded memory, deterministic ordering, and measured
CPU work reduction over an unmeasured GPU throughput optimization. Any future
optimization must preserve the exact schedule contract, event ordering, sink
ordering, and publication semantics before its paired time/RSS result is
reported.

## Known limitations

The fixed 120 Hz simulation, grouped CPU force calculation, optional Rayon
execution, blocking readback, external FFmpeg process, and host GPU driver all
affect observed time. The optional idle-skip path contains a floating-point
conversion for its jump calculation; the exactness claim in this document
applies to schedule/time conversion and normal tick advancement, not to
treating that optional optimization as a bit-identical performance oracle.
Camera presentation state is not a complete replay-checkpoint identity, so
camera-path measurements must state how the view was supplied.

The CPU diagnostic is a replay measurement, not an export benchmark: it
excludes GPU rendering, readback, PNG/FFmpeg encoding, and sink publication.
Conversely, export measurements must include those user-visible stages when
reporting end-to-end time. No export-throughput number should be inferred from
the passing check gate, source-level unit checks, fixture shape, or the absence
of dropped frames. The measured CPU replay evidence below must not be
presented as a GPU or cross-backend result.

## Measured CPU replay evidence (2026-09-13)

### Command, workload, and host

The endpoint-parallel baseline and the grouped-solver measurements used this
command:

```text
target/release/gource-app diagnose --input <log> --threads 8
```

`diagnose` parses/indexes once, then times a full serial replay and a full
bounded-parallel replay. The replay timers start after the immutable history
clone, so `serial_ms` and `parallel_ms` are replay wall times rather than
ingest or clone times. The command does not initialize a renderer or GPU.

The grouped ten-run stress workload was configured for exactly 4,096 events,
1,024 visible items, 16 parent directories, and seed `424242`. The host was
Linux on x64 with an AMD Ryzen 7 5700G with Radeon Graphics; the host GPU was
`00.0 VGA compatible controller: Advanced Micro Devices, Inc. [AMD/ATI] Cezanne
[Radeon Vega Series / Radeon Vega Mobile Series] (rev c8)`. The CPU-only
diagnostic did not select a GPU backend, record a driver version, or collect
GPU timestamps.

### Losing endpoint-parallel baseline

The earlier endpoint-parallel implementation lost to serial in every recorded
row. Values are elapsed milliseconds; these are the recorded baseline samples:

| Workload | Serial | Endpoint-parallel | Outcome |
| --- | ---: | ---: | --- |
| 1,536 events / 128 visible | 324.261 | 368.122 | 13.526% slower |
| 4,096 events / 384 visible | 894.421 | 909.218 | 1.654% slower |
| 4,096 events / 1,024 visible | 3,190.582 | 3,329.460 | 4.353% slower |

This is a rejection of that parallelization granularity, not a rejection of
parallel replay in general. The baseline concerns force-solver endpoint
work; it does not alter the export schedule's end-exclusive endpoint rule.

### Grouped solver improvement and Rayon contribution

The replacement first sorts points by stable `(parent, id)` order and
partitions them into complete same-parent groups. It computes each group's
pair-work estimate `n(n-1)/2`, chooses a balanced split only at group
boundaries, requires at least 512 pair operations on each side, and recurses
with `rayon::join` over disjoint point and scratch slices. If no split meets
that threshold, the same groups are processed serially. This changes the
algorithmic work granularity and applies to both modes; it is not merely a
larger thread count.

On the matched 4,096-event / 1,024-visible row, the grouped serial median was
`1,421.2075 ms`, compared with the endpoint-era serial record of
`3,190.582 ms` (55.456% lower). Because the older value is a recorded
baseline sample while the new value is a ten-run median, this is directional
evidence of the grouped algorithmic improvement, not a controlled
cross-version speedup claim.

For the grouped ten-run stress measurement, the raw replay wall times were:

```text
serial_ms: [1424.1, 1419.989, 1416.56, 1447.422, 1425.591,
            1408.084, 1419.291, 1416.003, 1422.426, 1426.562]
rayon8_parallel_ms: [719.029, 701.016, 725.663, 723.204, 723.554,
                     743.327, 722.011, 711.293, 724.581, 696.789]
```

The raw medians are `1,421.2075 ms` (serial) and `722.6075 ms` (Rayon8).
Rayon therefore reduced the grouped serial median by `49.155%`, reported as
`49.16%`; all 10 Rayon runs were faster, and all 10 paired comparisons
produced exact serial/parallel snapshots. The 55.456% serial reduction above
is the grouped-solver contribution; the 49.155% reduction from
`1,421.2075 ms` to `722.6075 ms` is the measured Rayon contribution. Neither
number is an end-to-end renderer/export speedup.

### GPU-compute decision: reject for this phase

**Observed evidence.** The grouped CPU path still costs a `722.6075 ms`
Rayon8 median at 1,024 visible items, while the serial reference costs
`1,421.2075 ms`. The diagnostic is explicitly CPU-only, and no stage-level
profile has measured how much of either value is force accumulation versus
event application, checkpointing, or other replay work.

**Inference and cost.** The remaining replay time is a material CPU cost and
the quadratic same-parent pair work makes the grouped force stage a plausible
CPU bottleneck, but the current evidence does not prove that it dominates
end-to-end export time. A GPU force stage would have to preserve the
serial-reference snapshot contract despite backend-dependent floating-point
reduction order. Feeding the current CPU-owned replay state and snapshots
would also require per-tick uploads, fences, and/or readback, whereas the
renderer already has blocking readback and no active render/readback overlap
pipeline. Those synchronization costs can erase a kernel-only win. Compute
limits, precision, driver behavior, and shader support also vary across the
native `wgpu` backends; native packaging does not remove that portability
cost. Browser/WASM is explicitly deferred by the architecture and is not a
GPU-compute target or fallback.

**Decision.** Reject GPU compute for this phase. Keep the grouped serial
reference and the measured bounded Rayon path; do not add a speculative
compute shader, GPU simulation state, or browser scaffold.

**Revisit threshold.** Reopen the GPU question only when a stage-level profile
on a representative workload of at least 1,024 visible items shows the grouped
force stage consuming more than half of CPU replay or end-to-end export wall
time after the Rayon path, *and* a prototype measured with transfer,
dispatch, synchronization, readback, and encoding demonstrates a reproducible
end-to-end win over the `722.6075 ms` Rayon8 baseline. Acceptance would further
require exact serial snapshot equality over repeated runs and successful
behavior on every claimed native backend/adapter. Without all of those
measurements, the CPU result is evidence for continued CPU optimization, not
permission to add GPU compute.

## Visual boundedness and first-smoke evidence (2026-09-13)

The visual correction set changes the cost model without supplying a
benchmark result:

- Palette selection is deterministic and bounded. Explicit event colours
  override role defaults; otherwise fixed palettes and stable identity hashes
  choose file, contributor, directory, action, and label colours. No
  performance or visual-parity claim follows from this lookup policy.
- Parent-relative layout computes keyed anchors from actual hierarchy parents
  and applies serial spring/repulsion relaxation. Same-parent collision
  separation remains active while the scene settles, with finite per-tick
  movement and a 24-world-unit radial envelope. The force reference remains
  quadratic in visible layout points per canonical tick; the envelope bounds
  drift and memory shape but is not a speed guarantee.
- Action state is tied to typed `FileId` incarnations. A live trail follows
  its bound file; a deleting trail retains the captured pre-fade endpoint, and
  a recreated path cannot retarget an older action. This preserves stable
  topology semantics rather than using path aliases or collision-specific
  shortcuts.
- Snapshot bounds are a linear pass over drawable collections and include
  radii, contributors, both action endpoints, branch endpoints, and bounded
  label rectangles. Export recomputes an aspect-aware framing view from those
  bounds for each snapshot, with finite extents, zoom guards, and a 0.88
  content scale. This removes the fixed-view clipping failure but means
  framing work is part of every rendered frame.
- Labels use one batched vertex stream backed by an embedded 5x7 bitmap atlas.
  Candidate text is capped at 256 Unicode scalar values, and label/glyph/
  vertex/resource limits bound geometry. Unsupported Unicode uses a visible
  hollow-box fallback, so fallback glyphs consume bounded work rather than
  being silently omitted. Batching reduces draw-call structure; it is not a
  measured throughput claim.

### First headless smoke (defect record, not a benchmark)

The first headless export smoke used 320x180, 130 frames, 8 events, and the
llvmpipe Vulkan adapter. It recorded 753 black geometry pixels plus 20
label-bar pixels over 56,820 clear pixels in frame 4, and 477 black geometry
pixels plus 12 label-bar pixels over the same clear-pixel baseline in frame
129. The run exposed black default colours, unbounded drift, a fixed export
view, and placeholder label bars.

The corrected smoke is **PENDING — Main must rerun it** after the correction
set is integrated. The initial counts are failure evidence only and are not a
throughput, latency, memory, GPU, visual-parity, or cross-backend result. No
performance number or visual parity claim is made here; controlled
measurements remain open until the corrected smoke succeeds.

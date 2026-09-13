# Successor decisions (stage 1)

**Scope.** This is the stage-1 decision ledger for the Rust successor. It does not
change or remove the existing C++ application. The C++ implementation remains a
behavioral reference and a runnable application while the successor is built.

**Evidence convention.** `INSPECTED` means a source or specification fact was read
at the cited location. It does not mean the Rust behavior exists. Every Rust
implementation check in this document is explicitly **UNVERIFIED — PLANNED**;
no build, test, benchmark, package, or runtime check is claimed here.

**Status vocabulary.**

- **Inspected evidence:** directly observed in the C++ source, README, or SPEC.
- **Planned support:** the successor contract is selected, but its implementation
  and acceptance check are still unverified.
- **Intentional change:** the successor deliberately rejects or replaces an
  upstream accident, unsafe behavior, or nondeterministic rule.
- **Deferred:** retained as a possible later feature, not part of the first
  complete successor slice.
- **Unsupported:** not a successor capability; input/option rejection is required.
- **Verified:** reserved for a completed implementation check. No item in this
  ledger has this status yet.

## Frozen D01–D13 register

The following decisions are frozen for the successor. The wording is normative;
implementation may refine private representations without changing these
contracts.

| ID | Decision | Reason and consequence | Check state |
|---|---|---|---|
| **D01** | Use six crates — `gource-core`, `gource-ingest`, `gource-sim`, `gource-render`, `gource-export`, and `gource-app` — with one `ReplaySession` in `gource-sim` over a `gource-core::HistorySource`. | Keeps event/config/replay semantics independent of windowing and UI. Ingestion supplies a history; simulation owns world, playback, camera state, and checkpoints; render consumes snapshots; export and app reuse the same replay engine. No second export scheduler, engine, or async runtime is introduced. | **UNVERIFIED — PLANNED:** resolve package targets and exercise one shared replay path. |
| **D02** | Canonical event order is `(timestamp, source_sequence)`; duplicate events are retained; actions and paths are strict. | `timestamp` is signed epoch seconds (`i64`); `source_sequence` is the physical record order (`u64`) assigned before sorting. Stable ordering preserves equal-time input order and never removes duplicate records. Strict action/path validation prevents malformed input from becoming a different history. | **UNVERIFIED — PLANNED:** parser/order fixtures for equal times, duplicates, negative/zero times, and malformed records. |
| **D03** | Ingest finite sources into a bounded index using external sort when memory is insufficient. | Regular files and finite stdin are read to EOF, normalized, sorted, and published only after validation. Sort runs, merge buffers, intern tables, temporary files, and queues all count against explicit byte/item limits. Live infinite stdin is deferred because it cannot provide global order or deterministic backward seek. | **UNVERIFIED — PLANNED:** bounded-memory large-input and cancellation checks, including publication only after a complete index. |
| **D04** | Represent the hierarchy as a compressed component-radix tree. | Store path components, not one node per character or synthetic empty component. Common component prefixes share one branch; file leaves and explicit directory targets remain distinguishable. This preserves the visible repository tree while bounding structural overhead and allowing stable `PathId` identities. | **UNVERIFIED — PLANNED:** prefix split/merge, deep path, file-to-directory, and subtree deletion fixtures. |
| **D05** | Apply ordered lifecycle transitions with `EventKey` stale guards. | Every applied event carries its canonical key and generation. A delayed action, stale snapshot, or stale UI command cannot mutate a newer replay generation. Lifecycle order is create/modify/delete against the current path identity; delete/recreate gets a new file incarnation while preserving source history. | **UNVERIFIED — PLANNED:** equal-time transition, delete/recreate, stale-generation, and action-target checks. |
| **D06** | Simulate at a fixed 120 Hz and map repository time with rational arithmetic; sample with floor semantics. | The canonical simulation tick is `1/120` second, independent of wall-clock refresh and export frame rate. Repository-time advancement uses an exact rational numerator/denominator rather than accumulated `f32`; a frame at requested time `T` observes the state at `floor(T * 120)` ticks. Optional interpolation is deferred until measured. | **UNVERIFIED — PLANNED:** repeated-run tick/time tests, long-idle tests, and export schedule checks. |
| **D07** | Seeking restores complete replay state and snaps to repository time. | A seek restores world hierarchy, file incarnations, users/contributors, action lifetimes, random state, canonical camera, playback clocks, generation, and any other state that affects future output; then it replays forward to the snapped target. Changing a displayed timestamp alone is invalid. Checkpoints are bounded and keyed by replay identity. | **UNVERIFIED — PLANNED:** seek-vs-fresh-forward equivalence at event, idle, deletion, and equal-time boundaries. |
| **D08** | Use versioned, keyed deterministic perturbations. | Layout tie-breakers, initial offsets, and other pseudo-random choices derive from a versioned seed/key (dataset identity, stable path/identity, and purpose), never process-global RNG order or wall-clock state. Changing the algorithm version or seed invalidates replay checkpoints. | **UNVERIFIED — PLANNED:** same input/config repeated runs and seed/version invalidation checks. |
| **D09** | Establish an exact serial force reference before measured spatial/Rayon optimization with disjoint outputs. | The serial simulation defines behavior. Spatial indexes and Rayon may be added only where immutable frozen inputs produce disjoint output slots and measured work justifies the change; no floating-point atomics or lock-per-interaction design is accepted. Numerical tolerances are documented per workload. | **UNVERIFIED — PLANNED:** serial/optimized state comparison and paired measurements; no speedup is claimed now. |
| **D10** | Start with app-thread simulation; add bounded three-buffer snapshots only if threading is measured to help. | Window events, UI commands, simulation ownership, and presentation begin on one app thread. If later measurements justify a worker, transfer immutable snapshots through at most three reusable buffers, expose age/queue diagnostics, and allow interactive stale-snapshot discard without discarding required history. | **UNVERIFIED — PLANNED:** first vertical slice on the app thread; later concurrency gate requires bounded queue and latency evidence. |
| **D11** | Checkpoint the canonical camera; treat manual view as a presentation-only override. | Automatic overview/track camera state that affects replay is in the checkpoint and replay identity, including viewport/aspect where relevant. Paused/live manual pan, zoom, and rotation compose in `gource-app` at presentation time and do not mutate world or replay state. Export uses an explicit camera configuration. | **UNVERIFIED — PLANNED:** camera seek determinism, resize/aspect invalidation, and manual-view isolation checks. |
| **D12** | Replay-affecting configuration invalidates replay identity; render-only controls do not. | Dataset filters, dates, playback speed policy, simulation parameters, camera policy, seed, algorithm versions, and numeric mode produce a new replay identity/generation. Window size, label budget, UI theme, and other render controls may change without reparsing or replaying history. Effective config precedence is deterministic and validated. | **UNVERIFIED — PLANNED:** config precedence, identity hash, checkpoint invalidation, and render-only update checks. |
| **D13** | Resource-limit exhaustion is an explicit error; never silently truncate. | Limits cover bytes as well as counts: records, paths, hierarchy depth, sort runs, queues, caches, checkpoints, decoded assets, readback, and subprocess diagnostics. A limit failure reports the resource and configured bound. `max-files`-style event dropping is not a substitute for a bounded implementation. | **UNVERIFIED — PLANNED:** cap-boundary, oversized, cancellation, and failed-publication checks. |

## Source-backed observations that constrain the decisions

These are **INSPECTED** observations, not claims about the Rust implementation:

1. **Custom grammar and grouping.** The legacy regex is
   `^(?:BOM)?([^|]+)\|([^|]*)\|([ADM]?)\|([^|]+)(?:\|#?([hex]{6}))?`
   (`src/formats/custom.cpp:21`). Parsing accepts an epoch-like integer or a
   date string, defaults blank user/action to `Unknown`/`A`, and groups adjacent
   records only while timestamp and username match
   (`src/formats/custom.cpp:48-100`). The regex has no end anchor, integer
   conversion is permissive, and malformed lines can be mistaken for a parse
   boundary. D02 intentionally replaces those accidental acceptances with the
   finite grammar in `docs/input-format.md`.
2. **Legacy event normalization.** File paths are filtered for invalid UTF-8
   and receive a leading virtual root slash (`src/formats/commitlog.cpp:290-300`);
   usernames are filtered in `RCommit::postprocess`
   (`src/formats/commitlog.cpp:353-355`). File/user filters can drop records
   before application (`src/formats/commitlog.cpp:325-350`, `357-384`). D02
   and D05 keep source identity and filtering explicit instead of silently
   replacing bytes or dropping required history.
3. **Hierarchy behavior.** `RDirNode::addFile` discovers a common path prefix,
   creates an intermediate node, and redistributes children
   (`src/dirnode.cpp:241-267`, `377-496`). Removing the last file reaps an empty
   node (`src/dirnode.cpp:274-320`), and a file that becomes a directory is
   forced out when a descendant arrives (`src/dirnode.cpp:432-454`). D04 is a
   compact component-level implementation of these observed invariants.
4. **Lifecycle/action vocabulary.** `processCommit` expands a trailing-slash
   delete over all descendant files and ignores add/modify directory targets
   (`src/gource.cpp:1188-1242`). `addFileAction` maps `D` to remove, `A` to
   create, and every remaining action to modify (`src/gource.cpp:1244-1290`).
   The successor accepts only explicit `A`, `M`, and `D` (with the documented
   blank-action compatibility normalization) and rejects unsupported action
   bytes rather than mapping them silently.
5. **Legacy playback clock.** The C++ loop converts floating `dt` to integer
   seconds plus an accumulated fractional value (`src/gource.cpp:1715-1733`),
   then applies all queued commits whose timestamp is no later than the current
   time (`src/gource.cpp:1743-1774`). It can move the clock backwards for an
   out-of-order commit unless `--no-time-travel` is set
   (`src/gource.cpp:1756-1769`). D02/D06 intentionally provide a globally
   monotonic, rational clock instead of preserving float accumulation and
   non-linear time travel.
6. **Legacy seek reset.** `Gource::seekTo` calls `reset` and seeks the log
   (`src/gource.cpp:1067-1078`); reset clears queues, trees, users, files,
   captions, selection, clocks, and sequence counters
   (`src/gource.cpp:877-960`). D07 retains the observable invariant but makes
   complete replay state and checkpoint identity explicit, including deterministic
   perturbation state and canonical camera.
7. **Rendering surface.** Legacy drawing starts by selecting the display's 2-D
   mode and directly issuing scene/UI OpenGL work
   (`src/gource.cpp:2401-2475`); blending is configured in-place
   (`src/gource.cpp:2477-2479`). SPEC requires the successor renderer to accept
   a supplied texture view for both window and offscreen targets (SPEC.md:147-151).
   This is an intentional boundary change, not evidence that the C++ renderer
   already supports supplied targets.
8. **Colour/gamma.** The inspected C++ path supplies normalized RGB values to
   `glColor` and alpha blending without an explicit sRGB/linear transfer policy
   (`src/textbox.cpp:118-158`, `src/gource.cpp:2477-2479`). The successor
   intentionally chooses gamma-space premultiplied blending for the baseline:
   scene shaders emit premultiplied sRGB-encoded values into a shared
   `RGBA8Unorm` target using `ONE, ONE_MINUS_SRC_ALPHA`. The same canonical
   target is used for the window and export; the final presenter owns any
   surface conversion so the pipeline cannot apply gamma twice. This is an
   intentional compatibility choice, not a claim that the C++ path specified
   gamma. The backend/readback/presenter check is **UNVERIFIED — PLANNED**;
   no cross-backend pixel equivalence is promised.
9. **Git subprocess risk.** The legacy adapter assembles a shell command from
   options (`src/formats/git.cpp:88-128`), changes the process working directory,
   redirects to a temporary file, and calls `system`
   (`src/formats/git.cpp:145-193`; `src/formats/commitlog.cpp:93-96`). D13 and
   the safe-ingestion policy replace this with fixed executable arguments,
   child-specific working directories, bounded stderr, explicit exit/EOF checks,
   and cancellation/reap. User repository paths and refs are never shell text.
10. **Config flow and precedence.** The legacy startup first parses CLI args,
    optionally guesses a `.conf`/`.cfg`/`.ini` path, then loads it and parses
    CLI args again before importing settings and optionally saving
    (`src/main.cpp:35-115`). Defaults are reset during import
    (`src/gource_settings.cpp:611-619`), and legacy option types/aliases are
    registered in `src/gource_settings.cpp:211-366`. The successor uses one
    versioned TOML schema and explicit precedence in `docs/configuration.md`;
    old repeated-section files require loss-detecting migration.
11. **Format inventory.** The legacy selector includes Git, Git raw, Mercurial,
    Bazaar, custom, Apache, SVN, CVS-exp, CVS2CL, and related fallbacks
    (`src/logmill.cpp:190-263`). The successor's first input is strict finite
    custom logs; Git follows in a safe subprocess phase. Other adapters are
    explicitly deferred or rejected in `docs/compatibility.md`, never implied
    by omission.

## Cross-cutting implementation contract

- **Attribution:** translated/derived Rust modules use SPDX GPL-3.0-or-later and
  preserve applicable upstream attribution. This document is a plan, not a
  license grant or a claim that any module has been translated.
- **Finite publication:** no consumer observes a history until parsing,
  canonical sorting, validation, and index integrity checks complete. A failed
  import leaves no apparently complete cache/index.
- **Errors:** unsupported options, malformed records, invalid config, and limit
  exhaustion are typed, actionable errors with bounded source location/context;
  none is converted to EOF, a default, or an empty history.
- **Verification boundary:** implementation checks are planned for the phase
  owning each contract. Until those checks run, status remains **UNVERIFIED**;
  inspected C++ evidence must not be reported as successor compatibility.
## Dated deviations and evidence (2026-09-13)

The frozen decision register above (D01–D13) is unchanged. This appendix records implementation evidence and clarifications discovered while documenting the export, security, cache, and performance contracts; it does not replace those decisions.

### Evidence status

- **Implemented in source:** export scheduling uses a reduced positive rational frame rate, a 120 Hz simulation grid, an end-exclusive interval, ceiling frame-count calculation, and floor conversion from each exact sample time to a simulation tick.
- **Implemented in source:** repository-time export bounds are mapped to playback time from the first event timestamp (or zero for empty history). The default start is the first event timestamp and the default end is the last event timestamp plus one repository-time unit.
- **Implemented in source:** ingest validates finite input, preserves canonical event order and duplicates, supports bounded external sorted runs, and removes temporary state on failure or cancellation without publishing a prefix.
- **Implemented in source:** headless export renders through a supplied offscreen target. RGBA8 readback strips the backend's 256-byte row padding before the sink receives tight rows. PNG and FFmpeg/FFV1 sinks stage output and publish only after successful completion.
- **Implemented in source:** FFmpeg is launched with direct executable arguments and a bounded, cancellation-aware frame queue. Cancellation, timeout, encoder failure, and writer failure reap the child and remove staged output.
- **Implemented in source:** manifests identify the filtered history catalog, export configuration, seed/revision, toolchain, backend, render target, encoder, and frame count. The input identity is a catalog identity, not a raw source-file digest.
- **Observed gate:** `cargo check --workspace --all-targets` is green after application/export integration.
- **Observed gate:** an earlier workspace test run reached 67 passing tests before the latest additions.
- **Observed gate:** the latest workspace test run stopped during `ffmpeg_nonzero_fixture_reaps_and_removes_partial_output` setup because its temporary executable was still open for writing (`ETXTBSY`). The full workspace suite is therefore not called green here.
- **Present but unexecuted:** native headless runtime smoke, controlled export measurements, and cross-backend pixel checks.

The unit checks named by the implementation are source evidence; because the latest workspace run stopped at fixture setup, this appendix does not imply that every check executed successfully after the current additions.

### Deviations and limits

- **D06 clarification:** exact rational schedule arithmetic, end-exclusive counting, and floor tick sampling are implemented. The optional `maybe_skip_idle` jump still converts its jump calculation through `f64`; no interpolation is used.
- **D07 clarification:** replay checkpoints restore state, clock, snapshot identity, and generation, and the checkpoint count is bounded at 64. A byte quota for checkpoint storage is not implemented.
- **D08 clarification:** deterministic positions are keyed by seed/algorithm/path, and directory positions use stable sorted indices. This is not a blanket promise of bit-identical pixels or state across backends, GPUs, or platforms.
- **D09 clarification:** the active force reference is serial. Rayon/spatial parallelism and any corresponding speedup claim remain planned.
- **D10 clarification:** application-thread simulation and a three-entry latest-snapshot pool exist, but there is no worker-thread simulation pipeline. Readback is currently blocking rather than overlapped with later rendering.
- **D11 clarification:** camera mode is represented in replay configuration, while the canonical camera state is not part of the `ReplaySession` checkpoint identity. Manual camera input is presentation-only; export receives its view through the explicit render specification. A complete camera checkpoint contract is not verified.
- **D12 clarification:** actual config precedence is built-ins, TOML file plus selected command-specific section, known `GOURCE_*` environment variables, then CLI flags. Transient UI state is not persisted. The current TOML override structure does not reject unknown keys, so unknown-key rejection must not be documented as an implemented guarantee.
- **D13 clarification:** ingest, renderer, readback, and sinks enforce their implemented bounds, but there is no persistent-cache quota, checkpoint-byte quota, or general total-output quota.
- **Deferred boundary:** Git ingestion and persistent caching are not supported by the current implementation. Their safe input boundary, invalidation, quotas, and cleanup policy remain planned.
- **Reproducibility boundary:** gamma-space premultiplied RGBA8 and encoder color metadata are defined, but native runtime smoke and cross-backend pixel reproducibility remain open gates.

### Visual implementation decisions and first-smoke evidence (2026-09-13)

This appendix records the visual correction decisions represented by the
successor source. It refines D04--D12 without rewriting the frozen D01--D13
register above.

- **Deterministic palette precedence.** An event's explicit optional
  `#rrggbb` colour wins for the file incarnation, contributor, and action trail
  created by that event. If no event colour is present, file colours come from
  a fixed non-black palette selected by seed, algorithm version, and file
  extension (or the canonical path when there is no extension); contributor
  colours use a stable contributor-identity hash; directories use a fixed
  muted blue-grey; and add/modify/delete trails use semantic green/orange/red
  defaults. Labels use a separate light palette keyed by their stable target.
  Event colour is part of replay identity, so changing it cannot reuse an
  incompatible checkpoint. The consequence is deterministic, visibly
  non-black fallback output without path-based aliases or process-global
  colour state.
- **Parent-relative, bounded layout.** Directory anchors are derived from
  stable directory identity, parent identity, and depth, then placed relative
  to the current parent position. File anchors are likewise derived from
  stable file identity, parent identity, and path identity and are relative to
  the actual owning directory. A serial, bounded spring/repulsion relaxation
  runs on canonical ticks; positions are clamped to a finite 24-world-unit
  radial envelope and per-tick movement is bounded. Same-parent directory/file
  collisions use stable tie-breaking and persistent repulsion, including after
  nodes settle, so separation converges rather than being a one-tick collision
  patch. The consequence is recognizable hierarchy motion that cannot expand
  without bound; crowded or deeply nested input remains subject to the
  explicit envelope and force limits.
- **Typed action targets.** Action state binds a file action to its `FileId`
  incarnation, while retaining `PathId` only for catalog/display lookup.
  Live actions follow that exact file as its spring settles. A delete captures
  the deleted incarnation's position before fade-out; an absent delete or a
  directory-target activity has no file binding. Delete/recreate therefore
  receives a new `FileId`, and an old action cannot retarget the replacement
  through the shared path name.
- **Snapshot bounds and export framing.** `SceneSnapshot::bounds()` computes a
  finite envelope over directory/file radii, contributor extents, both ends of
  action trails, branch endpoints, and estimated label rectangles (with label
  extent bounded to 256 Unicode scalar values). Export derives a fresh
  aspect-aware `RenderView` from that envelope for every snapshot: overview
  centers the envelope; track prefers the mean of active contributors, then
  active files, and otherwise falls back to overview; fit uses a 0.88 content
  scale and finite zoom guards. Empty snapshots use a safe finite default.
  Offscreen export consequently no longer relies on a fixed world view, and
  animated trails/labels are included in framing, at the cost of a camera
  that may change when snapshot contents change.
- **Embedded bounded glyph batch.** Labels use a renderer-owned embedded 5x7
  bitmap atlas and one batched label vertex stream, rather than a downloaded
  font or a second texture-generation pass. Candidate text is capped at 256
  scalar values and scene label/glyph/vertex limits bound total work. ASCII
  letters, digits, and punctuation have deterministic glyphs; every
  unsupported Unicode scalar consumes one glyph slot and emits a visible
  hollow-box fallback instead of disappearing. This keeps label work bounded
  and recognizable, but intentionally does not promise upstream font shaping
  or glyph-level visual parity.

#### First headless smoke: defect evidence

The first headless export smoke used a 320x180 target, 130 frames, and 8
events on the llvmpipe Vulkan adapter. It was a defect-finding run, not a
passing visual result: frame 4 contained 753 black geometry pixels and 20
label-bar pixels over 56,820 clear pixels; frame 129 contained 477 black
geometry pixels and 12 label-bar pixels over the same clear-pixel baseline.
The failure exposed black default colours, unbounded layout drift, a fixed
export view, and placeholder label bars.

The corrective smoke is **PENDING — Main must rerun it** after the correction
set is integrated. Until that rerun, these counts are evidence of the former
defect only: they do not establish visual parity, cross-backend pixel
equivalence, or performance. The earlier “present but unexecuted” smoke note
above refers to this corrected rerun; the initial defect-finding smoke is
recorded here.

### Optional visual effects evaluation (2026-09-13)

#### Evidence inspected

The current visual artifact is `target/gource-smoke-frames-2/`: its manifest
records a 320x180, 130-frame export on the llvmpipe Vulkan adapter using the
canonical single-sample `Rgba8Unorm` target and premultiplied blending. Frames
`frame_00000004.png` and `frame_00000129.png` show a sparse but recognizable
hierarchy: muted blue-grey directory nodes, distinct coloured file/contributor
markers, thin branch/action connectors, a dark clear colour, and small embedded
glyphs. The connectors are faint at this size and the glyphs are not reliably
readable, but inspection shows no visible halo, particle cloud, or catastrophic
edge failure. This is screenshot evidence only; it is not a cross-backend pixel
or performance claim.

The renderer currently records one supplied-target render pass with batched
branch triangles, action triangles, instanced nodes/contributors, and one
batched label stream (`crates/gource-render/src/renderer.rs`). The target
contract rejects formats other than `Rgba8Unorm` and sample counts other than
one. `SceneSnapshot` already carries a bounded, deterministic action visual
with a live position, exact target, progress, and opacity; the overview bounds
include both action endpoints (`crates/gource-sim/src/lib.rs`). The specification
explicitly places glow, trails, and other multipass effects after the baseline
renderer (`SPEC.md:167`). The legacy bloom path also depends on the unresolved
`data/bloom*.tga` asset provenance documented in `docs/assets.md`.

#### Decisions

| Effect | Decision now | Minimal current polish and evidence | Concrete revisit metric |
|---|---|---|---|
| **Bloom** | **Reject for this slice; defer.** | Deterministic role/path palettes, a restrained dark clear colour, bounded parent-relative layout, and batched node/branch/action geometry already separate the visible objects in the inspected frames. Recreating the legacy texture-quad bloom would add asset/provenance work and a post-scene pass without a demonstrated readability defect. | Revisit only if a five-frame event-bearing audit at 320x180 finds at least one active node below **3:1 local luminance contrast** in two or more frames, and a paired prototype improves that rate with no more than **10% median frame-time increase** and bounded intermediate-target memory. |
| **Glow** | **Reject for this slice; defer.** | No independent glow state is present or needed: node size/opacity, contributor energy, semantic action colours, and premultiplied blending provide the current salience treatment. The screenshots show crisp coloured markers rather than a missing-contrast failure. | Revisit if the same fixed audit records **5% or more** of active node/action bounding boxes below the 3:1 contrast threshold, or if two independent screenshot reviews identify the same object-separation failure; require the same paired cost gate as bloom. |
| **Particles** | **Reject for this slice; defer.** | The immutable snapshot has no particle collection or unseeded transient state. Activity is represented by deterministic contributors and bounded action markers, while file fade and action lifetime already provide temporal change. Adding particles would require seeded/checkpointed state, caps, and another bounded batch for no observed need in the sparse current frames. | Revisit only when a fixed event-burst fixture has **at least 20% of event-bearing frames with no visible action marker despite active events**, and a bounded particle candidate improves event salience without changing replay identity or causing unbounded per-frame allocations. |
| **Trails** | **Keep the current minimal trail; reject longer/history trails now.** | `ActionVisual` is already a contributor-to-file motion trail: its marker moves from `position` toward the exact `target`, carries progress/opacity, and is encoded in one action batch. Delete/recreate identity and snapshot bounds are already handled, so a longer trail is not required for topology correctness. The inspected frames show the existing connector/action treatment, including faint links at the small export size. | Revisit longer trails if a fixed event-bearing export has **more than 10% of frames** where a marker cannot be followed from contributor to target for two consecutive samples, or if an explicit visual requirement demands persistent history; retain bounded history and deterministic snapshot identity if then added. |
| **Anti-aliasing** | **Reject for this slice; defer.** | Crisp instanced quads, thick-segment triangles, aspect-correct projection, and premultiplied blending keep the 320x180 output readable. There is no current MSAA or analytic coverage path, and the target validator intentionally requires one sample. | Revisit if an edge audit at 320x180 finds visible one-pixel stair-step discontinuities on **5% or more of non-axis-aligned branch/action segments** across two or more event-bearing frames; any AA path must preserve a single-sample supplied final target (or use a bounded renderer-owned resolve) and pass the same determinism/cost gate. |

These are intentional visual deferrals, not a rejection of the legacy C++
appearance. Any later effect must be derived from the immutable snapshot/tick
or from explicitly seeded replay state, remain bounded by renderer limits, and
use the same renderer-owned effect path for native and offscreen export. The
caller still supplies the final target and command encoder; no effect may assume
a window, swapchain, downloaded asset, wall-clock randomness, or backend-specific
pixel behavior. Export remains deterministic under the existing manifest
envelope, with the canonical `Rgba8Unorm` final target and premultiplied
gamma-space blending unchanged.

# Upstream behavior and successor contract

**Purpose.** This document separates what was inspected in the existing C++
application from what the Rust successor is required to do. C++ behavior is not
silently promoted to compatibility: each successor item is marked **planned**,
**intentional change**, **deferred**, or **unsupported**. Every implementation
check below is **UNVERIFIED — PLANNED**; no runtime, test, build, benchmark, or
visual result is claimed.

## 1. Evidence legend and reference boundary

- **INSPECTED:** exact C++/README/SPEC evidence at the cited file and line span.
- **PLANNED:** successor behavior selected for implementation; check not run.
- **INTENTIONAL CHANGE:** successor behavior differs by design, usually for
  determinism, safety, or a strict input contract.
- **DEFERRED:** later phase; no first-slice support is implied.
- **UNSUPPORTED:** successor rejects the behavior with a stable diagnostic.

The C++ application remains intact and runnable. The references below describe
its observed implementation, including quirks that the successor deliberately
does not reproduce.

## 2. Input, normalization, and event order

### Inspected C++ behavior

1. The custom regex accepts an optional UTF-8 BOM, four pipe-delimited fields,
   and an optional colour (`src/formats/custom.cpp:21`). It is not end-anchored.
2. Custom timestamp parsing accepts date strings through `parseDateTime` or
   otherwise uses permissive `atoll`; blank user/action become `Unknown`/`A`
   (`src/formats/custom.cpp:48-72`).
3. Adjacent records are coalesced only when timestamp and username match; a
   mismatching line is buffered for the next commit
   (`src/formats/custom.cpp:73-84`). `RCommitLog::nextCommit` parses, postprocesses,
   and validates one commit in source order (`src/formats/commitlog.cpp:216-235`).
4. File names are UTF-8-filtered and receive a leading `/`
   (`src/formats/commitlog.cpp:290-300`); usernames are filtered in postprocess
   (`src/formats/commitlog.cpp:353-355`). File and user filters can cause a
   commit/file to disappear before application
   (`src/formats/commitlog.cpp:325-350`, `357-384`).

### Successor contract

**PLANNED / INTENTIONAL CHANGE.** The successor accepts only the strict finite
grammar in `docs/input-format.md`. Each physical record gets a
`source_sequence` before any sort/filter; canonical order is
`(timestamp:i64, source_sequence:u64)`. Equal timestamps preserve source order;
duplicates remain distinct. Complete finite input is globally indexed before
publication, with bounded external sort if needed. Invalid UTF-8, overflow,
trailing data, empty lines, unknown actions, invalid paths, and record/resource
limits are fatal typed errors, never skipped or interpreted as EOF.

A source record's raw identity and its escaped display text are separate. Filtering
is replay configuration and cannot corrupt the immutable canonical index. The
legacy grouping and replacement behavior are evidence, not the successor event
model.

**Check:** **UNVERIFIED — PLANNED** (phase-2 parser/order fixtures and
index-publication check).

## 3. Hierarchy and path lifecycle

### Inspected C++ behavior

- `RDirNode::addNode` moves prefixed children under a newly inserted node and
  reparents them (`src/dirnode.cpp:241-267`).
- `RDirNode::addFile` forks a root on a nonmatching path, creates child nodes,
  detects a file that is a prefix of a new descendant, and redistributes common
  prefixes (`src/dirnode.cpp:377-496`).
- `removeFile` removes a file from the owning directory and deletes a child node
  when it has no files or children (`src/dirnode.cpp:274-320`).
- A file addition can therefore turn an existing file path into a directory
  prefix; the old file is forced out (`src/dirnode.cpp:432-454`).
- A trailing-slash delete expands recursively over descendant files, while
  add/modify directory targets are ignored (`src/gource.cpp:1188-1229`).

### Successor contract

**PLANNED / D04.** Use a compressed component-radix hierarchy. Split and merge
branches only at component boundaries; never create a node for every character
or an empty host-path component. Keep file leaves and explicit directory targets
distinct. Preserve virtual-root semantics, file-to-directory conversion,
delete/recreate identity, common-prefix reparenting, subtree deletion, and
reaping of empty branches. `PathId` identity is lexical and stable for a
canonical dataset; no host canonicalization, case folding, Unicode normalization,
symlink traversal, or backslash conversion occurs.

The implementation must keep source events even when a branch is not currently
visible. Level-of-detail/render culling is not history truncation.

**Check:** **UNVERIFIED — PLANNED** (phase-2 hierarchy reference scenarios,
deep/common-prefix memory bounds, and delete/recreate fixtures).

## 4. Actions, contributors, and stale transitions

### Inspected C++ behavior

`processCommit` obtains or creates each file and then delegates action creation.
`addFileAction` creates a contributor when needed and maps `D` to `RemoveAction`,
`A` to `CreateAction`, and every other action to `ModifyAction`
(`src/gource.cpp:1231-1241`, `1244-1290`). The contributor map is keyed by the
commit username, and `commit_seq` advances per file action
(`src/gource.cpp:1251-1278`).

### Successor contract

**PLANNED / D05.** Only normalized `A`, `M`, and `D` actions reach replay. An
empty custom action normalizes to `A`; any other byte is rejected. Every event
carries an `EventKey` and replay generation. A stale action, delayed UI command,
or stale snapshot whose generation/key is older than the current state MUST be
ignored or reported as stale, never applied to a newer file incarnation.

A deletion marks the current file incarnation dead; a later addition creates a
new incarnation and actions cannot target the old one. Directory deletion is a
single canonical event expanded deterministically by lexical descendant order.
Contributor identity is exact source identity unless a future adapter explicitly
normalizes it.

**Check:** **UNVERIFIED — PLANNED** (action ordering, duplicate, stale-generation,
delete/recreate, and contributor-identity fixtures).

## 5. Playback clock and idle behavior

### Inspected C++ behavior

When the first queue entry arrives, C++ sets `currtime` and `lasttime` to that
entry timestamp (`src/gource.cpp:1715-1720`). Each frame converts floating
`dt` and `days_per_second` to integer seconds plus accumulated fractional
seconds (`src/gource.cpp:1722-1733`). It then processes queued commits through
current time and can jump to the next commit after idle time
(`src/gource.cpp:1743-1756`). Without `--no-time-travel`, an out-of-order commit
can move `currtime` backward (`src/gource.cpp:1758-1769`).

The inspected defaults are 10 seconds/day, 3 seconds auto-skip, time scale 1,
no loop, no file-idle expiry, and overview camera
(`src/gource_settings.cpp:368-489`). These are migration/reference defaults,
not evidence that Rust has implemented them.

### Successor contract

**PLANNED / D06.** Simulation advances at exactly 120 ticks per wall/export
second. Repository time is a rational mapping from simulation ticks and the
configured playback policy; no `f32` remainder accumulation determines event
order. Canonical output at target `T` uses the state after
`floor(T_seconds * 120)` simulation ticks. The same schedule is used by
interactive and export paths; wall-clock refresh does not change replay results.

Idle-skip is an explicit replay policy: after its threshold, the clock advances
to the next event boundary, subject to stop/end rules. Repository timestamps are
never moved backward by input order. Negative/zero timestamps are valid source
values under the strict custom grammar.

**Check:** **UNVERIFIED — PLANNED** (120-Hz repeated-run, rational clock,
long-idle, unordered-input, end-of-history, and exact export-frame schedule
checks).

## 6. Seek, checkpoints, and replay identity

### Inspected C++ behavior

`Gource::seekTo` unpauses, calls `reset`, and seeks the log
(`src/gource.cpp:1067-1078`). `reset` clears the commit queue, trees, files,
users, captions, selected/hovered state, clocks, idle time, and sequence counters
(`src/gource.cpp:877-960`). The legacy log seek itself clears lookahead state and
moves a file pointer (`src/formats/commitlog.cpp:183-189`); it does not by itself
prove that all simulation state is restored.

### Successor contract

**PLANNED / D07.** A checkpoint includes every state that can affect future
output: canonical event cursor, repository/simulation clocks, hierarchy and file
incarnations, contributor/action timers, deterministic perturbation state,
canonical automatic camera, replay generation, and algorithm/numeric identity.
Seek snaps to a representable repository/simulation tick, restores a compatible
checkpoint (or origin), then replays events/ticks forward to the target. A changed
displayed timestamp without restoring state is invalid.

Checkpoints are bounded by count and bytes, keyed by `ReplayIdentity`; a
checkpoint from another dataset, filter, seed, algorithm, camera policy, numeric
mode, or generation is a miss. A stale snapshot cannot overwrite a newer one.

**Check:** **UNVERIFIED — PLANNED** (seek versus fresh replay at every boundary,
checkpoint invalidation, cancellation during restore, and bounded storage).

## 7. Deterministic layout and simulation

### Inspected C++ behavior

The upstream layout uses mutable directory positions, accelerations, and
per-frame logic (`src/dirnode.cpp:647-714`, `762-825`, `895-928`), a quadtree for
interaction (`src/gource.cpp:1418-1454`), and pseudo-random initial positions
(`src/dirnode.cpp:780-797`). This is evidence of recognizable hierarchy motion,
not evidence that the successor should copy memory layout or random-call order.

### Successor contract

**PLANNED / D08–D10.** Versioned keyed perturbations derive tie-breaks and initial
positions from stable identities and an explicit seed. The serial force/update
path is the correctness oracle. Only after measuring a bottleneck may a spatial
index or Rayon parallel stage be added; workers read frozen inputs and write
disjoint output slots, without float atomics or per-edge locks. App-thread
simulation is the first ownership model. A later threaded model is bounded by
three reusable immutable snapshot buffers and may drop only stale interactive
snapshots, never events.

**Check:** **UNVERIFIED — PLANNED** (serial reference, deterministic repeated
runs, optimized-vs-serial tolerance, and queue/memory measurements).

## 8. Camera and input view

### Inspected C++ behavior

The legacy loop updates manual keyboard/mouse camera state even while paused
(`src/gource.cpp:1613-1699`), rotates the world and user positions for manual
rotation (`src/gource.cpp:1654-1685`), and updates camera after interactions and
layout (`src/gource.cpp:1867-1877`). Rendering projects world objects using the
current OpenGL matrices and viewport (`src/gource.cpp:2431-2453`).

### Successor contract

**PLANNED / D11.** The canonical automatic camera is part of replay/checkpoint
state, including viewport/aspect when it changes the canonical result. Paused/live
manual pan, zoom, and rotation are an app presentation override and do not rotate
or mutate the replay world. A selected export camera is explicit and included in
export metadata. Overview/track are the initial camera modes; unknown modes are
errors.

**Check:** **UNVERIFIED — PLANNED** (camera seek equivalence, resize/aspect
identity, paused manual-view isolation, and export-camera checks).

## 9. Supplied-target renderer and gamma policy

### Inspected C++ behavior

`Gource::draw` selects the display's 2-D mode, clears/draws scene state, obtains
the current OpenGL viewport/matrices, updates buffers, and issues scene/UI draw
calls (`src/gource.cpp:2401-2475`). It configures alpha blending in the active
window context (`src/gource.cpp:2477-2479`). No supplied texture view or headless
render target is present in this path. `SPEC.md:147-151` explicitly requires the
successor renderer to render into a supplied texture view for both window and
offscreen targets.

The C++ snippets inspected for text/colour use normalized `glColor` values and
straight-looking alpha blends without declaring a transfer function
(`src/textbox.cpp:118-158`). That is an observation, not a gamma specification.

### Successor contract

**PLANNED / INTENTIONAL CHANGE.** `gource-render` accepts prepared scene data and
a caller-supplied target/view plus viewport and camera state. It does not create
a window, own repository parsing, or own replay semantics. Interactive window
and offscreen export use the same canonical render target path.

The frozen baseline gamma policy is **gamma-space premultiplied blending**:

1. Scene shaders emit **premultiplied sRGB-encoded** values.
2. The shared canonical target is `RGBA8Unorm` (not an implicit sRGB target).
3. Blend factors are `ONE, ONE_MINUS_SRC_ALPHA`.
4. Window and export render into that same canonical target.
5. The final presenter performs any surface conversion exactly once, preventing
   double gamma. Export readback records the target/transfer policy in metadata.

This is an intentional compatibility choice for a recognizable legacy look; it
must not be described as an upstream C++ guarantee. No cross-backend or
cross-GPU bit-identical pixels are promised.

**Check:** **UNVERIFIED — PLANNED** (supplied window/offscreen target, alpha edge,
readback, resize, and single-conversion gamma checks).

## 10. Git extraction and process safety

### Inspected C++ behavior

Git command text is assembled from options including branch and date strings
(`src/formats/git.cpp:88-128`). Directory extraction changes the process cwd,
redirects command output to a temporary file, invokes `system`, and restores cwd
(`src/formats/git.cpp:145-193`). The old parser is line/tab based and reads user
text and paths from that output (`src/formats/git.cpp:198-248`).

### Successor contract

**PLANNED / INTENTIONAL CHANGE.** Git is a later input phase. The adapter:

- resolves one explicit ref to an immutable object ID with fixed argv and
  `--end-of-options` semantics;
- uses a bounded raw `-z` traversal with no shell, no pager, no external diff,
  no textconv, no notes/signatures, no network/fetch, and no rename detection;
- uses one persistent length-framed metadata child (rather than one process per
  commit), with at most two Git children per import;
- drains stderr into a bounded tail, checks both EOF and child exit status, and
  terminates/reaps on cancellation; and
- treats repository paths, refs, author names, and object bytes as untrusted,
  never executable text.

Raw names remain lossless source bytes with escaped display/filter forms. Git
history statuses map `A/M/D` and type changes to `M`; rename/copy statuses are
not guessed under no-renames. Unsupported Git capabilities fail clearly rather
than fetching or silently reducing the history.

**Check:** **UNVERIFIED — PLANNED** (hostile path/ref/config, stderr saturation,
missing-object, cancellation/reap, raw-byte, merge, shallow-boundary, and parity
fixtures).

## 11. Config and compatibility boundary

### Inspected C++ behavior

The legacy format selector can instantiate Git, Git raw, Mercurial, Bazaar,
custom, Apache, SVN, CVS-exp, and CVS2CL adapters
(`src/logmill.cpp:190-263`). `GourceSettings` registers a broad set of booleans,
number types, strings, aliases, and command-line-only options
(`src/gource_settings.cpp:211-366`). Import resets defaults and validates many
legacy values (`src/gource_settings.cpp:611-1667`).

### Successor contract

**PLANNED / INTENTIONAL CHANGE.** One schema-1 TOML configuration has precedence
built-ins < one explicitly loaded file < explicit CLI overrides < transient
validated UI commands. Replay-affecting fields produce a new replay identity;
render-only fields do not. Unknown/duplicate keys and nonfinite values fail.
Legacy repeated-section playlists, implicit `.conf`/`.cfg`/`.ini` autodetection,
arbitrary `--log-command`, and unsupported options require explicit migration or
clear rejection. Details and field ownership are in `docs/configuration.md`.

**Check:** **UNVERIFIED — PLANNED** (precedence, migration loss detection,
identity invalidation, and unsupported-option diagnostics).

## 12. Behavior checklist for implementation

| Area | Successor target | Status/check |
|---|---|---|
| Event order | `(timestamp, source_sequence)`; monotonic; duplicates retained | Planned intentional change; **UNVERIFIED — PLANNED** |
| Hierarchy | Component-radix compression with file/dir lifecycle | Planned; **UNVERIFIED — PLANNED** |
| Actions | Strict `A/M/D`; directory delete recursive | Planned; **UNVERIFIED — PLANNED** |
| Playback | Rational repository clock, 120-Hz simulation | Planned intentional change; **UNVERIFIED — PLANNED** |
| Seek | Complete state restore and replay identity | Planned intentional change; **UNVERIFIED — PLANNED** |
| Camera | Canonical automatic state plus presentation override | Planned intentional change; **UNVERIFIED — PLANNED** |
| Rendering | Supplied target, shared window/offscreen RGBA8Unorm, frozen gamma policy | Planned intentional change; **UNVERIFIED — PLANNED** |
| Git | Safe fixed-argv adapter in later phase | Planned intentional change; **UNVERIFIED — PLANNED** |
| Other VCS | Explicitly deferred, not inferred | Deferred; **UNVERIFIED — PLANNED** |
| Limits | Explicit resource errors; no silent truncation | Planned; **UNVERIFIED — PLANNED** |

The exhaustive README option/format matrix and rejection text are in
`docs/compatibility.md`. This document intentionally does not label inspected
C++ behavior as verified successor behavior.

## 13. Successor visual implementation and first-smoke evidence (2026-09-13)

The following visual policies are successor implementation decisions and
intentional deviations from the inspected C++ rendering path. They do not
reclassify any upstream observation as compatibility and do not rewrite the
frozen D01--D13 register.

### Implemented visual decisions and consequences

- **Palette precedence.** An explicit event colour, when present, takes
  precedence for the file incarnation, contributor, and action trail generated
  by that event. Without it, files use a fixed non-black palette indexed by
  seed, algorithm version, and extension (or canonical path without an
  extension); contributors use a stable identity hash; directories use a fixed
  muted blue-grey; and add/modify/delete trails use semantic green/orange/red
  defaults. Labels use a light, stable-target palette. This is a deterministic
  successor rule, not an upstream `glColor` guarantee. It prevents black
  default geometry and makes event colour part of replay identity.
- **Parent-relative bounded hierarchy layout.** Keyed directory anchors are
  relative to their actual parent, and keyed file anchors are relative to their
  owning directory. Serial spring/repulsion relaxation is bounded by a
  24-world-unit radial envelope and a finite per-tick movement limit. Stable
  same-parent collision repulsion remains active while nodes settle, including
  mixed directory/file siblings; it is not a one-tick separation hack. The
  consequence is recognizable, convergent hierarchy motion without the
  unbounded drift of the old force behavior, while dense/deep trees remain
  constrained by the explicit envelope.
- **Typed action targets.** A file action binds to its `FileId` incarnation;
  `PathId` remains a lexical/catalog identity, not an alias for the current
  file. Live action trails follow that bound incarnation. Deletes retain the
  pre-fade position and file binding; absent deletes and directory-target
  activity have no file binding. A recreated path receives a new `FileId`, so
  an old action cannot attach to the replacement.
- **Snapshot bounds and export framing.** The immutable snapshot computes a
  finite bounds envelope including node radii, contributor extents, both
  action endpoints, branch endpoints, and bounded label rectangles. Export
  derives an aspect-aware view from that envelope per frame: overview centers
  the envelope, track follows active-contributor mean then active-file mean,
  and no active target falls back to overview. A 0.88 content scale, minimum
  extents, finite zoom clamps, and a safe empty-scene view keep framing
  representable. This intentionally replaces a fixed export view and can
  change composition as animated content changes.
- **Embedded bounded glyph batch.** Labels use an embedded deterministic 5x7
  bitmap atlas in one batched label stream. Text candidates are capped at 256
  Unicode scalar values and configured label/glyph/vertex limits bound scene
  work. Supported ASCII letters, digits, and punctuation use atlas glyphs;
  unsupported Unicode scalars use a visible hollow-box fallback and still
  consume one glyph slot. This removes external font/texture generation from
  the first slice, but is an intentional visual deviation from upstream font
  shaping and is not a glyph-level parity promise.

These choices preserve stable typed IDs and recognizable hierarchy while
keeping geometry and label work bounded. They also make the shared supplied
target path usable for both window presentation and offscreen export; they do
not promise identical pixels across backends, GPUs, or platforms.

### First headless smoke: observed failure and status

The first headless export smoke used a 320x180 target, 130 frames, and 8
events on llvmpipe Vulkan. It exposed black/unframed geometry rather than
passing visual compatibility: frame 4 contained 753 black geometry pixels and
20 label-bar pixels over 56,820 clear pixels; frame 129 contained 477 black
geometry pixels and 12 label-bar pixels over the same clear-pixel baseline.
The observed causes were black defaults, unbounded drift, a fixed export view,
and placeholder label bars.

This run is evidence of the former defect only. The corrected smoke is
**PENDING — Main must rerun it** after the correction set is integrated.
Accordingly, successor visual compatibility remains **UNVERIFIED — PLANNED**:
no visual parity, cross-backend pixel equivalence, or performance claim is
made from the first smoke or from source-level geometry checks.

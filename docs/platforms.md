# Platform support status

**Native-first status (2026-09-13):** the Rust successor's native implementation and packaging boundary is complete for this phase. Observed implementation evidence is a recognizable, batched renderer with embedded text, native packaging, and a 10-run diagnostic at 1,024 visible items: serial median `1421.208 ms` versus `Rayon8` median `722.608 ms` (49.16% median improvement, 10/10 runs faster, exact snapshots). This evidence describes the native implementation and measurement result; it does not make every OS/GPU combination supported. Each native row remains evidence-gated below. Browser/WASM remains explicitly **Deferred** after native completion: it is not a fallback target or an implied next build, and no browser scaffold is included.

## Status vocabulary

- **Inspected evidence:** documentation, source, or target configuration was inspected; no runtime claim follows.
- **Proposed:** an intended target or capability awaiting a native smoke record.
- **Intentional change:** the Rust successor deliberately uses a different implementation or backend policy.
- **Deferred:** intentionally held outside the current native-first release; it may be reconsidered only under explicit revisit criteria.
- **Unsupported:** not a target; reject or document it rather than silently implying support.
- **Verified:** only after the exact target, revision, assets, and interaction path have been exercised natively and the evidence is recorded.

## Native target matrix

| Platform | Initial candidate target | Intended capability | Current status | Evidence required before saying “supported” |
| --- | --- | --- | --- | --- |
| GNU/Linux | `x86_64-unknown-linux-gnu` (initial candidate; other architectures require separate evidence) | Windowed interactive playback, native `wgpu` rendering, and frame/offscreen export where the adapter permits it | **Proposed** | Native launch, custom-log playback, resize/input check, one frame/export check, GPU/driver record, and no-download asset check. |
| macOS | `aarch64-apple-darwin` and/or `x86_64-apple-darwin` (each is a separate candidate) | Native window/input and `wgpu` Metal path; offscreen export where the adapter permits it | **Proposed** | At least one smoke record per claimed architecture, including Metal adapter, window/input, asset lookup, and frame/export check. |
| Windows | `x86_64-pc-windows-msvc` (initial candidate; MinGW/other architectures require separate evidence) | Native window/input and `wgpu` backend; offscreen export where the adapter permits it | **Proposed** | Native launch, input/resize, packaged-asset lookup, frame/export check, driver record, and archive inspection. |

The listed triples are planning candidates, not a supported-architecture whitelist. A platform row may be split or narrowed when the first native run shows a backend or dependency limitation. A GPU driver that can run the legacy OpenGL program does not verify the Rust `wgpu` path.

## Existing C++ evidence versus Rust status

`INSTALL` documents generic Linux/macOS autotools commands and Windows Qt Creator/qmake guidance. `configure.ac` and `gource.pro` show the legacy dependency and platform paths; the changelog mentions experimental Wayland and Apple M1 fixes. These are **Inspected evidence** about the preserved C++ program and documentation only. They are not native smoke evidence for the Rust successor, and this stage intentionally does not change C++ behavior.

The Rust successor uses `winit` and `wgpu` rather than the C++ SDL/OpenGL path. That is an **Intentional change** in implementation, while keeping the required native Windows/macOS/Linux goal. Direct Vulkan integration remains **Deferred/Unsupported** as a project API choice: `wgpu` may select a native Vulkan backend where appropriate, but the application does not expose or require a direct Vulkan integration.

## Capability status

| Capability | Status | Notes |
| --- | --- | --- |
| Interactive native window | **Proposed** | Requires a successful `winit`/`wgpu` smoke run on each claimed target. |
| Window input and resize | **Proposed** | Must be exercised, not inferred from compilation. |
| Custom-log playback | **Planned** | First vertical slice; no Rust runtime evidence yet. |
| Git ingestion | **Deferred** | Use an external Git process later with explicit arguments; do not claim current support. |
| Frame-image/offscreen export | **Planned** | Must work without FFmpeg where the adapter supports headless/offscreen rendering. |
| FFmpeg video export | **Planned / external** | Requires a user-provided FFmpeg; no binary bundling or runtime download. |
| Browser/WASM deployment | **Deferred (native-first; not a current target)** | No `wasm32` target, web bootstrap, browser asset-download path, or browser runtime is promised. Revisit only under the criteria below; do not infer browser support from native `wgpu`. |
| Mobile and embedded targets | **Unsupported** | Not in the stage-1 target set. |
| Hosted/remote rendering | **Unsupported/deferred** | No service or remote runtime requirement may be introduced. |

Headless export may still require a compatible graphics adapter and driver. “Headless” does not mean “works on every CPU-only or software-only host”; record the adapter and result in the smoke evidence.

## Native-first boundary and browser/WASM deferral

Native completion does not promote the browser row. The current release boundary is the native Windows/macOS/Linux family, with support wording still controlled by the target-specific smoke records below. The renderer, export, and packaging contracts are kept native-first rather than being reshaped around an unimplemented web host.

### Recorded blockers

| Native assumption | Why it blocks browser/WASM now |
| --- | --- |
| Local Git subprocess and cache | The planned Git phase uses a local child with fixed arguments, a child-specific working directory, bounded diagnostics, explicit EOF/exit checks, cancellation, and reaping. Persistent history caching is also a native filesystem contract with versioning, quotas, validation, and atomic publication. Git ingestion and persistent cache are not current features even natively; a browser must not invent a remote or browser-local replacement for either one. |
| External process encoder | Native video export starts the configured FFmpeg executable directly, feeds it bounded frame data, applies backpressure, and reaps it on cancellation or failure. A browser cannot assume arbitrary local process launch, executable discovery, or equivalent cleanup. WebCodecs or another browser encoder would be a new output contract, not a transparent replacement for FFmpeg. |
| Native `winit` lifecycle | The native application owns the event loop, window, input, scale-factor, resize, minimize, and surface-error lifecycle. A browser host would need an explicit canvas/DOM scheduling, visibility, input, and device-loss integration; `winit` compiling for a web target would not by itself verify those transitions. |
| Headless `wgpu` export assumptions | Native export requests a no-surface context, a compatible adapter/driver, an offscreen `Rgba8Unorm` target, and mapped readback with backend row alignment. Browser WebGPU availability, limits, asynchronous device lifecycle, offscreen/readback behavior, and encoder access must be measured separately; “headless” is not a browser portability guarantee. |

### Decision

Keep native Windows/macOS/Linux as the only current release target family. Do not add a `wasm32` target, JavaScript bootstrap, web asset loader, remote Git/cache service, or browser encoder as preparatory scaffolding. Preserve the native fixed-argument process boundaries, `winit` lifecycle, supplied-target renderer, and headless export assumptions; browser work must not weaken or silently fork those contracts.

### Explicit revisit scope and trigger

Reopen browser/WASM evaluation only when all of the following are true:

1. Every native architecture that will be called supported has a completed smoke record for launch, custom-log playback, input/resize, packaged assets without runtime downloads, frame/offscreen export, and its adapter/driver; native archive inspection is complete.
2. A concrete browser requirement exists and is prioritized after the native release, rather than being justified by speculative portability.
3. A browser design can state bounded behavior for user-selected finite custom-log bytes only. It must not require local Git process access, persistent cache semantics, arbitrary process encoders, network asset downloads, or a server-side substitute.
4. A real browser feasibility run demonstrates the chosen WebGPU/canvas lifecycle, device-loss and resize handling, bounded memory/input limits, deterministic replay, and frame readback. Browser video export is in scope only if a browser-native encoder and its failure/cancellation semantics are separately specified and observed.

If approved, the first scope is a separate browser host that reuses the native core/simulation and supplied-target renderer contracts. It may expose interactive playback and frame images only after browser-specific evidence exists; it does not alter `gource-app`, native `winit`, native Git/cache boundaries, or FFmpeg behavior. A browser row may move from Deferred to Proposed only after that feasibility record, and to Verified only after the same exact browser target, revision, assets, and interaction/export path have been exercised. Until then, browser/WASM stays Deferred and no scaffold is created.

## Native smoke record

Before changing any target from Proposed to Verified, capture:

- target triple, OS release, CPU/GPU, driver, and selected `wgpu` backend;
- exact Rust toolchain, source revision, and `Cargo.lock` identity;
- launch, custom-log playback, input, resize, and a frame-image/export action;
- asset paths resolved from the archive, with confirmation that no runtime download occurred;
- behavior when optional FFmpeg/Git is absent or the feature is deferred;
- archive file list, including `COPYING`, `THIRD_PARTY_NOTICES`, WGSL, assets, and source URL/offer.

Until these records exist, release wording must say “proposed” (or “development target”), never “supported” or “verified.”

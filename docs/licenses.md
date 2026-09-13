# License and provenance audit

**Stage 1 status:** this is an evidence-backed inventory for the Rust successor. It is not a claim that a binary release has passed the release gates. Status words in this document have the following meaning:

- **Inspected evidence** means the path, header, history, or upstream metadata was inspected in this checkout.
- **Planned** means the successor intends to use or support the item; it is not implemented or release-verified.
- **Intentional change** means the successor deliberately differs from the C++ implementation and must document that difference.
- **Deferred** means the item is explicitly left for a later stage.
- **Unsupported** means a release must reject or omit it rather than silently pretending to support it.
- **Verified** is reserved for a repeatable release or native smoke check with recorded evidence. File presence alone is not runtime verification.

## Project license and C++ derivative obligations

`COPYING` is the repository's GNU General Public License, version 3, 29 June 2007. C++ headers and implementation files inspected under `src/` carry the GPL version 3 or later notice. The notices identify Andrew Caudwell in the primary files; format-adapter files also carry contributor notices, including John Arbash Meinel in the Bazaar adapter. `README.md` and `data/gource.1` repeat the copyright and GPL notice. These are **Inspected evidence**, and the files must remain intact.

The Rust successor is a derivative work where it translates, adapts, or otherwise incorporates protected Gource implementation or behavior. For every such Rust module:

1. Keep the applicable upstream copyright and contributor attribution.
2. Add an SPDX identifier for `GPL-3.0-or-later`, with the module's own copyright attribution as required by the repository convention.
3. Mark material translation or modification and the relevant date. Do not imply that an unchanged upstream implementation was newly authored.
4. License the combined derivative under GPL-3.0-or-later and retain notices and the absence-of-warranty text. Do not add a downstream restriction or a proprietary-only exception.
5. Keep `COPYING` in every source distribution and keep the corresponding notice in binary distributions.

These are GPL obligations, not a claim that all successor code has already been inspected. The Rust modules are **Planned** until their headers and history have been reviewed.

The C++ application remains in the repository and its behavior is intentionally preserved. Adding the Rust workspace does not relicense, remove, or rewrite the C++ program. A future release that ships both programs must satisfy the notice and source obligations for both.

## Existing third-party code and notices

| Component | Evidence in this checkout | License/provenance status | Release action |
| --- | --- | --- | --- |
| TinyXML sources under `src/tinyxml/` | Each inspected source/header begins with the TinyXML notice naming the SourceForge project and, in some files, Lee Thomason. | Permissive TinyXML notice: use, commercial use, alteration, and redistribution are allowed, subject to preserving origin attribution, marking altered sources, and retaining the notice. **Inspected evidence.** | Keep the source headers unchanged and repeat the notice in `THIRD_PARTY_NOTICES`. If a system TinyXML is selected, record that it is not bundled. |
| `src/core` submodule | `.gitmodules` points to `https://github.com/acaudwell/Core.git`; the checkout records gitlink `d8d880fd58fda37b9d8fb59844a3c2a712969948`, but the submodule work tree is not populated here. | License and headers are **unresolved** for this checkout. Do not infer them from the parent project's GPL notice. | Inspect the pinned Core revision and include its source and notices, or provide a compliant corresponding-source path, before a release that builds or ships it. |
| System C/C++ libraries used by the legacy build (SDL2/SDL2_image, FreeType, PCRE2, GLEW, GLM, Boost, libpng, OpenGL/GLU) | Dependencies are named by `configure.ac`, `INSTALL`, and `gource.pro`; this repository does not bundle their system copies. | External/system dependency status is **Inspected evidence** only; exact package and build licenses belong to the selected distribution. | Do not copy system-library notices into a project binary archive unless the library is actually bundled. Audit any static or vendored distribution separately. |

The C++ source and TinyXML notices are also captured in `THIRD_PARTY_NOTICES`. A notice file supplements, but does not replace, the original source headers.

## Bundled font: FreeSans

`data/fonts/README` identifies GNU FreeFont/FreeSans and states GPL version 3 or (at the distributor's option) any later version, with the special exception for embedding an unaltered font or unaltered portions in a document. It also says that the `.sfd` files are the FontForge native format and the preferred files for modification. The repository contains `data/fonts/FreeSans.ttf` and that README, but no `.sfd` preferred source or source-release pointer was found.

Therefore the current status is:

- **Inspected evidence:** the TTF and the license/exception text are present; the repository history records an import of FreeSans and a later update.
- **Blocked:** bundling `FreeSans.ttf` in a new Rust/native release is blocked until the preferred source (or a documented, compliant source offer) is available and the exact imported release is recorded.
- **Not blocked:** this provenance gap blocks bundling that font, **not development**. Developers may use a system FreeSans or pass another font with the existing/configured font-file mechanism while the audit is completed.
- **Planned:** if the preferred source is recovered, ship the TTF, `data/fonts/README`, the preferred source, and the applicable license notice together; otherwise make the release use a separately cleared system/alternate font and do not silently include this TTF.

The FreeSans exception does not remove the need to preserve the GPL notice or to determine the preferred source for a conveyed font work.

## Rust dependency license plan

The workspace foundation has selected the following direct dependencies and exact versions. License fields are the package metadata expected from the upstream crates; the exact enabled set, transitive set, and resolved checksums must still be taken from the committed `Cargo.lock`, not guessed from this table.

| Planned direct dependency | Planned version | Upstream package license metadata | Status and required evidence |
| --- | ---: | --- | --- |
| `egui` | 0.36.2 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `egui-winit` | 0.36.2 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `egui-wgpu` | 0.36.2 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `epaint` | 0.36.2 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `wgpu` | 30.0.1 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `winit` | 0.30.13 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `serde` | 1.0.229 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `clap` | 4.6.6 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `tracing` | 0.1.44 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `rayon` | 1.12.0 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `crossbeam-channel` | 0.5.17 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `image` | 0.25.10 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `thiserror` | 2.0.20 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `bytemuck` | 1.25.2 | Zlib OR Apache-2.0 OR MIT | **Planned**; verify package metadata and lock entry. |
| `pollster` | 1.0.1 | Apache-2.0 OR MIT | **Planned**; verify package metadata and lock entry. |
| `tempfile` | 3.27.0 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `blake3` | 1.8.7 | CC0-1.0 OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |
| `toml` | 1.1.6+spec-1.1.0 | MIT OR Apache-2.0 | **Planned**; verify package metadata and lock entry. |

No Rust dependency license status is **Verified** in this stage because the workspace manifests and lockfile are still being assembled. At release time, inspect every enabled direct and transitive package in `Cargo.lock`, preserve license texts where required, and update `THIRD_PARTY_NOTICES`. Do not add a dependency merely to avoid an audit finding. In particular, there is no async runtime in the planned dependency policy.

## FFmpeg and Git are external tools

FFmpeg is specified as an external process for the initial video-export implementation. Git is a planned external process/input source after the custom-log vertical slice. Neither tool is a bundled Rust dependency or a library linked into the successor at this stage.

- **FFmpeg — Planned / external:** do not ship an FFmpeg binary, download one at runtime, or claim a particular LGPL/GPL configuration without auditing the user's actual FFmpeg build. If a release later bundles FFmpeg, record that build's configuration, notices, and corresponding source separately.
- **Git — Deferred / external:** Git ingestion is after the first visualization slice. Do not construct shell commands from repository paths; use explicit arguments. Git's own license applies to the external Git executable, not to this project merely because it invokes it. If Git is bundled later, perform a separate notice/source audit.
- **Runtime downloads — Unsupported:** the application and release archives must not download assets, fonts, Git, FFmpeg, shaders, or license files at runtime. Missing external tools should produce an actionable error or an explicitly unsupported feature.

## Corresponding source and notice baseline

For an object-code/native release, the corresponding source package must include all source needed to build, install, run, and modify the shipped work, including build/release scripts and interface/source data that is not regenerated automatically. The initial release baseline is:

- `COPYING` and `THIRD_PARTY_NOTICES`;
- all C++ and Rust source, including the six workspace crates (`gource-core`, `gource-ingest`, `gource-sim`, `gource-render`, `gource-export`, and `gource-app`);
- `Cargo.toml`, the committed `Cargo.lock`, and `rust-toolchain.toml`;
- build, packaging, and release scripts;
- the WGSL sources used by the Rust renderer, plus the legacy GLSL sources needed by the preserved C++ application;
- bundled/required assets and their source forms or documented source offers, including sprite inputs where they are part of the release and the FreeSans resolution described above;
- a clear revision identifier and source URL/offer that remains available for the applicable GPL period.

A binary archive must carry `COPYING` and `THIRD_PARTY_NOTICES` and point to the matching corresponding-source archive. A source archive must not omit files simply because the binary can regenerate them. These are **Planned release requirements**, not **Verified** release results.

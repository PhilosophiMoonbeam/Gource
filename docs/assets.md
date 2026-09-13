# Asset provenance and distribution

**Stage 1 status:** the inventory below records what is in the existing C++ tree and what the Rust successor may use. It does not grant a license where the repository has not recorded one. A release status of **Verified** requires an archive review and a native smoke run; no Rust/native asset path is verified by this document alone.

## Status vocabulary

- **Inspected evidence:** the file, format, repository history, or notice was inspected.
- **Planned:** intended for a successor release, pending implementation and license clearance.
- **Intentional change:** the successor uses a different representation while the legacy asset remains for the preserved C++ application.
- **Deferred:** deliberately left for a later stage.
- **Unsupported:** must not be fetched or silently substituted at runtime.
- **Blocked:** a release cannot bundle the item until the stated provenance/source issue is resolved.
- **Verified:** only after reproducible packaging and the relevant native smoke evidence are recorded.

## Existing asset inventory

| Path(s) | Current evidence and provenance | License/provenance status | Successor/release treatment |
| --- | --- | --- | --- |
| `data/beam.png` | Present as a 128x1 RGBA PNG. Git history identifies the original sprite-image import (`83776ed`). | **Inspected evidence**, but no asset-specific copyright or license notice was found beside the file. | Keep for the legacy C++ application; **Planned** for successor reuse only after project-asset provenance is confirmed. Include in source/binary archives only when cleared. |
| `data/file.png` | Present as a 256x256 RGBA PNG. The high-quality sprite change is recorded in `fb592ca`; an earlier sprite import is `83776ed`. | **Inspected evidence** of repository provenance; no standalone license statement or artist credit was found. | The matching editable source is `resources/file.xcf`; retain both in the source archive. Successor bundling remains **Planned**, not license-verified. |
| `data/user.png` | Present as a 384x512 RGBA PNG. The high-quality sprite change is recorded in `fb592ca`; an earlier sprite import is `83776ed`. | **Inspected evidence** of repository provenance; no standalone license statement or artist credit was found. | The matching editable source is `resources/user.xcf`; retain both in the source archive. Successor bundling remains **Planned**, not license-verified. |
| `data/bloom.tga` | Present as a 512x512 RGB RLE TGA. The file metadata records Paint Shop Pro 12.80 and a 2009-05-12 timestamp, but no creator/license field. | **Inspected evidence** of format metadata only; provenance and permission are unresolved. | Preserve for C++ behavior. **Blocked** for a new bundled successor release until provenance/permission is recorded, or replace it with a separately cleared asset and document the **Intentional change**. |
| `data/bloom_alpha.tga` | Present as a 512x512 RGBA RLE TGA. No useful author or license metadata was found. | **Inspected evidence** of file presence; provenance/license unresolved. | Same as `data/bloom.tga`: preserve for C++, but successor bundling is **Blocked** pending clearance or an intentional replacement. |
| `data/gource.style` | Present as a small Mercurial-style template; it is listed in `dist_pkgdata_DATA` and is therefore an intended C++ runtime asset. | No standalone license header was found. Its authorship is treated as project work by context, but this is not an independent license record. | Keep in the legacy package. **Planned** for successor compatibility only if the parser consumes this format; carry the source file and attribution in the archive. |
| `data/shaders/bloom.vert`, `bloom.frag`, `shadow.vert`, `shadow.frag`, `text.vert`, `text.frag` | Six GLSL/OpenGL shader sources are present and listed for C++ installation. Shader history records project changes including `e57f583` and `4395ffc`. | No per-file license header was found; repository provenance is **Inspected evidence**, not a separate asset license grant. | These GLSL files are required by preserved C++ behavior. Rust rendering is an **Intentional change** to WGSL; do not label the existing GLSL as WGSL. New WGSL sources are **Planned** and must carry SPDX attribution and be included in corresponding source. |
| `data/gource.1` | Present as the installed man-page source; it repeats the GPL copyright notice and acknowledgements. | **Inspected evidence** of project documentation under GPL context. | Preserve in source archives and package documentation. It is not a substitute for `COPYING` or `THIRD_PARTY_NOTICES`. |
| `data/fonts/README` | Present GNU FreeFont documentation. It states GPL v3-or-later and the special unaltered-font embedding exception; it says `.sfd` is the preferred modification source. | **Inspected evidence** of the FreeSans terms. | Ship with any cleared FreeSans release, but the missing preferred source is a release blocker. |
| `data/fonts/FreeSans.ttf` | Present TrueType binary; git history records an import (`f5b35e9`) and later upstream update (`7f76932`). | License text is present, but no `.sfd` preferred source or compliant source offer is present. | **Blocked for bundling**, not for development. Use system FreeSans or an explicitly supplied alternate font while resolving the source gap. Do not silently include this TTF in a Rust/native archive. |
| `resources/file.xcf` | GIMP XCF source for the file sprite; it was introduced with the high-quality sprite change (`fb592ca`). The XCF contains a `Visible` layer but no asset license/author statement was found in its metadata. | **Inspected evidence** of an editable source and repository history; copyright/license/permission remain unresolved. | Keep in source archives while investigating. It is not a release clearance by itself; successor distribution is **Blocked** until sprite provenance is confirmed. |
| `resources/user.xcf` | GIMP XCF source for the user sprite; it was introduced with the high-quality sprite change (`fb592ca`). The XCF contains layers such as `Head`, `Head Shadow`, and `Background`, but no asset license/author statement was found in its metadata. | **Inspected evidence** of an editable source and repository history; copyright/license/permission remain unresolved. | Same as `resources/file.xcf`: preserve as source, but do not treat the XCF as license proof. Successor distribution is **Blocked** until provenance is confirmed. |

The image commit history provides useful provenance leads and credits Andrew Caudwell for the later sprite commits, but it does not replace an explicit asset license or permission record. Until that record exists, the prudent release status is **Blocked**, not “GPL by assumption.”

## FreeSans source gap

The unresolved FreeSans preferred source **blocks bundling that font, not development**. The C++ program can continue to use a system font directory or an explicitly selected font file, and Rust development can use a separately obtained/system font. A release may bundle FreeSans only after the exact upstream release, preferred source (normally `.sfd`), license/exception text, and corresponding-source treatment are recorded together. Never solve the gap by downloading the font at runtime.

## Rust successor assets

The Rust renderer is required to use WGSL. New WGSL files and any generated shader metadata are project source, must receive the repository's SPDX attribution convention, and must be included in the source archive. Existing C++ GLSL remains an intentionally preserved path; it is not removed or silently converted.

The initial successor asset policy is:

1. Prefer source-controlled, license-cleared assets in the release archive.
2. Keep source-form design files (`resources/*.xcf`) in the corresponding-source archive whenever their derived sprites ship.
3. Do not make a binary depend on an unrecorded working-tree path.
4. Do not download fonts, sprites, shaders, or other assets at runtime. Missing or uncleared assets must fail clearly or remain an explicitly unsupported option.
5. Record asset hashes and the matching source revision in release metadata once packaging exists. Hashes are **Planned**, not **Verified**, in stage 1.

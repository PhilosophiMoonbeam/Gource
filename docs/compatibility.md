# Compatibility matrix and migration policy

**Scope.** This matrix is exhaustive for the significant legacy options and
formats documented in `README.md`, plus the C++-registered aliases that can
otherwise be mistaken for supported behavior. It describes the Rust successor;
it does not change or remove the existing C++ application.

**Implementation status.** Every successor implementation check is
**UNVERIFIED — PLANNED**. `INSPECTED` below means only that the cited README or
C++ source was read. A row marked **Supported contract** is a selected target,
not evidence that the Rust behavior is currently present.

## Status vocabulary and rejection rule

- **Supported contract:** preserve the meaningful behavior in the first
  successor path, subject to the listed scope. Check is unverified/planned.
- **Planned support:** selected for a later successor phase; not available until
  its check passes.
- **Intentionally changed:** a replacement contract is selected and the legacy
  meaning is rejected or transformed with an explicit diagnostic.
- **Deferred:** may be implemented later; no support is implied now.
- **Unsupported:** not a successor feature. It is rejected, not ignored.
- **Inspected:** source evidence only, never an implementation verification.

For every known deferred, intentionally changed, and unsupported option, the
CLI MUST emit a diagnostic of the form
`unsupported-option: --name — <status>; <replacement or phase>` and return a
usage error. Unknown options emit `unknown-option`. Parsing and then ignoring a
flag is prohibited. Diagnostics go to stderr; stdout remains data-only for an
explicit output stream.

## 1. Input format and source behavior

| Legacy input/behavior | Inspected evidence | Successor status | Replacement/rejection and check |
|---|---|---|---|
| Custom log: `timestamp|username|A/M/D|path|colour` | README.md:402-412; src/formats/custom.cpp:21,48-100 | **Supported contract** (phase 2) | Strict finite grammar, exact fields, UTF-8/path/date validation, optional colour, blank user/action compatibility normalization. See `docs/input-format.md`. **UNVERIFIED — PLANNED.** |
| Custom log from finite stdin (`-`) | README.md:350-355; src/formats/commitlog.cpp:48-59 | **Supported contract** (phase 2) | Read to EOF, globally index/sort, preserve source sequence; empty input is valid. Live/tailing stdin is **Deferred**. **UNVERIFIED — PLANNED.** |
| Custom log from a regular file | README.md:350-355; src/formats/commitlog.cpp:61-81 | **Supported contract** (phase 2) | Complete bounded parse/index before publication. Resource errors never publish a prefix. **UNVERIFIED — PLANNED.** |
| Legacy custom parser's optional BOM, blank user→`Unknown`, blank action→`A` | src/formats/custom.cpp:21,58-72 | **Supported contract / intentionally tightened** | Retain only these normalizations; reject trailing junk, invalid UTF-8, malformed fields, and permissive numeric overflow. **UNVERIFIED — PLANNED.** |
| Equal timestamp grouping by adjacent username/timestamp | src/formats/custom.cpp:73-84 | **Intentionally changed** | Do not coalesce commits as an identity rule. Sort by `(timestamp, source_sequence)` and retain duplicates. **UNVERIFIED — PLANNED.** |
| Legacy VCS-generated Git log text | README.md:378-390; src/formats/git.cpp:198-248 | **Deferred** | Convert to strict custom log or wait for safe Git phase; line/tab quoted parsing is not accepted as a Rust input protocol. **UNVERIFIED — PLANNED.** |
| Git repository/directory | README.md:358-385; src/logmill.cpp:216-224 | **Planned support** (phase 5) | Fixed argv, immutable validated ref, raw `-z`, persistent bounded metadata child; no shell, fetch, pager, external diff, or unsafe config. **UNVERIFIED — PLANNED.** |
| Git raw fallback adapter | src/logmill.cpp:216-224; src/formats/gitraw.cpp:34-37 | **Deferred** | No separate line-based raw adapter in the first Git phase; use the one safe protocol or convert input. **UNVERIFIED — PLANNED.** |
| Mercurial (`hg`) | README.md:173-179; src/logmill.cpp:226-230 | **Deferred** | Reject with phase status; use custom-log conversion. **UNVERIFIED — PLANNED.** |
| Bazaar (`bzr`) | README.md:173-179; src/logmill.cpp:232-236 | **Deferred** | Reject with phase status; use custom-log conversion. **UNVERIFIED — PLANNED.** |
| SVN (`svn`) | README.md:173-179; src/logmill.cpp:256-260 | **Deferred** | Reject with phase status; use custom-log conversion. **UNVERIFIED — PLANNED.** |
| CVS via `cvs-exp` | README.md:393-400; src/logmill.cpp:238-242 | **Deferred** | Reject with phase status; use custom-log conversion. **UNVERIFIED — PLANNED.** |
| CVS via `cvs2cl` | README.md:173-179,393-400; src/logmill.cpp:262-263 | **Deferred** | Reject with phase status; use custom-log conversion. **UNVERIFIED — PLANNED.** |
| Apache combined log | C++ registration: src/logmill.cpp:250-254; config validation: src/gource_settings.cpp:773-794 | **Deferred** | Reject with phase status; no accidental support from a string parser. **UNVERIFIED — PLANNED.** |
| Caption log `timestamp|caption` | README.md:413-420; src/gource.cpp:1080-1129 | **Deferred** | `--caption-file` and caption parsing are not first-slice inputs. Reject clearly. **UNVERIFIED — PLANNED.** |
| Config file passed as positional input | README.md:348-355; src/main.cpp:45-70 | **Intentionally changed** | Normal input is schema-1 TOML only through `--load-config`; old config requires explicit loss-detecting migration and is never treated as an empty log. **UNVERIFIED — PLANNED.** |
| Omitted path/current-directory auto-discovery | README.md:350-355; src/logmill.cpp:194-210 | **Planned support with explicit limits** | A source path may default to the invocation directory only when the selected adapter can validate it; implicit nested-VCS guessing is not relied on. **UNVERIFIED — PLANNED.** |

## 2. Core CLI set

These are the meaningful names selected for successor support. Their checks are
all **UNVERIFIED — PLANNED**.

| README option | Inspected evidence | Successor status | Semantics/replacement |
|---|---|---|---|
| `-h`, `--help` | README.md:30-31; src/gource_settings.cpp:50-55 | **Supported contract** | Print successor options and status/rejection rules; exit without opening input. **UNVERIFIED — PLANNED.** |
| `--path PATH` | README.md:348-355; src/gource_settings.cpp:1629-1665 | **Supported contract** | Set one source path; resolve from CLI cwd; validate before replay. **UNVERIFIED — PLANNED.** |
| Positional `path` | README.md:25-27,348-355 | **Supported contract** | One positional source is equivalent to `--path`; extra positional values are a usage error. **UNVERIFIED — PLANNED.** |
| `--log-format custom` | README.md:176-179; src/logmill.cpp:244-248 | **Supported contract** | Select strict finite custom parser. **UNVERIFIED — PLANNED.** |
| `--log-format git` | README.md:176-179; src/logmill.cpp:216-224 | **Planned support** | Available only with phase-5 safe Git adapter. **UNVERIFIED — PLANNED.** |
| `--viewport WIDTHxHEIGHT` | README.md:33-35 | **Supported contract** | Bounded render viewport; `!` suffix is not accepted as hidden syntax (use explicit resizable/window config). **UNVERIFIED — PLANNED.** |
| `-f`, `--fullscreen` | README.md:57-58 | **Supported contract** | Window presentation control, not replay identity. **UNVERIFIED — PLANNED.** |
| `-w`, `--windowed` | README.md:60-61 | **Supported contract** | Explicit inverse of fullscreen; conflict is a usage error. **UNVERIFIED — PLANNED.** |
| `--start-date` | README.md:66-76; src/gource_settings.cpp:1271-1286 | **Supported contract** | Strict UTC/explicit-offset timestamp bound; not host-local ambiguous parsing. **UNVERIFIED — PLANNED.** |
| `--stop-date` | README.md:77-80; src/gource_settings.cpp:1288-1312 | **Supported contract** | Strict bound, inclusive/exclusive rule documented in input/replay API; no local-DST guessing. **UNVERIFIED — PLANNED.** |
| `-a`, `--auto-skip-seconds` | README.md:100-101; src/gource_settings.cpp:1211-1220 | **Supported contract** | Rational replay idle skip; invalid/nonfinite/negative values reject. **UNVERIFIED — PLANNED.** |
| `-s`, `--seconds-per-day` | README.md:103-104; src/gource_settings.cpp:1197-1209 | **Supported contract** | Convert to a rational repository clock; no float accumulation. **UNVERIFIED — PLANNED.** |
| `--realtime` | README.md:106-107; src/gource_settings.cpp:1360-1362 | **Supported contract** | Explicit repository-time policy; conflicts with a non-default seconds/day value are reported. **UNVERIFIED — PLANNED.** |
| `--no-time-travel` | README.md:109-111; src/gource.cpp:1758-1769 | **Intentionally changed** | Global canonical ordering already forbids backward replay time; reject as obsolete rather than accept a no-op. **UNVERIFIED — PLANNED.** |
| `-c`, `--time-scale` | README.md:115-118; src/gource_settings.cpp:1259-1268 | **Supported contract** | Replay-affecting simulation scale with finite bounds. **UNVERIFIED — PLANNED.** |
| `-t`, `--stop-at-time` | README.md:88-89; src/gource_settings.cpp:1341-1349 | **Supported contract** | Finite playback/export duration; invalid/nonpositive values reject. **UNVERIFIED — PLANNED.** |
| `--stop-at-end` | README.md:91-92; src/gource_settings.cpp:1372-1374 | **Supported contract** | End-of-history behavior is deterministic; no implicit looping. **UNVERIFIED — PLANNED.** |
| `-i`, `--file-idle-time` | README.md:120-122; src/gource_settings.cpp:1222-1233 | **Supported contract** | Replay lifecycle policy; `0` means the successor's explicit no-expiry setting, not a memory escape. **UNVERIFIED — PLANNED.** |
| `--camera-mode overview|track` | README.md:286-287; src/gource_settings.cpp:1455-1464 | **Supported contract** | Canonical automatic camera; manual view is presentation-only. **UNVERIFIED — PLANNED.** |
| `--file-filter REGEX` | README.md:224-225; src/gource_settings.cpp:1515-1535 | **Supported contract** | Bounded exclusion regex; any match excludes at replay configuration level. **UNVERIFIED — PLANNED.** |
| `--file-show-filter REGEX` | README.md:227-228; src/gource_settings.cpp:1538-1558 | **Supported contract** | Bounded include regex; repeated patterns are explicitly AND-combined. **UNVERIFIED — PLANNED.** |
| `--user-filter REGEX` | README.md:230-231; src/gource_settings.cpp:1561-1580 | **Supported contract** | Bounded exclusion regex; any match excludes. **UNVERIFIED — PLANNED.** |
| `--user-show-filter REGEX` | README.md:233-234; src/gource_settings.cpp:1584-1604 | **Supported contract** | Bounded include regex; repeated patterns are AND-combined. **UNVERIFIED — PLANNED.** |
| `-b`, `--background-colour FFFFFF` | README.md:131-132; src/gource_settings.cpp:1087-1103 | **Supported contract** | Bounded RGB render control; invalid colour rejects. **UNVERIFIED — PLANNED.** |
| `--title TITLE` | README.md:143-144; src/gource_settings.cpp:1166-1171 | **Supported contract** | Presentation title, escaped and bounded. **UNVERIFIED — PLANNED.** |
| `--hide ELEMENT,...` | README.md:295-310; src/gource_settings.cpp:623-702 | **Supported contract (implemented elements only)** | `date,users,tree,files,usernames,filenames,dirnames,bloom,progress,mouse,root` are explicit values; unknown/unsupported elements reject. **UNVERIFIED — PLANNED.** |
| `--hash-seed SEED` | README.md:312-313; src/gource_settings.cpp:1062-1067 | **Supported contract** | Versioned keyed deterministic perturbation seed; changing it invalidates replay identity. **UNVERIFIED — PLANNED.** |
| `--load-config FILE` | README.md:342-343; src/main.cpp:84-91 | **Supported contract (schema 1 only)** | Built-ins < file < CLI; old config requires migration. **UNVERIFIED — PLANNED.** |
| `--save-config FILE` | README.md:345-346; src/main.cpp:112-115 | **Supported contract (schema 1 only)** | Atomic effective-config save; no silent lossy legacy rewrite. **UNVERIFIED — PLANNED.** |
| `--author-time` | README.md:112-114; src/formats/git.cpp:106-110 | **Planned support** | Git phase only; selects the explicit author timestamp policy. Reject for custom input. **UNVERIFIED — PLANNED.** |
| `--git-branch BRANCH` | README.md:181-182; src/formats/git.cpp:122-125 | **Intentionally changed / planned Git support** | One explicitly validated ref/commit-ish via fixed argv; raw shell/revision-option text is rejected. **UNVERIFIED — PLANNED.** |

## 3. Legacy option inventory: window and playback

| README option | Inspected evidence | Successor status | Rejection/replacement and check |
|---|---|---|---|
| `--screen SCREEN` | README.md:37-38 | **Deferred** | Native monitor selection is not first-slice core behavior; reject with platform/deferred status. **UNVERIFIED — PLANNED.** |
| `--high-dpi` | README.md:40-46; src/main.cpp:151-163 | **Deferred** | Window scale follows native `winit` scale policy; no old toggle is accepted until verified. **UNVERIFIED — PLANNED.** |
| `--window-position XxY` | README.md:48-52; src/main.cpp:169-172 | **Deferred** | Reject; use platform/window config when implemented. **UNVERIFIED — PLANNED.** |
| `--frameless` | README.md:54-55 | **Deferred** | Reject until native decoration policy is implemented. **UNVERIFIED — PLANNED.** |
| `--transparent` | README.md:63-64; src/main.cpp:138-141 | **Deferred** | Reject; does not silently switch the canonical RGBA8Unorm/gamma target. **UNVERIFIED — PLANNED.** |
| `-p`, `--start-position POSITION` | README.md:82-84; src/gource_settings.cpp:1314-1328 | **Deferred** | Fraction/random start requires a deterministic indexed-position contract; use explicit timestamp seek first. **UNVERIFIED — PLANNED.** |
| `--stop-position POSITION` | README.md:85-86; src/gource_settings.cpp:1330-1339 | **Deferred** | Reject until event-index position semantics are specified. **UNVERIFIED — PLANNED.** |
| `--loop` | README.md:94-95; src/gource_settings.cpp:724-726 | **Deferred** | Finite end-of-history is first-slice behavior; reject rather than loop unexpectedly. **UNVERIFIED — PLANNED.** |
| `--loop-delay-seconds` | README.md:97-98; src/gource_settings.cpp:728-737 | **Deferred** | Requires loop semantics; reject with `--stop-at-end` replacement. **UNVERIFIED — PLANNED.** |
| `--file-idle-time-at-end` | README.md:124-126; src/gource_settings.cpp:1235-1246 | **Deferred** | End-specific lifetime policy is later; use one validated file-idle policy. **UNVERIFIED — PLANNED.** |
| `--elasticity FLOAT` | README.md:128-129; src/gource_settings.cpp:974-983 | **Deferred** | Layout tuning is not the core CLI set; later simulation config must preserve D09 reference semantics. **UNVERIFIED — PLANNED.** |

## 4. Legacy option inventory: appearance and assets

| README option | Inspected evidence | Successor status | Rejection/replacement and check |
|---|---|---|---|
| `--background-image IMAGE` | README.md:134-135; src/gource_settings.cpp:1159-1164 | **Deferred** | Local bounded assets may be added later; no remote downloads. **UNVERIFIED — PLANNED.** |
| `--logo IMAGE` | README.md:137-138; src/gource_settings.cpp:1173-1178 | **Deferred** | Reject until bounded local asset loading and provenance are implemented. **UNVERIFIED — PLANNED.** |
| `--logo-offset XxY` | README.md:140-141; src/gource_settings.cpp:1180-1193 | **Deferred** | Depends on logo support. **UNVERIFIED — PLANNED.** |
| `--font-file FILE` | README.md:146-147; src/gource_settings.cpp:985-1002 | **Deferred** | Reject until bounded local font loading, cache identity, and asset licensing are verified. **UNVERIFIED — PLANNED.** |
| `--font-scale SCALE` | README.md:149-150; src/gource_settings.cpp:1048-1060 | **Deferred** | Later render control; not implied by egui text support. **UNVERIFIED — PLANNED.** |
| `--font-size SIZE` | README.md:152-153; src/gource_settings.cpp:1004-1013 | **Deferred** | Reject until successor text metrics are specified. **UNVERIFIED — PLANNED.** |
| `--file-font-size SIZE` | README.md:155-156; src/gource_settings.cpp:1015-1024 | **Deferred** | Reject; use bounded label budget until implemented. **UNVERIFIED — PLANNED.** |
| `--dir-font-size SIZE` | README.md:158-159; src/gource_settings.cpp:1026-1035 | **Deferred** | Reject; use bounded label budget until implemented. **UNVERIFIED — PLANNED.** |
| `--user-font-size SIZE` | README.md:161-162; src/gource_settings.cpp:1037-1046 | **Deferred** | Reject; use bounded label budget until implemented. **UNVERIFIED — PLANNED.** |
| `--font-colour FFFFFF` | README.md:164-165; src/gource_settings.cpp:1069-1085 | **Deferred** | Reject until UI/render palette is explicit. **UNVERIFIED — PLANNED.** |
| `--key` | README.md:167-168; src/gource_settings.cpp:1352-1354 | **Deferred** | File-extension key is not first-slice UI. **UNVERIFIED — PLANNED.** |
| `--date-format FORMAT` | README.md:170-171; src/gource_settings.cpp:705-710 | **Deferred** | Successor dates use explicit UTC/metadata formatting first; reject strftime injection/locale ambiguity. **UNVERIFIED — PLANNED.** |
| `--highlight-dirs` | README.md:187-188; src/gource_settings.cpp:1451-1453 | **Deferred** | Reject until label/selection policy is implemented. **UNVERIFIED — PLANNED.** |
| `--highlight-user USER` | README.md:190-191; src/gource_settings.cpp:1479-1491 | **Deferred** | Reject; no silent selection filter. **UNVERIFIED — PLANNED.** |
| `--highlight-users` | README.md:193-194; src/gource_settings.cpp:1446-1449 | **Deferred** | Reject until user-label presentation exists. **UNVERIFIED — PLANNED.** |
| `--highlight-colour FFFFFF` | README.md:196-197; src/gource_settings.cpp:1105-1121 | **Deferred** | Reject until palette support. **UNVERIFIED — PLANNED.** |
| `--selection-colour FFFFFF` | README.md:199-200; src/gource_settings.cpp:1123-1139 | **Deferred** | Reject until selection UI. **UNVERIFIED — PLANNED.** |
| `--filename-colour FFFFFF` | README.md:202-203; src/gource_settings.cpp:924-939 | **Deferred** | Reject until label palette. **UNVERIFIED — PLANNED.** |
| `--dir-colour FFFFFF` | README.md:205-206; src/gource_settings.cpp:1141-1157 | **Deferred** | Reject until hierarchy palette. **UNVERIFIED — PLANNED.** |
| `--dir-name-depth DEPTH` | README.md:208-209; src/gource_settings.cpp:1607-1616 | **Deferred** | Reject until label budgeting/depth policy. **UNVERIFIED — PLANNED.** |
| `--dir-name-position FLOAT` | README.md:211-213; src/gource_settings.cpp:1618-1627 | **Deferred** | Reject until edge-label layout is implemented. **UNVERIFIED — PLANNED.** |
| `--filename-time SECONDS` | README.md:215-216; src/gource_settings.cpp:941-950 | **Deferred** | Reject until label lifetime is replay/render-separated. **UNVERIFIED — PLANNED.** |
| `--file-extensions` | README.md:218-219; src/gource_settings.cpp:1507-1509 | **Deferred** | Reject until display-only path projection is specified. **UNVERIFIED — PLANNED.** |
| `--file-extension-fallback` | README.md:221-222; src/gource_settings.cpp:1511-1513 | **Deferred** | Reject until display-only path projection is specified. **UNVERIFIED — PLANNED.** |
| `--user-image-dir DIRECTORY` | README.md:236-237; src/gource_settings.cpp:803-863 | **Deferred** | Local images require bounded decode/cache and provenance; no auto remote assets. **UNVERIFIED — PLANNED.** |
| `--default-user-image IMAGE` | README.md:240-241; src/gource_settings.cpp:796-801 | **Deferred** | Same bounded/provenance policy. **UNVERIFIED — PLANNED.** |
| `--fixed-user-size` | README.md:243-244; src/gource_settings.cpp:1381-1383 | **Deferred** | Reject until contributor sprite policy exists. **UNVERIFIED — PLANNED.** |
| `--colour-images` | README.md:246-247; src/gource_settings.cpp:754-756 | **Deferred** | Reject until asset shader path exists. **UNVERIFIED — PLANNED.** |
| `--crop AXIS` | README.md:249-250; src/gource_settings.cpp:758-771 | **Deferred** | Use explicit viewport/export crop later; reject unknown axis. **UNVERIFIED — PLANNED.** |
| `--padding FLOAT` | README.md:252-253; src/gource_settings.cpp:1466-1475 | **Deferred** | Camera bounds tuning later; not silently converted to viewport padding. **UNVERIFIED — PLANNED.** |
| `--multi-sampling` | README.md:255-256; src/main.cpp:130-136 | **Deferred** | wgpu sample count must be adapter-validated; reject until target pipeline supports it. **UNVERIFIED — PLANNED.** |
| `--no-vsync` | README.md:258-259; src/main.cpp:143-148 | **Deferred** | Presentation-only swapchain policy; export is fixed-time regardless. **UNVERIFIED — PLANNED.** |
| `--bloom-multiplier FLOAT` | README.md:261-262; src/gource_settings.cpp:963-972 | **Deferred** | Advanced effect after baseline renderer; reject until measured/configurable. **UNVERIFIED — PLANNED.** |
| `--bloom-intensity FLOAT` | README.md:264-265; src/gource_settings.cpp:952-961 | **Deferred** | Advanced effect after baseline renderer; reject until measured/configurable. **UNVERIFIED — PLANNED.** |

## 5. Legacy option inventory: limits, users, and camera input

| README option | Inspected evidence | Successor status | Rejection/replacement and check |
|---|---|---|---|
| `--max-files NUMBER` | README.md:267-269; src/gource.cpp:991-1001 | **Intentionally changed** | Never discard required history when a visible-object limit is hit. Reject this legacy option and use explicit finite resource limits/LOD. **UNVERIFIED — PLANNED.** |
| `--max-file-lag SECONDS` | README.md:272-274; src/gource_settings.cpp:1400-1409 | **Deferred** | Legacy commit-lag visual scheduling is not canonical replay order. **UNVERIFIED — PLANNED.** |
| `--max-user-speed UNITS` | README.md:277-278; src/gource_settings.cpp:1435-1444 | **Deferred** | Later simulation control, subject to D09 reference. **UNVERIFIED — PLANNED.** |
| `--user-friction SECONDS` | README.md:280-281; src/gource_settings.cpp:1411-1422 | **Deferred** | Later simulation control; reject until numeric policy is fixed. **UNVERIFIED — PLANNED.** |
| `--user-scale SCALE` | README.md:283-284; src/gource_settings.cpp:1424-1433 | **Deferred** | Later render/simulation control; reject until identity boundary is fixed. **UNVERIFIED — PLANNED.** |
| `--disable-auto-rotate` | README.md:289-290; src/gource_settings.cpp:712-714 | **Deferred** | Automatic camera policy is canonical in D11; reject until the exact policy is implemented. **UNVERIFIED — PLANNED.** |
| `--disable-input` | README.md:292-293; src/gource_settings.cpp:720-722 | **Deferred** | App input policy later; export remains noninteractive. **UNVERIFIED — PLANNED.** |

## 6. Legacy option inventory: captions, output, and config

| README option | Inspected evidence | Successor status | Rejection/replacement and check |
|---|---|---|---|
| `--caption-file FILE` | README.md:315-316; src/gource.cpp:1082-1129 | **Deferred** | Caption input is later; reject rather than silently omit captions. **UNVERIFIED — PLANNED.** |
| `--caption-size SIZE` | README.md:318-319; src/gource_settings.cpp:888-897 | **Deferred** | Requires caption renderer; reject. **UNVERIFIED — PLANNED.** |
| `--caption-colour FFFFFF` | README.md:321-322; src/gource_settings.cpp:906-922 | **Deferred** | Requires caption renderer; reject. **UNVERIFIED — PLANNED.** |
| `--caption-duration SECONDS` | README.md:324-325; src/gource_settings.cpp:877-886 | **Deferred** | Requires caption lifecycle; reject. **UNVERIFIED — PLANNED.** |
| `--caption-offset X` | README.md:327-328; src/gource_settings.cpp:899-904 | **Deferred** | Requires caption renderer; reject. **UNVERIFIED — PLANNED.** |
| `-o`, `--output-ppm-stream FILE` | README.md:330-334; src/main.cpp:185-201 | **Intentionally changed** | Legacy PPM stream is not the successor output contract. Use bounded frame-image export with explicit target/timestamps; a request for this spelling is rejected. **UNVERIFIED — PLANNED.** |
| `-r`, `--output-framerate FPS` | README.md:336-337; src/gource.cpp:505-522 | **Planned support / intentionally generalized** | Fixed rational FPS for export, not only 25/30/60 and not wall-clock tick selection. **UNVERIFIED — PLANNED.** |
| `--output-custom-log FILE` | README.md:339-340; src/gource.cpp:171-214 | **Deferred** | Exporting normalized custom logs is later; use the input custom format for fixtures. **UNVERIFIED — PLANNED.** |
| `--load-config CONFIG_FILE` | README.md:342-343; src/main.cpp:84-91 | **Supported contract / intentionally changed** | Schema-1 TOML only; old config migration is explicit and loss-detecting. **UNVERIFIED — PLANNED.** |
| `--save-config CONFIG_FILE` | README.md:345-346; src/main.cpp:112-115 | **Supported contract / intentionally changed** | Save effective schema-1 config atomically; no repeated-section playlist serialization. **UNVERIFIED — PLANNED.** |
| `--path PATH` | README.md:348-349 | **Supported contract** | Same as core CLI table. **UNVERIFIED — PLANNED.** |

## 7. C++-registered options and aliases not fully described by README

These names are listed to prevent omission-based support claims. Registration is
visible in `src/gource_settings.cpp:211-366`; it is not successor support.

| Registered legacy name/alias | Inspected evidence | Successor status | Rejection/replacement and check |
|---|---|---|---|
| `--dont-stop` | src/gource_settings.cpp:256-259,1368-1370 | **Deferred** | Finite end-of-history is the first contract; reject until an explicit live/keep-window mode exists. **UNVERIFIED — PLANNED.** |
| `--stop-on-idle` | src/gource_settings.cpp:256-258,1376-1379 | **Intentionally changed** | C++ itself notes this “no longer does anything”; successor rejects instead of accepting a no-op. **UNVERIFIED — PLANNED.** |
| `--disable-auto-skip` | src/gource_settings.cpp:283-285,716-718 | **Planned support** | Explicit inverse of `--auto-skip-seconds` may map to a disabled policy; validation remains planned. **UNVERIFIED — PLANNED.** |
| `--ffp` | src/gource_settings.cpp:280-281,1356-1358 | **Unsupported** | OpenGL fixed-function mode has no wgpu successor; use the baseline renderer. **UNVERIFIED — PLANNED.** |
| `--disable-bloom`, `--disable-progress`, `--highlight-all-users` | src/gource_settings.cpp:233-236 | **Deferred aliases** | Reject until the corresponding successor render/UI elements exist; no silent alias. **UNVERIFIED — PLANNED.** |
| `--background` | src/gource_settings.cpp:233-234 | **Supported contract alias** | Explicit alias of `--background-colour` after the target value is validated. **UNVERIFIED — PLANNED.** |
| `--git-log-command`, `--cvs-exp-command`, `--cvs2cl-command`, `--hg-log-command`, `--bzr-log-command`, `--svn-log-command` | src/gource_settings.cpp:238-249,555-581 | **Intentionally changed / unsupported** | No arbitrary command display/execution; use fixed adapter diagnostics or custom-log conversion. **UNVERIFIED — PLANNED.** |
| `--log-level` | src/gource_settings.cpp:250-251,588-600 | **Planned support** | Bounded diagnostics verbosity in schema/CLI; invalid value rejects. **UNVERIFIED — PLANNED.** |
| Short aliases `-p,-a,-s,-t,-i,-e,-h,-H,-b,-c` | src/gource_settings.cpp:221-232 | **Supported where their long option is supported** | `-H` extended help is planned; aliases do not expand the supported option set. **UNVERIFIED — PLANNED.** |

## 8. Interactive commands and visible behavior

The README documents keyboard commands at `README.md:451-471`. They are listed
so a missing UI command cannot be mistaken for support.

| Legacy command | Successor status | Replacement/rejection and check |
|---|---|---|
| `SPACE` pause/resume | **Supported contract** | App command; replay clock pauses while presentation may update. **UNVERIFIED — PLANNED.** |
| `V` camera mode | **Planned support** | Toggle canonical overview/track policy; exact camera state is checkpointed. **UNVERIFIED — PLANNED.** |
| `C` logo | **Deferred** | Reject until logo asset support. **UNVERIFIED — PLANNED.** |
| `K` extension key | **Deferred** | Reject until key UI. **UNVERIFIED — PLANNED.** |
| `M` mouse visibility | **Deferred** | Reject until cursor/input policy. **UNVERIFIED — PLANNED.** |
| `N` next log entry | **Planned support** | Advance to the next canonical event boundary. **UNVERIFIED — PLANNED.** |
| `S` randomize colours | **Intentionally changed** | Colours are deterministic keyed values; reject nondeterministic recolour. **UNVERIFIED — PLANNED.** |
| `D`, `F`, `U`, `G`, `T`, `R` display toggles | **Deferred** | Per-element render controls later; unknown/absent elements reject. **UNVERIFIED — PLANNED.** |
| `<` / `>` time-scale adjustment | **Planned support** | Validated replay command; generation/checkpoint behavior is explicit. **UNVERIFIED — PLANNED.** |
| `+` / `-` simulation speed | **Planned support** | Validated replay command; no float drift. **UNVERIFIED — PLANNED.** |
| Keypad `+` / `-` camera zoom | **Planned support** | Presentation-only manual view; not replay state. **UNVERIFIED — PLANNED.** |
| `TAB` cycle visible users | **Deferred** | Requires contributor selection UI. **UNVERIFIED — PLANNED.** |
| `F12` screenshot | **Planned support** | Explicit frame-image output path; no implicit PPM stream. **UNVERIFIED — PLANNED.** |
| `Alt+Enter` fullscreen | **Planned support** | Presentation-only; equivalent to validated fullscreen command. **UNVERIFIED — PLANNED.** |
| `ESC` quit | **Supported contract** | Cancel/close app and reap owned resources. **UNVERIFIED — PLANNED.** |
| Mouse inspection, middle-button camera toggle, left drag, right rotation | **Planned/deferred split** | Selection/overview interactions are planned; legacy exact gesture parity is deferred. Manual view cannot mutate replay world. **UNVERIFIED — PLANNED.** |

## 9. Behavioral compatibility matrix

| Behavior | Inspected evidence | Successor status | Contract and check |
|---|---|---|---|
| Tree root/branches/files | README.md:11-14; src/dirnode.cpp:377-496 | **Supported contract** | Compressed component-radix hierarchy, stable IDs, file/dir conversion. **UNVERIFIED — PLANNED.** |
| Contributor activity | README.md:11-14; src/gource.cpp:1244-1290 | **Supported contract** | Exact source identity, deterministic actions/timers, bounded snapshots. **UNVERIFIED — PLANNED.** |
| Add/modify/delete actions | README.md:407-410; src/gource.cpp:1277-1289 | **Supported contract / tightened** | Strict A/M/D; duplicates retained; unsupported actions reject. **UNVERIFIED — PLANNED.** |
| Directory deletion | src/gource.cpp:1196-1229 | **Supported contract** | Trailing-slash `D` recursively deletes descendants; A/M directory targets reject. **UNVERIFIED — PLANNED.** |
| File-to-directory conversion | src/dirnode.cpp:432-454 | **Supported contract** | Deterministic lifecycle transition; no history truncation. **UNVERIFIED — PLANNED.** |
| Invalid UTF-8 handling | src/formats/commitlog.cpp:24-35 | **Intentionally changed** | Custom invalid UTF-8 rejects; source bytes and display escaping remain separate. **UNVERIFIED — PLANNED.** |
| Path separators/root | src/formats/commitlog.cpp:290-300; src/dirnode.cpp:141-149 | **Intentionally changed/tightened** | Lexical `/` only, explicit virtual root, no host canonicalization or traversal components. **UNVERIFIED — PLANNED.** |
| Equal/out-of-order timestamps | src/gource.cpp:1758-1769 | **Intentionally changed** | Canonical `(timestamp, source_sequence)` ordering and monotonic clock. **UNVERIFIED — PLANNED.** |
| Playback speed/idle skip | README.md:100-118; src/gource.cpp:1722-1756 | **Supported contract** | Rational repository clock at 120-Hz simulation; deterministic idle jump. **UNVERIFIED — PLANNED.** |
| Backward seek | README.md:82-86; src/gource.cpp:1067-1078 | **Supported contract / tightened** | Full-state restore + forward replay; timestamp-only seek is invalid. **UNVERIFIED — PLANNED.** |
| Loop/end behavior | README.md:91-98; src/gource.cpp:1707-1713 | **Deferred** | First slice stops at finite end; loop options reject. **UNVERIFIED — PLANNED.** |
| Overview/track camera | README.md:286-287; src/gource.cpp:1475-1550 | **Supported contract** | Canonical auto camera; manual presentation override. **UNVERIFIED — PLANNED.** |
| Supplied render target | SPEC.md:147-151; src/gource.cpp:2401-2475 | **Intentionally changed** | Renderer accepts target view for window/offscreen; old display-owned OpenGL path is not copied. **UNVERIFIED — PLANNED.** |
| Gamma/blending | src/textbox.cpp:118-158; src/gource.cpp:2477-2479 | **Intentionally changed/defined** | Gamma-space premultiplied shaders, RGBA8Unorm, `ONE,ONE_MINUS_SRC_ALPHA`, one final presenter conversion; prevent double gamma. **UNVERIFIED — PLANNED.** |
| Filters | src/formats/commitlog.cpp:325-350,357-384 | **Supported contract / tightened** | Exclusions OR; repeated show filters AND; bounded regex subset; replay identity changes. **UNVERIFIED — PLANNED.** |
| Git command construction | src/formats/git.cpp:88-193; src/formats/commitlog.cpp:93-96 | **Intentionally changed** | Fixed argv, no shell/chdir/interpolated path, bounded stderr, exit/EOF checks. **UNVERIFIED — PLANNED.** |
| Config precedence | src/main.cpp:35-115; src/gource_settings.cpp:611-619 | **Intentionally changed** | Schema-1 TOML, built-ins < file < CLI < transient UI; explicit migration. **UNVERIFIED — PLANNED.** |
| Resource limits | SPEC.md:106-110,139-145; src/gource.cpp:991-1001 | **Intentionally changed** | Errors on limits; never silently discard events or publish prefixes. **UNVERIFIED — PLANNED.** |
| License/attribution | README.md:473-490; source headers e.g. src/formats/custom.cpp:1-16 | **Supported contract** | GPL-3.0-or-later notices and SPDX attribution for translated/derived modules. **UNVERIFIED — PLANNED.** |

## 10. Formats and option migration examples

```text
# Supported first-slice custom input
0|Alice|A|src/main.rs
1|Alice|M|src/main.rs|#80c0ff
2|Alice|D|src/main.rs
```

```text
# Deferred legacy format
successor: unsupported-option: --log-format svn — deferred; convert svn log to strict custom format

# Intentionally changed unsafe command option
successor: unsupported-option: --log-command — intentionally changed; use --log-format git (planned) or custom input

# Intentionally changed silent truncation option
successor: unsupported-option: --max-files — intentionally changed; configure finite resource limits/LOD

# Legacy configuration
successor: legacy-config-requires-migration: use explicit schema-1 migration; no lossy repeated sections
```

Exact error wording may evolve, but the status, option, and actionable
replacement MUST remain present. No deferred or unsupported row becomes
implicitly supported by omission from a future parser.

## 11. Compatibility checks

All are **UNVERIFIED — PLANNED** and must be executed by the owning phase:

1. Every custom grammar boundary and malformed record produces the documented
   error rather than EOF; equal-time/duplicate/order output matches the source
   sequence contract.
2. Hierarchy fixtures cover empty/single event, deep/common prefixes,
   Unicode/unusual paths, deletion/recreation, file-to-directory conversion,
   and trailing-slash subtree deletion.
3. Core CLI checks exercise each Supported contract row and each known
   deferred/intentionally-changed/unsupported row, asserting clear rejection.
4. Seeked state matches fresh forward replay at event, idle, camera, and
   deletion boundaries; render-only config changes do not reparse.
5. Window and offscreen targets produce the same canonical RGBA8Unorm/gamma
   policy; export schedules fixed ticks and drops no frame.
6. Git hostile-path/ref/config and cancellation scenarios prove no shell text,
   network fetch, leaked child, partial cache, or unbounded stderr.
7. Clean C++ behavior remains available and is not removed as part of successor
   compatibility work.

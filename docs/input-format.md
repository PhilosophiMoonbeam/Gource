# Strict finite custom-log format

**Implementation status:** the grammar and behavior below are **planned support**
for the first Rust input slice. The parser/indexer check is **UNVERIFIED —
PLANNED**. This document records the successor contract; it does not claim that
the C++ parser already enforces it.

## 1. Source and framing

A custom log is a finite regular file or a finite stdin stream. The reader MUST
consume the source through EOF before publishing an indexed history. Empty input
is a valid empty history. A pipe that remains open, a tailing file, or a stream
that cannot reach EOF is not a first-slice custom input; live/infinite input is
**deferred** because it cannot provide canonical global ordering or deterministic
backward seeking.

The stream is bytes. Limits are checked before allocations grow:

| Resource | Initial bound | Failure behavior |
|---|---:|---|
| One physical record, including delimiter | 1 MiB | Fatal `record-too-large` with byte offset and line number. |
| Whole finite source / spool | 8 GiB | Fatal `input-too-large`; no partial index. |
| Path bytes | 64 KiB | Fatal `path-too-long`. |
| Contributor bytes | 4 KiB | Fatal `contributor-too-long`. |
| Path components | 256 | Fatal `path-too-deep`. |
| Compiled filter expression | 4 KiB input / 1 MiB representation | Fatal configuration error. |

The configured limits are finite and may be raised explicitly. There is no
unlimited sentinel. A limit error is never treated as EOF and never truncates
the history.

The only permitted record terminators are LF (`0x0A`) and CRLF (`0x0D 0x0A`). A
final terminator is optional for the final record. A lone CR, NUL, or embedded
line terminator is invalid. A UTF-8 BOM (`EF BB BF`) is permitted exactly once,
only at byte offset zero, and is removed before parsing the first timestamp.
Blank lines, comments, and whitespace-only records are not grammar productions
and are rejected. Leading/trailing spaces are data, not implicit trimming.

The legacy parser's regex is not a safe grammar: it accepts the optional BOM,
pipe-delimited fields, and action/colour forms but has no end anchor
(`src/formats/custom.cpp:21`). The successor therefore requires complete record
consumption and reports trailing bytes instead of treating them as an accepted
suffix.

## 2. Record grammar

The following ABNF-like notation is byte-oriented. `LF` means `0x0A` and the
optional `CR` is accepted only immediately before `LF`.

```text
stream       = [ BOM ] *( record LF ) [ record [ LF ] ]
record       = timestamp "|" username "|" action "|" path [ "|" colour ]
BOM          = %xEF. BB. BF

username     = *( username-byte )
action       = "" / "A" / "M" / "D"
path         = path-byte 1*( path-byte )
colour       = [ "#" ] HEXDIG HEXDIG HEXDIG HEXDIG HEXDIG HEXDIG

; In all byte fields, "|", LF, CR, and NUL are forbidden.
; timestamp has the alternatives in section 3.
```

There are exactly four fields (`timestamp|username|action|path`) or exactly five
fields with a non-empty colour field. A fifth empty field is invalid; a sixth
field is invalid. There is no escape, quote, or continuation syntax. Therefore a
literal pipe, newline, or carriage return cannot occur in a username or path.
If an upstream producer needs such data it MUST encode it into a different
source format before conversion to custom log; the successor does not guess an
escape convention.

### Compatibility normalizations

These are the only legacy conveniences retained:

- Empty `username` normalizes to the literal contributor key `Unknown`.
- Empty `action` normalizes to `A`.
- A colour may be six ASCII hexadecimal digits with or without one leading `#`.
  Digits are case-insensitive and are normalized to three bytes `(R,G,B)`.
- UTF-8 is validated. Invalid UTF-8 is a fatal `invalid-utf8` error, not a
  replacement character. This intentionally differs from
  `RCommitLog::filter_utf8`, which replaces invalid sequences
  (`src/formats/commitlog.cpp:24-35`).

A syntactically valid record can still fail semantic validation (for example,
an add/modify action against a directory target). Such failures have their own
stable error code and location.

## 3. Timestamp grammar and normalization

`timestamp` is one of the following complete forms:

1. **Epoch seconds:** an optional `-` followed by one or more ASCII digits.
   The value is parsed as a signed 64-bit integer; `0` and negative values are
   valid when representable. Leading `+`, decimal fractions, overflow, and
   surrounding whitespace are rejected.
2. **RFC3339-compatible timestamp:**
   `YYYY-MM-DDTHH:MM:SS[.fraction]Z` or the same with an explicit `+HH:MM` or
   `-HH:MM` offset. The first implementation accepts no fractional seconds
   (the fraction is rejected rather than rounded); leap-second `:60` spellings,
   invalid calendar dates, offset overflow, and year/range overflow are errors.
3. **Legacy date compatibility form:**
   `YYYY-MM-DD`, `YYYY-MM-DD HH:MM`, or `YYYY-MM-DD HH:MM:SS`, optionally followed
   by an explicit numeric `+HH[:MM]`/`-HH[:MM]` offset. An omitted offset means
   UTC in the successor, never the host local timezone.

All accepted forms normalize to `Timestamp(i64)` epoch seconds. The original
spelling MAY be retained as diagnostic metadata but MUST NOT affect ordering.
The UTC choice and strict overflow/date handling are intentional changes from
host-local, permissive date parsing. The C++ custom parser delegates date strings
to `parseDateTime` and parses other values with `atoll`
(`src/formats/custom.cpp:58-68`); that behavior is evidence of the input forms,
not a requirement to retain its silent overflow/truncation.

## 4. Paths and target kinds

Paths are repository-lexical names, not host filesystem paths:

- Components are separated only by `/`; backslash is an ordinary byte and is
  not converted to a separator.
- One leading `/` is accepted as the legacy virtual-root notation and removed
  before identity interning. A path is not resolved against the process cwd.
- Interior empty components, `.` and `..` components, NUL, and an empty path are
  rejected. Case is significant; Unicode is not normalized or case-folded.
- A final `/` marks an explicit directory target. A non-final path is a file
  target. The path remains lexical and is never stat'ed, canonicalized,
  symlink-followed, or used as a temporary filename.
- Custom input must be valid UTF-8, but identity is based on normalized source
  bytes/components rather than display-font substitution. Display escaping is a
  renderer concern.

The old normalizer prepends `/` to every non-root path
(`src/formats/commitlog.cpp:290-300`), and the hierarchy uses slash-prefixed
paths when creating and finding nodes (`src/dirnode.cpp:377-416`). The successor
makes the virtual-root step explicit and rejects traversal/aliasing inputs rather
than allowing host path semantics.

Action/target rules:

| Target | `A` | `M` | `D` |
|---|---|---|---|
| File | Create/replace the current file incarnation; if already present, the lifecycle guard determines whether this is a duplicate create. | Modify the current file; an absent target is a typed lifecycle error unless the configured compatibility mode explicitly maps it to create (default: reject). | Delete the current file incarnation; an absent target is retained in history but has no visual side effect. |
| Directory (trailing `/`) | Reject as `directory-action-unsupported`. | Reject as `directory-action-unsupported`. | Recursively delete every current descendant file under the lexical prefix. |

The directory-delete behavior mirrors `processCommit`, which expands a trailing
slash delete over recursive files and ignores non-delete directory actions
(`src/gource.cpp:1188-1229`). Duplicate records are not collapsed; their action
is applied in canonical event order and stale/absent-target behavior is visible
through a typed error or no-op as specified above.

## 5. Canonical ordering and identity

During the first pass, each physical record receives a monotonically increasing
`source_sequence: u64` before any sorting, filtering, or interning. The canonical
key is:

```text
EventKey = (timestamp: i64, source_sequence: u64)
```

The complete finite event stream is sorted ascending by that key. Equal
timestamps therefore preserve source order. Duplicate records remain separate
events with distinct source sequences. No timestamp or sequence is synthesized
from a hash-table order, thread completion order, filesystem mtime, or wall
clock. A malformed record consumes no published sequence and aborts the import.

The legacy parser groups adjacent same-timestamp/same-user lines into one
`RCommit` (`src/formats/custom.cpp:73-84`) and `RCommitLog::nextCommit` consumes
records in source order (`src/formats/commitlog.cpp:216-235`); it does not define a
global sort. The successor's global finite sort and duplicate retention are an
intentional deterministic change.

Filtering does not mutate the source index. File/user filters are replay
configuration and produce a new replay identity; they cannot silently delete
source events from the cached canonical stream. This avoids the legacy behavior
where a filter can cause `RCommit::addFile` or `RCommit::isValid` to discard a
record before application (`src/formats/commitlog.cpp:325-350`, `357-384`).

## 6. Errors and diagnostics

Every fatal input error contains:

- stable machine-readable code (`record-too-large`, `wrong-field-count`,
  `invalid-timestamp`, `invalid-action`, `invalid-colour`, `invalid-path`,
  `invalid-utf8`, `directory-action-unsupported`, `resource-limit`, etc.);
- one-based physical line number and zero-based byte offset when available;
- bounded escaped context (never an unbounded copy of an attacker-controlled
  record); and
- whether parsing, normalization, sorting, or index publication failed.

The reader MUST distinguish malformed data from clean EOF. It MUST NOT skip a bad
line and continue, treat trailing junk as a valid record, silently discard an
oversized event, or publish a prefix as a successful history. Stdin and regular
file errors include the source kind and OS error, with no repository data dumped
by default.

## 7. Examples

Lexically and semantically accepted:

```text
0|Alice|A|src/main.rs
2024-01-01T00:00:00Z|Bob|M|README.md|#80c0ff
2024-01-01 00:00:01 +02:00||D|old.txt
-1|Unknown|D|/docs/
```

Lexically valid but semantically rejected:

```text
-1|Unknown||/docs/
```

The last line normalizes to user `Unknown`, action `A`, and a directory target;
add/modify directory targets are rejected as `directory-action-unsupported`.
Use the trailing-slash `D` form above to delete that directory subtree.

Rejected:

```text

# comment
1|a|A|a|ffffff|extra       # sixth field/trailing data
1|a|X|a                  # unsupported action
1|a|A|a/../b             # traversal component
1|a|A|a|gggggg           # non-hex colour
1|a|A|a|                 # empty fifth field
```

The comments after the examples are explanatory text, not part of the format.

## 8. Planned checks

The following checks are required before this contract may be marked supported;
all are currently **UNVERIFIED — PLANNED**:

1. Empty input, one event, final-newline/no-final-newline, LF/CRLF, BOM-at-start,
   zero/negative timestamps, strict date offsets, Unicode, and exact four/five
   fields.
2. Equal timestamps, duplicate records, shuffled timestamps, source-sequence
   stability, and external-sort output equivalence.
3. Every malformed case above, including bad UTF-8, NUL, lone CR, trailing junk,
   overflow, and record/whole-input caps; verify errors are not EOF.
4. Path identity for leading virtual root, slash/backslash, Unicode forms,
   case-distinct names, deep trees, file-to-directory conversion, and trailing
   slash deletion/recreation.
5. Lifecycle application against a fresh replay and after complete seek restore.

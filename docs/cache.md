# Persistent history cache and bounded storage

The Rust application has an opt-in, persistent cache for complete indexed
histories. It is separate from parser working memory, replay checkpoints, GPU
readback, and export staging. Those other allocations are bounded working state
for one command; they are not reusable cache entries.

## Enabling the cache

Caching is disabled unless `--cache-dir` (or `cache_dir`/`GOURCE_CACHE_DIR`) is
set:

```sh
gource-app export \
  --input /tmp/history.log \
  --cache-dir /tmp/private-gource-cache \
  --cache-bytes 1073741824 \
  --output /tmp/history-frames
```

The application uses the cache only when `--input` names an existing regular
file. A finite stdin source (`--input -`) and a directory source selected for
Git ingestion bypass persistent cache lookup and storage. `view`, `export`, and
`diagnose` share the same loader, so a regular custom-log file can be reused by
any of those commands when they select the same cache directory and applicable
limits.

The default application quota is 4 GiB (`4 * 1024^3`) for published entry
files. `--cache-bytes` must be finite and greater than zero; zero and
`u64::MAX` are rejected. The application uses the selected value as the maximum
size of one entry as well as the aggregate published-entry quota. The cache API
also has independent defaults of 4,096 directory entries and 16 MiB of
accounted eviction metadata. Those bounds are not unlimited sentinels and are
not exposed as separate CLI options.

A cache is a performance and disk-reuse feature, not a source of truth. If it
is disabled, unavailable, empty, stale, or corrupt, the command parses and
indexes the selected source normally. Cache failures that indicate unsafe
configuration or I/O are reported; a malformed entry is removed safely and
handled as a miss where possible.

## Key and contents

Each key includes:

- a BLAKE3 digest and byte length of the exact finite regular input file;
- the cache format/schema version and the core catalog, event, and history
  schema versions; and
- an ingest-options fingerprint covering the dataset-affecting parser/index
  limits and run policy, plus a limit envelope used during decoding.

Changing input bytes, file length, schema, or a key-affecting ingest limit
therefore selects a different entry. Replay presentation, transient UI state,
and cancellation are not cache identity. Path filters are applied after a
cached canonical history is loaded; they create a filtered in-memory event
view and do not create a second filtered cache file.

A published entry contains a complete normalized catalog and canonical event
history, including repository path and contributor names needed to replay it.
It is not encrypted. The privacy boundary is filesystem ownership/ACL
protection, not cryptography: anyone who can legitimately read the private
cache directory can inspect its repository metadata.

A cache hit is accepted only after all of the following have succeeded:

1. the held cache directory still has the identity and privacy properties
   checked when it was opened;
2. the entry is a regular file of an allowed size and has a valid header,
   schema, key, counts, and payload lengths;
3. the complete checksum matches; and
4. decoding constructs a complete immutable history within the configured
   limits.

A malformed, truncated, mismatched, oversized, symlinked, or checksum-invalid
entry is not exposed to replay. The implementation removes the entry only
when the file identity check still refers to the file it inspected, so a
concurrent replacement is not accidentally deleted.

## Privacy and filesystem safety

The cache root is created as a private directory (`0700` on supported Unix
systems and an owner-only ACL on Windows). Existing roots must satisfy the same
privacy check; a symlink root, a root owned by another Unix user, or a root
with group/other access is rejected. On platforms where private ACL validation
cannot be guaranteed, opening the cache fails rather than silently weakening
privacy. Cache files are created with private permissions (`0600` on supported
Unix systems or an owner-only ACL on Windows).

The implementation holds directory capabilities where supported, rejects final
symlinks, and rechecks directory identity before and after operations. A
bounded lock (`.cache.lock`) serializes scans and publication; waiting for the
lock is limited to ten seconds. Unknown directory entries are rejected during
quota scans instead of being treated as cache data.

These controls do not make the cache a sandbox. An attacker with write access
to the parent filesystem can still race directory creation before the private
root exists, delete the root, deny service, or read data with equivalent OS
privileges. Use a dedicated private parent and normal filesystem permissions
when repository metadata is sensitive. Do not place secrets in input paths or
assume a cache digest hides the source contents.

## Quotas and eviction

The cache enforces all of these bounds before publishing an entry:

| Bound | Default | Effect |
| --- | ---: | --- |
| Aggregate published entry bytes | 4 GiB | Oldest valid entries are evicted until the incoming entry fits. |
| One entry bytes | Cache quota in the app; 4 GiB in the API default | Oversized entries are rejected or treated as misses. |
| Directory entries inspected | 4,096 | A scan above the bound fails rather than allocating unbounded metadata. |
| Eviction metadata | 16 MiB | A scan above the bound fails rather than growing metadata without limit. |
| Lock wait | 10 seconds | A busy cache returns a bounded lock-timeout error. |

Files with the known `.bin` entry, `.tmp` staging, or `.cache.lock` names are
recognized. Stale temporary files are removed during a bounded eviction scan;
unknown names cause an error. Entries are ordered by bounded filesystem
modification metadata and oldest entries are removed first. An entry being
replaced is protected from eviction while its replacement is prepared.

The quota covers the published entries considered by the eviction scan. A
private staging file exists briefly during a write; the writer still accounts
for the incoming entry before it starts and removes the staging file on error.
There is no crash-recovery journal or unlimited spill area.

## Atomic publication and failure behavior

A cache store follows this sequence:

1. validate the complete immutable history and key identity;
2. encode the bounded header and payload, including a checksum;
3. acquire the bounded directory lock and evict within quota;
4. create a uniquely named private `.tmp` file;
5. write and synchronize the complete entry;
6. atomically rename it to the digest-derived `.bin` name and synchronize the
   directory; and
7. recheck the held directory identity/privacy state.

Parse, sort, normalization, cancellation, allocation, quota, or publication
failure never publishes a history prefix and removes the private staging file
when possible. Replacing an entry is a complete-file operation; readers either
see the previous complete entry or the new complete entry, not a partially
written payload.

Cache loading is also complete-before-use. A hit is decoded into temporary
values and returned only after all bytes, counts, limits, and checksum checks
succeed. Removing a cache directory or deleting an entry changes only reuse;
it does not change the canonical result of parsing the source again.

## Working-state bounds that are not the cache

The source parser/indexer has separate defaults of 1 MiB per physical record,
8 GiB per finite source, 64 KiB per path, 4 KiB per contributor, 256 path
components, 128 MiB working memory, 16 GiB temporary sort disk, and merge
fan-in 32. The application event-count default is `u64::MAX`; set
`--max-events` to impose a lower bound. External sort runs are private
`TempDir` files and are removed when their state is dropped.

Replay checkpoints (64 entries), latest-wins snapshot storage (three snapshots),
renderer buffers, GPU readback, and FFmpeg's three-frame queue are bounded
working resources. Export staging is deliberately not a cache: a PNG directory
or video is published only after the complete operation and manifest succeed.
See [`export.md`](export.md) for output staging and
[`security.md`](security.md) for process/filesystem boundaries.

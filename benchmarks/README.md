# Benchmark and validation fixtures

This directory contains the stage-one history workload contract. The custom-log
inputs and expectation manifests live in `../tests/fixtures/`; the benchmark
fixture directory contains only a deterministic generator and its configuration.
Generated history output is intentionally not committed as a large blob.

## Custom-log contract

The compact fixtures use the existing custom-log record shape:

```text
timestamp|contributor|action|path[|#rrggbb]
```

`A`, `M`, and `D` mean add, modify, and delete. The optional colour is six
hexadecimal digits, with or without `#`. Paths and contributor names may contain
spaces and UTF-8 characters, but `|` remains the record delimiter.

`tests/fixtures/index.json` is the catalog. Each `.log` has a matching
`.expect.json` manifest. Manifests record accepted source lines, expected
normalized ordering, final hierarchy state, contributor/action counts, and
scenario-specific outcomes. `contributors` uses first-seen order; hierarchy
paths are rooted and `directories` excludes the implicit `/` root.

The intended successor normalization is a stable `(timestamp,
source_sequence)` order. Equal timestamps therefore retain source order;
duplicates remain events rather than being silently collapsed; signed zero and
negative timestamps are retained. This is an intentional stage-one contract,
not a claim that the current C++ parser already performs global reordering.

The catalog records status explicitly:

- `inspected`: evidence read from the existing application or legacy fixtures.
- `planned`: the stage-one Rust support contract.
- `intentional`: a deliberate successor normalization or compatibility choice.
- `deferred`: outside this fixture slice and not silently assumed supported.
- `unsupported`: input that must be rejected with the named error.
- `verified`: reserved for an outcome observed by an executable check.
- `not-run`: no executable check is claimed by the manifest.

The malformed-record manifest names the expected error for every rejected line,
while also pinning the existing empty-contributor (`Unknown`) and empty-action
(`A`) defaults. Cancellation and encoder-failure manifests describe deterministic
injection points and required cleanup without pretending that either failure has
already been exercised.

The visual fixture (`visual-hierarchy-activity.log`) deliberately contains
nested branches, four active file nodes, three contributors, add/modify/delete
activity, and a 60-second idle gap. Legacy behavior is referenced, not copied,
through the paths listed in `tests/fixtures/index.json` (including deletion,
file-to-directory, directory-delete, and UTF-8 logs).

## Deterministic large-history workload

Generate the benchmark input on demand from the repository root:

```sh
python3 benchmarks/fixtures/generate_large_history.py \
  --config benchmarks/fixtures/large_history.config.json \
  --output /tmp/gource-large-history.log
```

The committed configuration uses seed `424242` and describes 1,536 events in
three phases: low visible-object count (8 target files), high visible-object
count (128 target files), and a low-count tail (16 target files). It uses 32
contributors, six top-level directories, equal-timestamp opportunities, and a
7,200-second idle gap every 256 events. The expectation manifest records the
phase line ranges and workload identity without embedding generated output.

Use `--seed N` to make an explicit alternate workload while keeping the same
shape. For a fixed generator, configuration, and seed, output is deterministic;
the seed and config path must be recorded with benchmark results. The generator
writes only custom-log records to stdout or the requested output path and has no
network, repository, or asynchronous-runtime dependency.

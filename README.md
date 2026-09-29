# lance-compression-inspector

Inspect a Lance dataset snapshot and identify compression opportunities.

The workspace separates the reusable analysis library from presentation:

- `lance-compression-estimation`: footer probing, report models, rules, and
  measured sample comparisons. Probe and estimation remain separate Rust modules.
- `lance-compression-cli`: human and versioned JSON reports.

The estimator models a full `EncodingPlan` instead of a flat codec: structural
layout (`auto`, miniblock, fullzip, or 2.3-only sparse), controllable value
encoding (RLE, FSST, byte-stream split, dictionary, or packed struct), general
compression (metadata-preserving baseline, none, LZ4, or ZSTD levels
1/3/6/9/12), and target file version. Bitpacking and constant encoding remain
automatic because Lance does not expose honest writer controls for them.

Candidate selection is deliberately bounded rather than Cartesian. Per
top-level field it first measures the baseline, every general compressor, and
each type/version-compatible structural or value axis candidate. It retains the
best structural and value result, optionally combines those two, and measures
that beam with none, LZ4, ZSTD-3, and ZSTD-9 (BSS uses only real compressors;
FSST cannot be combined because it shares Lance's compression control). This
keeps the worst-case set to roughly two dozen rewrites per field.

Ranking uses measured sample bytes by default. `--consider-decoding-penalty` enables
DuckDB-inspired consideration and decode-cost factors; these remain explicit in
the report rather than being folded into an unexplained recommendation.

File format capabilities are version-aware. Files older than the current stable
2.2 format can receive a rewrite suggestion for constant layouts, larger
miniblocks, and variable packed structs. The unstable 2.3 sparse layout is
also considered as a separate experimental recommendation, without a savings
estimate until null/empty-list density can be measured.

Every page's raw encoding description is preserved. Known structural, logical,
physical, and general-compression layers are also normalized into composable
tags. New Lance encodings therefore remain visible instead of being silently
misclassified.

Each rewrite changes metadata only on its target top-level field and preserves
schema and metadata for every other field. Reports aggregate all physical pages
belonging to that logical field. Nested child paths are intentionally not
offered as independent targets; their parent top-level field is measured as a
whole, avoiding false per-child estimates.

## Usage

```console
cargo run -- path/to/dataset.lance
cargo run -- path/to/dataset.lance --branch main --version 3
cargo run -- path/to/dataset.lance --sample-rows 32768
cargo run -- path/to/dataset.lance --consider-decoding-penalty
cargo run -- path/to/dataset.lance --verbose
cargo run -- path/to/dataset.lance --output json
```

The default snapshot is the latest version on the main branch. The probe reads
the selected manifest and only inspects active fragment and overlay data files.
By default it samples up to 16,384 visible rows uniformly without replacement
from each fragment—not per file, page, or row group—using Lance's native
`Dataset::sample` API. Each candidate rewrite uses temporary local data and
does not mutate the source dataset.

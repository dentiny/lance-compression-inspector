# lance-compression-inspector

Inspect a Lance dataset snapshot and identify compression opportunities.

The workspace separates the reusable analysis library from presentation:

- `lance-compression-estimation`: footer probing, report models, rules, and
  measured sample comparisons. Probe and estimation remain separate Rust modules.
- `lance-compression-cli`: human and versioned JSON reports.

Lance leaves general ZSTD/LZ4 compression disabled by default. This tool treats
that as a valid baseline, not an error. It re-encodes a bounded row sample with
LZ4 and several `(ZSTD, level)` candidates in parallel, then compares measured
per-column bytes.

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

## Usage

```console
cargo run -- path/to/dataset.lance
cargo run -- path/to/dataset.lance --branch main --version 3
cargo run -- path/to/dataset.lance --sample-rows 32768
cargo run -- path/to/dataset.lance --consider-decoding-penalty
cargo run -- path/to/dataset.lance --output json
```

The default snapshot is the latest version on the main branch. The probe reads
the selected manifest and only inspects active fragment and overlay data files.
By default it samples up to 16,384 visible rows uniformly without replacement
from each fragment—not per file, page, or row group—using Lance's native
`Dataset::sample` API. Each candidate rewrite uses temporary local data and
does not mutate the source dataset.

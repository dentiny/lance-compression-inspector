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

The inspector processes files, columns, candidate rewrites, and scoring
sequentially. Sampling uses Lance’s `Dataset::sample`, and the CLI uses a
single-threaded Tokio runtime. These limits apply to the inspector; Lance
may still schedule its own internal I/O and encoding tasks.

Ranking uses measured sample bytes by default. `--consider-decoding-penalty` enables
a codec/level decode-cost heuristic, exposed explicitly in the report.
It changes ranking only; projected sizes remain based on measured bytes.

File format capabilities are version-aware. Files older than the current stable
2.2 format can receive a rewrite suggestion for constant layouts, larger
miniblocks, and variable packed structs. The unstable 2.3 sparse layout is
also considered as a separate experimental recommendation, without a savings
estimate until null/empty-list density can be measured.

Page-encoding descriptions come directly from Lance and are preserved without
a custom tag taxonomy. JSON and `--verbose` include both source and sampled
rewrite descriptions; distinct page encodings remain distinct. The summary
table shows requested plans, which may differ from the writer's actual choices.
Blob columns are identified using Lance's schema metadata and excluded from
compression recommendations.

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
the selected manifest and only inspects active base and overlay data files.
By default Lance’s `Dataset::sample` selects up to 16,384 live rows from the
selected snapshot. The same dataset sample is reused for per-file candidate
evaluation, so estimates reflect the sampled dataset distribution rather than
each file’s individual row distribution. Deleted rows are excluded by Lance.
Candidate rewrites use temporary local data and do not mutate the source dataset.

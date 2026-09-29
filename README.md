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
also considered as a separate experimental recommendation. When baseline
sample measurements are available, format recommendations include projected
file size and the estimated gain or cost.

Page-encoding descriptions come directly from Lance and are preserved without
a custom tag taxonomy. JSON and `--verbose` include both source and sampled
rewrite descriptions; distinct page encodings remain distinct. The summary
table shows the current and suggested encodings observed in those descriptions.
Automatic choices retain their requested mode as `auto(actual)`, for example
`auto(miniblock) / auto(variable+flat) / zstd:6`. Codec levels come from the
written metadata; a missing ZSTD level is labeled `level not recorded`.
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

## Remote storage

Remote datasets use OpenDAL's `Operator` through
`object_store_opendal::OpendalStore`. The same accessor handles manifest reads,
dataset sampling, and data-file metadata. Use `s3://bucket/path` for S3-compatible storage.
Local paths and `file://` URIs are also supported.

All backend configuration comes from environment variables. The CLI only
needs the dataset URI:

```console
AWS_REGION=us-east-1 cargo run -- s3://my-bucket/dataset.lance
AWS_ENDPOINT_URL=http://localhost:9000 AWS_REGION=us-east-1 cargo run -- s3://my-bucket/dataset.lance
```

OpenDAL uses its native AWS credential discovery. Backend options can also be
supplied as `OPENDAL_S3_<OPTION>`, for example, `OPENDAL_S3_REGION` or
`OPENDAL_S3_ENDPOINT`. These explicit options take precedence over native
discovery. The dataset URI always determines the bucket and path, regardless
of environment options for `root` or `bucket`.

### Required environment variables for S3

For private buckets using access-key authentication, configure these variables
in the shell that launches the CLI:

| Variable | Requirement | Purpose |
|---|---|---|
| `AWS_ENDPOINT_URL` | Required for custom S3-compatible services; optional for AWS S3 | S3 API endpoint, including `https://` (or `http://` for a local test server) |
| `AWS_REGION` | Set to the bucket's region | Signing region; for example, `us-east-1` |
| `AWS_ACCESS_KEY_ID` | Required for access-key authentication | S3 access key ID |
| `AWS_SECRET_ACCESS_KEY` | Required for access-key authentication | Secret paired with the access key |
| `AWS_SESSION_TOKEN` | Only for temporary credentials that require a session token | Session token; omit for long-lived access keys |

Export the variables before launching the CLI; it does not load a `.env` file.

Example for an S3-compatible service (replace the placeholders):

```bash
export AWS_ENDPOINT_URL="https://<s3-endpoint>"
export AWS_REGION="<region>"
export AWS_ACCESS_KEY_ID="<access-key-id>"
export AWS_SECRET_ACCESS_KEY="<secret-access-key>"
cargo run -- s3://<bucket>/<dataset-path> --sample-rows 32
```

Remote source datasets are read only. Candidate rewrites still use local
temporary storage. External data-file base paths remain unsupported.
Snapshot selection (`--branch` and `--version`) works the same as for local
datasets.

### S3-compatible storage smoke test

Validated on 2026-09-29 against an existing private WARC dataset through
an S3-compatible endpoint, with configuration supplied through environment variables:

| Check | Result |
|---|---|
| Snapshot | Main branch, version 2 |
| Dataset | 63,523 rows, 7 Lance data files, 20 columns |
| Sample | 32 live rows |
| Candidate measurements | 2,751 across the 7 files |
| Blob handling | Descriptor column inspected; excluded from candidate rewrites |
| Runtime | 31.7 seconds, local debug build; exit code 0 |

This exercises remote manifest discovery, sampling, footer reads, and local
candidate rewrites. The 32-row sample is a connectivity and execution smoke test,
not a representative compression benchmark.

## Example: committed test dataset

Run the inspector on the fixture without regenerating it:

```console
cargo run --locked -- testdata/suboptimal.lance
```

The following results were captured on 2026-09-29 with the default 16,384-row
dataset sample and size-only ranking (the decode-cost penalty was disabled).
The fixture has 100,000 rows, five columns, and one 56.58 MiB data file in format
2.1. The CLI completed in about 16.8 seconds in a local debug build, excluding
compilation.

Changing only the file format produced these estimates:

| Target format | Projected file size | Change from the original file |
| --- | ---: | ---: |
| 2.2 | 56.61 MiB | 25.00 KiB larger |
| 2.3 (experimental) | 56.61 MiB | 25.00 KiB larger |

The encoding recommendations were:

```text
  COLUMN     | CURRENT ENCODING                 | SUGGESTED ENCODING                              | FORMAT    | PROJECTED FILE | SAVINGS
  -----------+----------------------------------+-------------------------------------------------+-----------+----------------+----------------
  row_id     | miniblock / flat / none          | auto(miniblock) / auto(bss+flat) / zstd:6       | 2.1 → 2.2 | 55.95 MiB      | 646.29 KiB (1%)
  event_json | miniblock / variable+flat / none | auto(miniblock) / auto(variable+flat) / zstd:6  | 2.1 → 2.2 | 41.32 MiB      | 15.26 MiB (26%)
  payload    | miniblock / variable+flat / none | auto(miniblock) / auto(variable+flat) / zstd:12 | 2.1 → 2.2 | 46.78 MiB      | 9.80 MiB (17%)
```

Encoding cells show `structural layout / value encoding / compression`.
`auto(...)` identifies what Lance actually selected for an automatic setting;
`baseline(...)`, when present, identifies the observed codec with existing writer
controls preserved. The configured candidate plans remain available in verbose
output, alongside the full native encoding descriptions.

Each row estimates the displayed format change plus a change to **only that
column**. `SAVINGS` compares the projected file with the original file, including
the format migration cost; these separate what-if results are not a combined
rewrite result. Percentages use the original file size, not the column size.
The default table omits rows whose net file savings round down to 0%. JSON and
verbose candidate estimates retain encoding-only savings within the target format.

These numbers extrapolate measured sample rewrites; the full source dataset was
not rewritten. File-level overhead is held constant by the estimator. Sampling is
random, so sizes and winning plans can vary between runs. Use `--verbose` to see
candidate scores and Lance-native encoding descriptions, or `--output json` for
machine-readable results.

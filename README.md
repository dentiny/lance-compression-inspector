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

Supported file formats are 2.0–2.3; legacy v1 files are rejected with an explicit
error. Files older than the current stable 2.2 format can receive a rewrite
suggestion for constant layouts, larger miniblocks, and variable packed structs. The unstable 2.3 sparse layout is
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
compression recommendations. File-size projections keep Blob bytes unchanged
so unmeasured Blob columns do not suppress recommendations for other columns.

Each temporary rewrite contains only its target top-level field. Baselines use
that physical file's footer metadata, including nested encoding controls, rather
than the latest manifest metadata. Measurements are reused across files only
when field ID, source format, and physical field configuration match. All output
files from a sample rewrite contribute to its measured bytes.

Fields are matched by ID so renames and dropped columns do not hide other
recommendations. Dropped column bytes remain in fixed file overhead. A changed
nested shape is left unmeasured because its old baseline cannot be reproduced.
Unsupported optional candidates are skipped with a diagnostic; source baseline
and I/O failures remain errors. Reports aggregate all physical pages
belonging to the logical field. Nested child paths are intentionally not
offered as independent targets; their parent top-level field is measured as a
whole, avoiding false per-child estimates. Lance reads structural-layout and
general-compression controls only from the field that owns each physical
column, so candidates apply those controls to every nested field. Value
encoding controls such as packed structs stay on the top-level field.

The human report starts with a dataset-wide `TOP STORAGE COLUMNS` table listing
the ten top-level columns with the most on-disk bytes. Each column's page and
shared column-buffer bytes are summed across all active data files and ranked
in descending order. `SHARE` divides those bytes by the total size of all data
files, including file metadata and footers, so shares add up to less than 100%.
`FILES` shows how many data files contain the column. Columns are grouped by
name in this table, so a renamed field appears once under each name. Blob
columns are marked with `*` because payloads stored outside column buffers are
not counted. Sizes are physical and include rows that were logically deleted.
The table is read from file metadata and does not depend on sampling.

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

OpenDAL reads endpoint, region, and credentials directly from the standard
AWS environment variables below. The dataset URI determines the bucket and path.

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

## Example: committed test dataset

Run the inspector on the fixture without regenerating it:

```console
cargo run --locked -- testdata/suboptimal.lance
```

The following results were captured on 2026-10-02 with the default 16,384-row
dataset sample and size-only ranking (the decode-cost penalty was disabled).
The fixture has 100,000 rows, seven columns, and one 73.84 MiB data file in
format 2.1. The CLI completed in about 13 seconds in a local debug build,
excluding compilation.

The storage breakdown shows that four columns account for about 95% of the
file (nested column types are abbreviated here):

```text
  TOP STORAGE COLUMNS (7 of 7, share of 73.84 MiB total file bytes)

  RANK | COLUMN     | TYPE                                                 | ON DISK    | SHARE | FILES | CURRENT ENCODING
  -----+------------+------------------------------------------------------+------------+-------+-------+----------------------------------------------------------------------------------------------------
  1    | embedding  | FixedSizeList(64 x Float32)                          | 24.41 MiB  | 33%   | 1/1   | fullzip / fixed-size-list+flat / none
  2    | event_json | Utf8                                                 | 18.32 MiB  | 24%   | 1/1   | miniblock / variable+flat / none
  3    | messages   | List(Struct("role", "content", "reasoning_content")) | 16.11 MiB  | 21%   | 1/1   | miniblock / bitpacking+flat+dictionary+variable / none; miniblock / bitpacking+flat+variable / none
  4    | payload    | Binary                                               | 12.70 MiB  | 17%   | 1/1   | miniblock / variable+flat / none
  5    | context    | Struct("source", "language", "turn_count")           | 1.15 MiB   | 1%    | 1/1   | miniblock / flat / none; miniblock / flat+dictionary+variable / none
  6    | row_id     | UInt64                                               | 783.16 KiB | 1%    | 1/1   | miniblock / flat / none
  7    | status     | Utf8                                                 | 391.63 KiB | <1%   | 1/1   | miniblock / flat+dictionary+variable / none
```

Changing only the file format produced these estimates:

| Target format | Projected file size | Change from the original file |
| --- | ---: | ---: |
| 2.2 | 73.89 MiB | 47.33 KiB larger |
| 2.3 (experimental) | 73.89 MiB | 47.33 KiB larger |

The encoding recommendations were:

```text
  COLUMN     | CURRENT ENCODING                                                                                    | SUGGESTED ENCODING                                                                                                                        | FORMAT    | PROJECTED FILE | SAVINGS
  -----------+-----------------------------------------------------------------------------------------------------+-------------------------------------------------------------------------------------------------------------------------------------------+-----------+----------------+-----------------
  row_id     | miniblock / flat / none                                                                             | auto(miniblock) / auto(bss+flat) / zstd:6                                                                                                 | 2.1 → 2.2 | 73.23 MiB      | 624.01 KiB (<1%)
  status     | miniblock / flat+dictionary+variable / none                                                         | auto(miniblock) / auto(bss+flat+dictionary+variable) / zstd:12+lz4                                                                        | 2.1 → 2.2 | 73.53 MiB      | 313.69 KiB (<1%)
  event_json | miniblock / variable+flat / none                                                                    | auto(miniblock) / auto(variable+flat) / zstd:6                                                                                            | 2.1 → 2.2 | 58.60 MiB      | 15.24 MiB (20%)
  payload    | miniblock / variable+flat / none                                                                    | auto(miniblock) / auto(variable+flat) / zstd:12                                                                                           | 2.1 → 2.2 | 64.06 MiB      | 9.78 MiB (13%)
  messages   | miniblock / bitpacking+flat+dictionary+variable / none; miniblock / bitpacking+flat+variable / none | auto(miniblock) / auto(bitpacking+flat+bss+dictionary+variable) / zstd:12+lz4; auto(miniblock) / auto(bitpacking+flat+variable) / zstd:12 | 2.1 → 2.2 | 62.09 MiB      | 11.75 MiB (15%)
  context    | miniblock / flat / none; miniblock / flat+dictionary+variable / none                                | auto(miniblock) / auto(bss+flat) / zstd:12; auto(miniblock) / auto(bss+flat+dictionary+variable) / zstd:12+lz4                            | 2.1 → 2.2 | 72.82 MiB      | 1.02 MiB (1%)
```

The fixture test `fixture_nested_estimates_match_full_column_rewrites` checks
the nested recommendations against a full rewrite of each column: from a
4,096-row sample, the projected `messages` size was within about 4% of the
rewritten size. The nearly constant `context` column compresses to about 14 KiB
in full but projects to about 90 KiB, because fixed page overhead dominates a
small sample. Its projected savings are still within 10% of the actual savings.

Encoding cells show `structural layout / value encoding / compression`.
`auto(...)` identifies what Lance actually selected for an automatic setting;
`baseline(...)`, when present, identifies the observed codec with existing writer
controls preserved. The configured candidate plans remain available in verbose
output, alongside the full native encoding descriptions.

Each row estimates the displayed format change plus a change to **only that
column**. `SAVINGS` compares the projected file with the original file, including
the format migration cost; these separate what-if results are not a combined
rewrite result. Percentages use the original file size, not the column size.
Positive savings below 1% remain visible and are labeled `<1%`. The default
encoding table omits rows with no net file savings. JSON and verbose candidate
estimates retain encoding-only savings within the target format.

These numbers extrapolate measured sample rewrites; the full source dataset was
not rewritten. File-level overhead is held constant by the estimator. Sampling is
random, so sizes and winning plans can vary between runs. Use `--verbose` to see
candidate scores and Lance-native encoding descriptions, or `--output json` for
machine-readable results.

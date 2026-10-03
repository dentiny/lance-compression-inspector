# Test data

`suboptimal.lance/` is a deterministic Lance 2.1 dataset whose fields explicitly
set `lance-encoding:compression=none`. It includes:

- unique, structurally repetitive JSON strings for ZSTD experiments;
- low-cardinality strings for dictionary/RLE interaction;
- smooth fixed-size float vectors for general-compression experiments (BSS
  candidates are intentionally limited to scalar Float32/Float64 columns);
- high-cardinality binary payloads;
- sequential integers for bit-packing interaction;
- chat transcripts in a `list<struct<role, content, reasoning_content>>`
  column with a variable number of turns and null reasoning on user turns;
- a top-level struct of low-cardinality strings and small integers.

Nested parents carry no encoding metadata; `compression=none` is set on each
leaf because Lance reads encoding controls from the field that owns each
physical column.

Regenerate it in an isolated environment with:

```console
uv run testdata/generate_suboptimal.py
```

The generated dataset is intentionally committed so Rust tests and CLI
experiments do not require Python.

To inspect the latest version on its main branch:

```console
cargo run -- testdata/suboptimal.lance
cargo run -- testdata/suboptimal.lance --consider-decoding-penalty
```

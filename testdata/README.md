# Test data

`suboptimal.lance/` is a deterministic Lance 2.1 dataset whose fields explicitly
set `lance-encoding:compression=none`. It includes:

- unique, structurally repetitive JSON strings for ZSTD experiments;
- low-cardinality strings for dictionary/RLE interaction;
- smooth fixed-size float vectors for BSS plus general-compression experiments;
- high-cardinality binary payloads;
- sequential integers for bit-packing interaction.

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

#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "numpy>=2",
#   "pyarrow>=18",
#   "pylance>=0.30",
# ]
# ///
"""Generate a deterministic Lance dataset with intentionally suboptimal compression."""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path

import lance
import numpy as np
import pyarrow as pa

COMPRESSION_KEY = b"lance-encoding:compression"
ROWS = 100_000
EMBEDDING_DIMENSION = 64
TOPICS = ["vector search", "file formats", "compaction", "schema evolution"]


def field(name: str, data_type: pa.DataType, nullable: bool = False) -> pa.Field:
    # Lance defaults general compression to off. Recording "none" explicitly
    # makes this fixture stable if the writer default changes in the future.
    return pa.field(
        name,
        data_type,
        nullable=nullable,
        metadata={COMPRESSION_KEY: b"none"},
    )


def nested(name: str, data_type: pa.DataType) -> pa.Field:
    # Lance reads encoding controls from the field that owns each physical
    # column, so nested parents carry no metadata; their leaves use `field`.
    return pa.field(name, data_type, nullable=False)


def build_table(rows: int) -> pa.Table:
    row_ids = np.arange(rows, dtype=np.uint64)
    rng = np.random.default_rng(42)

    # Unique but structurally repetitive JSON-like strings: deliberately good
    # candidates for ZSTD and too high-cardinality for a tiny dictionary.
    event_json = pa.array(
        [
            (
                '{"event":"page_view","tenant":"tenant-%04d","session":"%016x",'
                '"path":"/catalog/items/%08d","user_agent":"lance-compression-'
                'fixture/1.0","region":"us-west-2","success":true}'
            )
            % (i % 4096, int(rng.integers(0, 2**63)), i)
            for i in range(rows)
        ],
        type=pa.string(),
    )

    # A low-cardinality column exercises Lance's dictionary / RLE choices.
    status = pa.array(
        np.take(np.array(["new", "active", "paused", "deleted"]), row_ids % 4),
        type=pa.string(),
    )

    # Smooth floats are useful for comparing BSS alone with BSS + LZ4/ZSTD.
    positions = np.arange(rows * EMBEDDING_DIMENSION, dtype=np.float32)
    embedding_values = np.sin(positions / 97.0).astype(np.float32)
    embeddings = pa.FixedSizeListArray.from_arrays(
        pa.array(embedding_values),
        EMBEDDING_DIMENSION,
    )

    # Deterministic high-cardinality binary payload with repeated headers.
    payload = pa.array(
        [
            b"LCI1"
            + int(i).to_bytes(8, "little")
            + bytes(((i * 17 + j * 31) & 0xFF) for j in range(116))
            for i in range(rows)
        ],
        type=pa.binary(),
    )

    # Chat transcripts: unique, structurally repetitive text in a list of
    # structs with a variable number of turns and null reasoning on user turns.
    message_type = pa.struct(
        [
            field("role", pa.string()),
            field("content", pa.string()),
            field("reasoning_content", pa.string(), nullable=True),
        ]
    )
    messages = []
    for i in range(rows):
        topic = TOPICS[i % len(TOPICS)]
        turns = []
        for turn in range(1 + i % 3):
            token = int(rng.integers(0, 2**32))
            if turn % 2 == 0:
                turns.append(
                    {
                        "role": "user",
                        "content": f"Q{i}.{turn} {token:08x}: how does Lance do {topic}?",
                        "reasoning_content": None,
                    }
                )
            else:
                turns.append(
                    {
                        "role": "assistant",
                        "content": f"A{i}.{turn}: Lance does {topic} in pages, see {token % 97}.",
                        "reasoning_content": f"User asks about {topic}; cite note {token:08x}.",
                    }
                )
        messages.append(turns)

    # A top-level struct whose leaves are low-cardinality strings and small ints.
    context_type = pa.struct(
        [
            field("source", pa.string()),
            field("language", pa.string()),
            field("turn_count", pa.int32()),
        ]
    )
    context = [
        {
            "source": ["web", "api", "batch"][i % 3],
            "language": ["en", "zh", "ja", "de"][i % 4],
            "turn_count": 1 + i % 3,
        }
        for i in range(rows)
    ]

    schema = pa.schema(
        [
            field("row_id", pa.uint64()),
            field("status", pa.string()),
            field("event_json", pa.string()),
            field(
                "embedding",
                pa.list_(pa.float32(), EMBEDDING_DIMENSION),
            ),
            field("payload", pa.binary()),
            nested("messages", pa.list_(nested("item", message_type))),
            nested("context", context_type),
        ],
        metadata={b"fixture": b"suboptimal-general-compression"},
    )
    return pa.Table.from_arrays(
        [
            pa.array(row_ids),
            status,
            event_json,
            embeddings,
            payload,
            pa.array(messages, type=schema.field("messages").type),
            pa.array(context, type=context_type),
        ],
        schema=schema,
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output",
        type=Path,
        default=Path(__file__).with_name("suboptimal.lance"),
    )
    parser.add_argument("--rows", type=int, default=ROWS)
    args = parser.parse_args()

    if args.output.exists():
        shutil.rmtree(args.output)
    table = build_table(args.rows)
    lance.write_dataset(
        table,
        args.output,
        mode="create",
        data_storage_version="2.1",
    )
    print(f"wrote {args.rows:,} rows to {args.output}")


if __name__ == "__main__":
    main()

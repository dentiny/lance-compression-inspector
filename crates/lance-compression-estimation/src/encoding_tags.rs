use std::collections::BTreeSet;

use lance_encoding::{
    decoder::{ColumnInfo, PageEncoding},
    format::{pb, pb21},
};

use crate::model::EncodingTag;

pub(crate) fn classify_column(column: &ColumnInfo) -> BTreeSet<EncodingTag> {
    let mut tags = BTreeSet::new();
    classify_column_encoding(&column.encoding, &mut tags);
    for page in column.page_infos.iter() {
        match &page.encoding {
            PageEncoding::Legacy(encoding) => classify_legacy(encoding, &mut tags),
            PageEncoding::Structural(layout) => classify_layout(layout, &mut tags),
        }
    }
    if !tags.contains(&EncodingTag::GeneralLz4) && !tags.contains(&EncodingTag::GeneralZstd) {
        tags.insert(EncodingTag::GeneralUncompressed);
    }
    tags
}

fn classify_column_encoding(encoding: &pb::ColumnEncoding, tags: &mut BTreeSet<EncodingTag>) {
    use pb::column_encoding::ColumnEncoding;
    match encoding.column_encoding.as_ref() {
        Some(ColumnEncoding::Values(_)) | None => {}
        Some(ColumnEncoding::ZoneIndex(zone)) => {
            if let Some(inner) = zone.inner.as_deref() {
                classify_column_encoding(inner, tags);
            }
        }
        Some(ColumnEncoding::Blob(blob)) => {
            tags.insert(EncodingTag::StructuralBlob);
            if let Some(inner) = blob.inner.as_deref() {
                classify_column_encoding(inner, tags);
            }
        }
    }
}

fn classify_layout(layout: &pb21::PageLayout, tags: &mut BTreeSet<EncodingTag>) {
    use pb21::page_layout::Layout;
    match layout.layout.as_ref() {
        Some(Layout::MiniBlockLayout(layout)) => {
            tags.insert(EncodingTag::StructuralMiniBlock);
            for encoding in [
                layout.rep_compression.as_ref(),
                layout.def_compression.as_ref(),
                layout.value_compression.as_ref(),
                layout.dictionary.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                classify_compressive(encoding, tags);
            }
            if layout.dictionary.is_some() {
                tags.insert(EncodingTag::Dictionary);
            }
        }
        Some(Layout::FullZipLayout(layout)) => {
            tags.insert(EncodingTag::StructuralFullZip);
            if let Some(encoding) = layout.value_compression.as_ref() {
                classify_compressive(encoding, tags);
            }
        }
        Some(Layout::SparseLayout(layout)) => {
            tags.insert(EncodingTag::StructuralSparse);
            if let Some(encoding) = layout.value_compression.as_ref() {
                classify_compressive(encoding, tags);
            }
        }
        Some(Layout::ConstantLayout(layout)) => {
            tags.insert(EncodingTag::Constant);
            for encoding in [
                layout.rep_compression.as_ref(),
                layout.def_compression.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                classify_compressive(encoding, tags);
            }
        }
        Some(Layout::BlobLayout(layout)) => {
            tags.insert(EncodingTag::StructuralBlob);
            if let Some(inner) = layout.inner_layout.as_deref() {
                classify_layout(inner, tags);
            }
        }
        None => {
            tags.insert(EncodingTag::Unknown);
        }
    }
}

fn classify_compressive(encoding: &pb21::CompressiveEncoding, tags: &mut BTreeSet<EncodingTag>) {
    use pb21::compressive_encoding::Compression;
    match encoding.compression.as_ref() {
        Some(Compression::Flat(flat)) => {
            tags.insert(EncodingTag::Flat);
            classify_buffer(flat.data.as_ref(), tags);
        }
        Some(Compression::Variable(variable)) => {
            tags.insert(EncodingTag::VariableWidth);
            if let Some(offsets) = variable.offsets.as_deref() {
                classify_compressive(offsets, tags);
            }
            classify_buffer(variable.values.as_ref(), tags);
        }
        Some(Compression::Constant(_)) => {
            tags.insert(EncodingTag::Constant);
        }
        Some(Compression::OutOfLineBitpacking(bitpacked)) => {
            tags.insert(EncodingTag::BitPacked);
            if let Some(values) = bitpacked.values.as_deref() {
                classify_compressive(values, tags);
            }
        }
        Some(Compression::InlineBitpacking(bitpacked)) => {
            tags.insert(EncodingTag::BitPacked);
            classify_buffer(bitpacked.values.as_ref(), tags);
        }
        Some(Compression::Fsst(fsst)) => {
            tags.insert(EncodingTag::Fsst);
            if let Some(values) = fsst.values.as_deref() {
                classify_compressive(values, tags);
            }
        }
        Some(Compression::Dictionary(dictionary)) => {
            tags.insert(EncodingTag::Dictionary);
            for child in [dictionary.indices.as_deref(), dictionary.items.as_deref()]
                .into_iter()
                .flatten()
            {
                classify_compressive(child, tags);
            }
        }
        Some(Compression::Rle(rle)) => {
            tags.insert(EncodingTag::Rle);
            for child in [rle.values.as_deref(), rle.run_lengths.as_deref()]
                .into_iter()
                .flatten()
            {
                classify_compressive(child, tags);
            }
        }
        Some(Compression::ByteStreamSplit(bss)) => {
            tags.insert(EncodingTag::ByteStreamSplit);
            if let Some(values) = bss.values.as_deref() {
                classify_compressive(values, tags);
            }
        }
        Some(Compression::General(general)) => {
            classify_buffer(general.compression.as_ref(), tags);
            if let Some(values) = general.values.as_deref() {
                classify_compressive(values, tags);
            }
        }
        Some(Compression::FixedSizeList(list)) => {
            tags.insert(EncodingTag::FixedSizeList);
            if let Some(values) = list.values.as_deref() {
                classify_compressive(values, tags);
            }
        }
        Some(Compression::PackedStruct(packed)) => {
            tags.insert(EncodingTag::PackedStruct);
            if let Some(values) = packed.values.as_deref() {
                classify_compressive(values, tags);
            }
        }
        Some(Compression::VariablePackedStruct(packed)) => {
            tags.insert(EncodingTag::PackedStruct);
            for field in &packed.fields {
                if let Some(value) = field.value.as_ref() {
                    classify_compressive(value, tags);
                }
            }
        }
        None => {
            tags.insert(EncodingTag::Unknown);
        }
    }
}

fn classify_buffer(
    compression: Option<&pb21::BufferCompression>,
    tags: &mut BTreeSet<EncodingTag>,
) {
    let Some(compression) = compression else {
        return;
    };
    match pb21::CompressionScheme::try_from(compression.scheme).ok() {
        Some(pb21::CompressionScheme::CompressionAlgorithmLz4) => {
            tags.insert(EncodingTag::GeneralLz4);
        }
        Some(pb21::CompressionScheme::CompressionAlgorithmZstd) => {
            tags.insert(EncodingTag::GeneralZstd);
        }
        Some(pb21::CompressionScheme::CompressionAlgorithmUnspecified) | None => {}
    }
}

fn classify_legacy(encoding: &pb::ArrayEncoding, tags: &mut BTreeSet<EncodingTag>) {
    use pb::array_encoding::ArrayEncoding;
    match encoding.array_encoding.as_ref() {
        Some(ArrayEncoding::Flat(flat)) => {
            tags.insert(EncodingTag::Flat);
            classify_legacy_compression(flat.compression.as_ref(), tags);
        }
        Some(ArrayEncoding::Nullable(nullable)) => {
            tags.insert(EncodingTag::Nullable);
            use pb::nullable::Nullability;
            match nullable.nullability.as_ref() {
                Some(Nullability::NoNulls(no_nulls)) => {
                    if let Some(values) = no_nulls.values.as_deref() {
                        classify_legacy(values, tags);
                    }
                }
                Some(Nullability::SomeNulls(some_nulls)) => {
                    for child in [some_nulls.validity.as_deref(), some_nulls.values.as_deref()]
                        .into_iter()
                        .flatten()
                    {
                        classify_legacy(child, tags);
                    }
                }
                Some(Nullability::AllNulls(_)) => {
                    tags.insert(EncodingTag::Constant);
                }
                None => {
                    tags.insert(EncodingTag::Unknown);
                }
            }
        }
        Some(ArrayEncoding::FixedSizeList(list)) => {
            tags.insert(EncodingTag::FixedSizeList);
            if let Some(items) = list.items.as_deref() {
                classify_legacy(items, tags);
            }
        }
        Some(ArrayEncoding::List(list)) => {
            tags.insert(EncodingTag::List);
            if let Some(offsets) = list.offsets.as_deref() {
                classify_legacy(offsets, tags);
            }
        }
        Some(ArrayEncoding::Struct(_)) => {
            tags.insert(EncodingTag::Struct);
        }
        Some(ArrayEncoding::Binary(binary)) => {
            tags.insert(EncodingTag::Binary);
            for child in [binary.indices.as_deref(), binary.bytes.as_deref()]
                .into_iter()
                .flatten()
            {
                classify_legacy(child, tags);
            }
        }
        Some(ArrayEncoding::Dictionary(dictionary)) => {
            tags.insert(EncodingTag::Dictionary);
            for child in [dictionary.indices.as_deref(), dictionary.items.as_deref()]
                .into_iter()
                .flatten()
            {
                classify_legacy(child, tags);
            }
        }
        Some(ArrayEncoding::Fsst(fsst)) => {
            tags.insert(EncodingTag::Fsst);
            if let Some(binary) = fsst.binary.as_deref() {
                classify_legacy(binary, tags);
            }
        }
        Some(ArrayEncoding::PackedStruct(packed)) => {
            tags.insert(EncodingTag::PackedStruct);
            for child in &packed.inner {
                classify_legacy(child, tags);
            }
        }
        Some(
            ArrayEncoding::Bitpacked(_)
            | ArrayEncoding::BitpackedForNonNeg(_)
            | ArrayEncoding::InlineBitpacking(_)
            | ArrayEncoding::OutOfLineBitpacking(_),
        ) => {
            tags.insert(EncodingTag::BitPacked);
        }
        Some(ArrayEncoding::FixedSizeBinary(binary)) => {
            tags.insert(EncodingTag::FixedSizeBinary);
            if let Some(bytes) = binary.bytes.as_deref() {
                classify_legacy(bytes, tags);
            }
        }
        Some(ArrayEncoding::Constant(_)) => {
            tags.insert(EncodingTag::Constant);
        }
        Some(ArrayEncoding::Variable(_)) => {
            tags.insert(EncodingTag::VariableWidth);
        }
        Some(ArrayEncoding::PackedStructFixedWidthMiniBlock(packed)) => {
            tags.insert(EncodingTag::PackedStruct);
            if let Some(flat) = packed.flat.as_deref() {
                classify_legacy(flat, tags);
            }
        }
        Some(ArrayEncoding::Block(block)) => {
            tags.insert(EncodingTag::Block);
            classify_legacy_scheme(&block.scheme, tags);
        }
        Some(ArrayEncoding::Rle(_)) => {
            tags.insert(EncodingTag::Rle);
        }
        Some(ArrayEncoding::GeneralMiniBlock(general)) => {
            classify_legacy_compression(general.compression.as_ref(), tags);
            if let Some(inner) = general.inner.as_deref() {
                classify_legacy(inner, tags);
            }
        }
        Some(ArrayEncoding::ByteStreamSplit(_)) => {
            tags.insert(EncodingTag::ByteStreamSplit);
        }
        None => {
            tags.insert(EncodingTag::Unknown);
        }
    }
}

fn classify_legacy_compression(
    compression: Option<&pb::Compression>,
    tags: &mut BTreeSet<EncodingTag>,
) {
    if let Some(compression) = compression {
        classify_legacy_scheme(&compression.scheme, tags);
    }
}

fn classify_legacy_scheme(scheme: &str, tags: &mut BTreeSet<EncodingTag>) {
    match scheme.to_ascii_lowercase().as_str() {
        "lz4" => {
            tags.insert(EncodingTag::GeneralLz4);
        }
        "zstd" => {
            tags.insert(EncodingTag::GeneralZstd);
        }
        "fsst" => {
            tags.insert(EncodingTag::Fsst);
        }
        "none" | "" => {}
        _ => {
            tags.insert(EncodingTag::Unknown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_structural_and_buffer_compression_from_typed_metadata() {
        let value = pb21::CompressiveEncoding {
            compression: Some(pb21::compressive_encoding::Compression::Flat(pb21::Flat {
                bits_per_value: 32,
                data: Some(pb21::BufferCompression {
                    scheme: pb21::CompressionScheme::CompressionAlgorithmZstd as i32,
                    level: Some(6),
                }),
            })),
        };
        let layout = pb21::PageLayout {
            layout: Some(pb21::page_layout::Layout::FullZipLayout(
                pb21::FullZipLayout {
                    value_compression: Some(value),
                    ..Default::default()
                },
            )),
        };
        let mut tags = BTreeSet::new();
        classify_layout(&layout, &mut tags);
        assert!(tags.contains(&EncodingTag::StructuralFullZip));
        assert!(tags.contains(&EncodingTag::Flat));
        assert!(tags.contains(&EncodingTag::GeneralZstd));
        assert!(!tags.contains(&EncodingTag::GeneralUncompressed));
    }

    #[test]
    fn dictionary_presence_is_not_inferred_from_debug_text() {
        let layout = pb21::PageLayout {
            layout: Some(pb21::page_layout::Layout::MiniBlockLayout(
                pb21::MiniBlockLayout {
                    dictionary: Some(pb21::CompressiveEncoding {
                        compression: Some(pb21::compressive_encoding::Compression::Variable(
                            Box::default(),
                        )),
                    }),
                    ..Default::default()
                },
            )),
        };
        let mut tags = BTreeSet::new();
        classify_layout(&layout, &mut tags);
        assert!(tags.contains(&EncodingTag::StructuralMiniBlock));
        assert!(tags.contains(&EncodingTag::Dictionary));
        assert!(tags.contains(&EncodingTag::VariableWidth));
    }
}

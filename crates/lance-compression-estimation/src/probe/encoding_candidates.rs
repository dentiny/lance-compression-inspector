//! Type-aware structural/value encoding candidates and their field metadata.

use super::compression_candidates::{compression_candidate_plans, with_compression_field};
use super::type_utils::{fixed_bit_width, supports_dictionary};
use crate::{
    EncodingFileVersion, EncodingMeasurement, EncodingPlan, GeneralCompression, StructuralEncoding,
    ValueEncoding,
};
use arrow_schema::{DataType, Field as ArrowField};

/// Generate the independent axis sweep for a top-level Arrow field.
///
/// This always includes the exact metadata-preserving baseline and all general
/// codecs. Structural and value candidates are added only where Lance exposes
/// an applicable writer control for the data type and target format.
pub fn candidate_plans_for_type(
    data_type: &DataType,
    file_version: EncodingFileVersion,
) -> Vec<EncodingPlan> {
    let baseline = EncodingPlan::baseline(file_version);
    let mut plans = compression_candidate_plans(file_version);

    if !matches!(data_type, DataType::Struct(_)) {
        for structural in [StructuralEncoding::MiniBlock, StructuralEncoding::FullZip] {
            plans.push(EncodingPlan {
                structural,
                ..baseline
            });
        }
        if fixed_bit_width(data_type).is_some()
            || matches!(
                data_type,
                DataType::Boolean
                    | DataType::Utf8
                    | DataType::LargeUtf8
                    | DataType::Binary
                    | DataType::LargeBinary
            )
        {
            plans.push(EncodingPlan {
                structural: StructuralEncoding::Sparse,
                file_version: EncodingFileVersion::V2_3,
                ..baseline
            });
        }
    }

    let mut add_value = |value, general| {
        plans.push(EncodingPlan {
            value,
            general,
            ..baseline
        });
    };
    if fixed_bit_width(data_type).is_some_and(|width| matches!(width, 8 | 16 | 32 | 64)) {
        add_value(ValueEncoding::Rle, GeneralCompression::Baseline);
    }
    if matches!(
        data_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Binary | DataType::LargeBinary
    ) {
        add_value(ValueEncoding::Fsst, GeneralCompression::Baseline);
    }
    if matches!(data_type, DataType::Float32 | DataType::Float64) {
        // BSS has no useful standalone representation; Lance layers a general
        // compressor over the split byte streams.
        add_value(
            ValueEncoding::ByteStreamSplit,
            GeneralCompression::Zstd { level: 3 },
        );
    }
    if supports_dictionary(data_type) {
        add_value(ValueEncoding::Dictionary, GeneralCompression::Baseline);
    }
    if super::type_utils::supports_packed_struct(data_type)
        && matches!(
            file_version,
            EncodingFileVersion::V2_2 | EncodingFileVersion::V2_3
        )
    {
        add_value(ValueEncoding::PackedStruct, GeneralCompression::Baseline);
    }
    plans.sort();
    plans.dedup();
    plans
}

pub(super) fn combined_candidate_plans(measurements: &[EncodingMeasurement]) -> Vec<EncodingPlan> {
    let best_structural = measurements
        .iter()
        .filter(|measurement| measurement.plan.structural != StructuralEncoding::Auto)
        .min_by_key(|measurement| measurement.encoded_bytes)
        .map(|measurement| measurement.plan);
    let best_value = measurements
        .iter()
        .filter(|measurement| measurement.plan.value != ValueEncoding::Auto)
        .min_by_key(|measurement| measurement.encoded_bytes)
        .map(|measurement| measurement.plan);

    let mut beam = best_structural.into_iter().collect::<Vec<_>>();
    beam.extend(best_value);
    if let (Some(structural), Some(value)) = (best_structural, best_value) {
        beam.push(EncodingPlan {
            structural: structural.structural,
            value: value.value,
            general: value.general,
            file_version: structural.file_version.max(value.file_version),
        });
    }

    let mut plans = Vec::new();
    for plan in beam {
        plans.push(plan);
        if plan.value == ValueEncoding::Fsst {
            continue;
        }
        let generals: &[GeneralCompression] = if plan.value == ValueEncoding::ByteStreamSplit {
            &[
                GeneralCompression::Lz4,
                GeneralCompression::Zstd { level: 3 },
                GeneralCompression::Zstd { level: 9 },
            ]
        } else {
            &[
                GeneralCompression::None,
                GeneralCompression::Lz4,
                GeneralCompression::Zstd { level: 3 },
                GeneralCompression::Zstd { level: 9 },
            ]
        };
        plans.extend(
            generals
                .iter()
                .copied()
                .map(|general| EncodingPlan { general, ..plan }),
        );
    }
    plans.sort();
    plans.dedup();
    plans
}

pub(super) fn with_candidate_field(field: &ArrowField, plan: EncodingPlan) -> ArrowField {
    let mut metadata = field.metadata().clone();
    match plan.structural {
        StructuralEncoding::Auto => {}
        StructuralEncoding::MiniBlock => {
            metadata.insert(
                "lance-encoding:structural-encoding".into(),
                "miniblock".into(),
            );
        }
        StructuralEncoding::FullZip => {
            metadata.insert(
                "lance-encoding:structural-encoding".into(),
                "fullzip".into(),
            );
        }
        StructuralEncoding::Sparse => {
            metadata.insert("lance-encoding:structural-encoding".into(), "sparse".into());
        }
    }
    if plan.value != ValueEncoding::Auto {
        for key in [
            "lance-encoding:rle-threshold",
            "lance-encoding:bss",
            "lance-encoding:dict-divisor",
            "lance-encoding:dict-size-ratio",
            "lance-encoding:packed",
        ] {
            metadata.remove(key);
        }
    }
    match plan.value {
        ValueEncoding::Auto => {}
        ValueEncoding::Rle => {
            metadata.insert("lance-encoding:rle-threshold".into(), "1.0".into());
            metadata.insert("lance-encoding:bss".into(), "off".into());
        }
        ValueEncoding::Fsst => {
            metadata.insert("lance-encoding:compression".into(), "fsst".into());
            metadata.remove("lance-encoding:compression-level");
        }
        ValueEncoding::ByteStreamSplit => {
            metadata.insert("lance-encoding:bss".into(), "on".into());
            metadata.insert("lance-encoding:rle-threshold".into(), "0".into());
        }
        ValueEncoding::Dictionary => {
            metadata.insert("lance-encoding:dict-divisor".into(), "1".into());
            metadata.insert("lance-encoding:dict-size-ratio".into(), "1.0".into());
        }
        ValueEncoding::PackedStruct => {
            metadata.insert("lance-encoding:packed".into(), "true".into());
        }
    }
    with_compression_field(&field.clone().with_metadata(metadata), plan.general)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn plans_are_type_and_version_aware() {
        let floats = candidate_plans_for_type(&DataType::Float32, EncodingFileVersion::V2_2);
        assert!(floats.iter().any(|plan| {
            plan.value == ValueEncoding::ByteStreamSplit
                && plan.general == GeneralCompression::Zstd { level: 3 }
        }));
        assert!(floats.iter().any(|plan| {
            plan.structural == StructuralEncoding::Sparse
                && plan.file_version == EncodingFileVersion::V2_3
        }));

        let strings = candidate_plans_for_type(&DataType::Utf8, EncodingFileVersion::V2_2);
        assert!(strings.iter().any(|plan| plan.value == ValueEncoding::Fsst));
        assert!(
            strings
                .iter()
                .any(|plan| plan.value == ValueEncoding::Dictionary)
        );
        assert!(
            !strings
                .iter()
                .any(|plan| plan.value == ValueEncoding::ByteStreamSplit)
        );

        let int64 = candidate_plans_for_type(&DataType::Int64, EncodingFileVersion::V2_2);
        assert!(int64.iter().any(|plan| plan.value == ValueEncoding::Rle));
        assert!(
            int64
                .iter()
                .any(|plan| plan.value == ValueEncoding::Dictionary)
        );

        let int32 = candidate_plans_for_type(&DataType::Int32, EncodingFileVersion::V2_2);
        assert!(int32.iter().any(|plan| plan.value == ValueEncoding::Rle));
        assert!(
            !int32
                .iter()
                .any(|plan| plan.value == ValueEncoding::Dictionary)
        );

        let struct_type =
            DataType::Struct(vec![Arc::new(ArrowField::new("child", DataType::Utf8, true))].into());
        let structs = candidate_plans_for_type(&struct_type, EncodingFileVersion::V2_2);
        assert!(
            structs
                .iter()
                .any(|plan| plan.value == ValueEncoding::PackedStruct)
        );
        assert!(
            !structs
                .iter()
                .any(|plan| plan.structural == StructuralEncoding::Sparse)
        );
    }
}

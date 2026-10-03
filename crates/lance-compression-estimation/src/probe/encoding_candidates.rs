//! Type-aware structural/value encoding candidates and their field metadata.

use super::compression_candidates::{apply_compression, compression_candidate_plans};
use super::type_utils::{fixed_bit_width, supports_dictionary};
use crate::{
    EncodingFileVersion, EncodingMeasurement, EncodingPlan, GeneralCompression, StructuralEncoding,
    ValueEncoding,
};
use arrow_schema::{DataType, Field as ArrowField, FieldRef};
use std::sync::Arc;

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
        for general in [
            GeneralCompression::None,
            GeneralCompression::Lz4,
            GeneralCompression::Zstd { level: 3 },
            GeneralCompression::Zstd { level: 9 },
        ] {
            if plan.value != ValueEncoding::ByteStreamSplit || general != GeneralCompression::None {
                plans.push(EncodingPlan { general, ..plan });
            }
        }
    }
    plans.sort();
    plans.dedup();
    plans
}

pub(super) fn with_candidate_field(field: &ArrowField, plan: EncodingPlan) -> ArrowField {
    let mut metadata = field.metadata().clone();
    if plan.value != ValueEncoding::Auto {
        for key in [
            "rle-threshold",
            "bss",
            "dict-divisor",
            "dict-size-ratio",
            "packed",
        ] {
            metadata.remove(&format!("lance-encoding:{key}"));
        }
    }
    let controls: &[(&str, &str)] = match plan.value {
        ValueEncoding::Auto => &[],
        ValueEncoding::Rle => &[("rle-threshold", "1.0"), ("bss", "off")],
        ValueEncoding::Fsst => {
            metadata.remove("lance-encoding:compression-level");
            &[("compression", "fsst")]
        }
        ValueEncoding::ByteStreamSplit => &[("bss", "on"), ("rle-threshold", "0")],
        ValueEncoding::Dictionary => &[("dict-divisor", "1"), ("dict-size-ratio", "1.0")],
        ValueEncoding::PackedStruct => &[("packed", "true")],
    };
    for (key, value) in controls {
        metadata.insert(format!("lance-encoding:{key}"), (*value).into());
    }
    with_physical_controls(&field.clone().with_metadata(metadata), plan)
}

/// Lance reads structural and general-compression controls from the field that
/// owns each physical column and never inherits them from a nested parent, so
/// they are applied to every field in the tree.
fn with_physical_controls(field: &ArrowField, plan: EncodingPlan) -> ArrowField {
    let child = |child: &FieldRef| Arc::new(with_physical_controls(child, plan));
    let data_type = match field.data_type() {
        DataType::List(item) => DataType::List(child(item)),
        DataType::LargeList(item) => DataType::LargeList(child(item)),
        DataType::FixedSizeList(item, size) => DataType::FixedSizeList(child(item), *size),
        DataType::Map(entries, sorted) => DataType::Map(child(entries), *sorted),
        DataType::Struct(fields) => DataType::Struct(fields.iter().map(child).collect()),
        data_type => data_type.clone(),
    };
    let mut metadata = field.metadata().clone();
    if plan.structural != StructuralEncoding::Auto {
        metadata.insert(
            "lance-encoding:structural-encoding".into(),
            plan.structural.to_string(),
        );
    }
    apply_compression(&mut metadata, plan.general);
    field
        .clone()
        .with_data_type(data_type)
        .with_metadata(metadata)
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
        assert!(
            structs
                .iter()
                .any(|plan| plan.structural == StructuralEncoding::FullZip)
        );
    }

    #[test]
    fn physical_controls_reach_nested_leaves_but_value_controls_stay_on_parent() {
        let leaf = ArrowField::new("content", DataType::Utf8, false)
            .with_metadata([("lance-encoding:compression".into(), "none".into())].into());
        let item = ArrowField::new("item", DataType::Struct(vec![leaf].into()), false);
        let field = ArrowField::new("messages", DataType::List(Arc::new(item)), false);
        let plan = EncodingPlan {
            structural: StructuralEncoding::MiniBlock,
            value: ValueEncoding::PackedStruct,
            general: GeneralCompression::Zstd { level: 3 },
            file_version: EncodingFileVersion::V2_2,
        };
        let candidate = with_candidate_field(&field, plan);
        let DataType::List(item) = candidate.data_type() else {
            panic!("list shape changed");
        };
        let DataType::Struct(leaves) = item.data_type() else {
            panic!("struct shape changed");
        };
        let metadata = leaves[0].metadata();
        assert_eq!(metadata["lance-encoding:compression"], "zstd");
        assert_eq!(metadata["lance-encoding:compression-level"], "3");
        assert_eq!(metadata["lance-encoding:structural-encoding"], "miniblock");
        assert!(!item.metadata().contains_key("lance-encoding:packed"));
        assert_eq!(candidate.metadata()["lance-encoding:packed"], "true");

        let baseline = with_candidate_field(&field, EncodingPlan::baseline(plan.file_version));
        assert_eq!(baseline, field);
    }
}

//! Type checks for encoding candidate selection.

use arrow_schema::DataType;

/// Fixed width in bits used to select encoding candidates.
/// Returns None for types not handled by this policy.
pub(super) fn fixed_bit_width(data_type: &DataType) -> Option<usize> {
    match data_type {
        DataType::Int8 | DataType::UInt8 => Some(8),
        DataType::Int16 | DataType::UInt16 | DataType::Float16 => Some(16),
        DataType::Int32
        | DataType::UInt32
        | DataType::Float32
        | DataType::Date32
        | DataType::Time32(_) => Some(32),
        DataType::Int64
        | DataType::UInt64
        | DataType::Float64
        | DataType::Date64
        | DataType::Time64(_)
        | DataType::Timestamp(_, _)
        | DataType::Duration(_) => Some(64),
        DataType::Decimal128(_, _) => Some(128),
        DataType::FixedSizeBinary(bytes) if *bytes > 0 => usize::try_from(*bytes)
            .ok()
            .and_then(|bytes| bytes.checked_mul(8)),
        _ => None,
    }
}

/// Whether to try dictionary encoding: strings, binary, or 64/128-bit fixed-width types.
pub(super) fn supports_dictionary(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Binary | DataType::LargeBinary
    ) || fixed_bit_width(data_type).is_some_and(|width| matches!(width, 64 | 128))
}

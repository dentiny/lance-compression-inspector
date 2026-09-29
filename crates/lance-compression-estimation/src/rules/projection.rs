//! Shared baseline lookup and size projection for both recommendation rules.

use crate::{ColumnProfile, EncodingFileVersion, EncodingMeasurement, EncodingPlan, ProbeReport};

pub(super) fn baseline(
    column: &ColumnProfile,
    version: EncodingFileVersion,
) -> Option<&EncodingMeasurement> {
    column
        .encoding_measurements
        .iter()
        .find(|m| m.plan == EncodingPlan::baseline(version))
}

/// Scale column bytes by the sampled ratio; keep zero-byte baselines unchanged.
pub(super) fn project_bytes(full: u64, target: u64, current: u64) -> u64 {
    if current == 0 {
        return full;
    }
    (u128::from(full) * u128::from(target) / u128::from(current)).min(u128::from(u64::MAX)) as u64
}

/// Project all columns to the target format, holding Blob bytes and file overhead
/// fixed. Missing non-Blob baselines make a format migration estimate unavailable.
pub(super) fn projected_file_size(probe: &ProbeReport, target: EncodingFileVersion) -> Option<u64> {
    if target == probe.file_version {
        return Some(probe.file_size_bytes);
    }
    let overhead = probe
        .file_size_bytes
        .saturating_sub(probe.columns.iter().map(|c| c.on_disk_bytes).sum());
    probe.columns.iter().try_fold(overhead, |total, column| {
        let bytes = if column.has_blob {
            column.on_disk_bytes
        } else {
            project_bytes(
                column.on_disk_bytes,
                baseline(column, target)?.encoded_bytes,
                baseline(column, probe.file_version)?.encoded_bytes,
            )
        };
        Some(total.saturating_add(bytes))
    })
}

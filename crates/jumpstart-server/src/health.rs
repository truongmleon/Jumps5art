//! Reports the evidence available from an offline server root.
//!
//! A busy or corrupt database can stop verification even when the filesystem has
//! plenty of space. Keeping those observations separate gives the operator a
//! useful report without equating available bytes with a healthy server.

use std::{error::Error, path::Path};

use jumpstart_storage::{CapacityReport, MetadataReport, StorageError, inspect, inspect_capacity};
use serde_json::{Value, json};

/// Check the metadata root and its filesystem, then print both observations.
///
/// `root` is the path selected by the operator. `json` selects machine-readable
/// output. Returns `Ok(())` if both checks ran, including when space is low. A
/// missing observation yields an error after flushing the report, so scripts can
/// see the evidence and detect the incomplete check. The command is read-only.
pub(super) fn run(root: &Path, json: bool) -> Result<(), Box<dyn Error>> {
    let metadata = inspect(root);
    let capacity = inspect_capacity(root);
    let incomplete = metadata.is_err() || capacity.is_err();

    let report = json!({
        "scope": "offline_metadata",
        "metadata": metadata_component(metadata),
        "capacity": capacity_component(capacity),
    });
    super::print_report(&report, "Offline metadata health:", json)?;
    if incomplete {
        return Err("one or more offline checks could not complete; see the report".into());
    }
    Ok(())
}

/// Convert the checked metadata result into a component with a next action.
///
/// `result` comes from the existing locked inspection. Returns a JSON component
/// describing verification or why it was refused, keeping a busy root distinct
/// from a root that needs recovery. It never attempts the suggested action.
fn metadata_component(result: jumpstart_storage::Result<MetadataReport>) -> Value {
    match result {
        Ok(verified) => json!({"status": "verified", "evidence": verified}),
        Err(error) => {
            let (status, next_action) = match &error {
                StorageError::InUse => (
                    "busy",
                    "Stop the exclusive writer before retrying this offline check.",
                ),
                StorageError::NeedsRecovery(_) => (
                    "needs_recovery",
                    "Follow the recorded recovery workflow before retrying. Retain the journal and SQLite sidecars.",
                ),
                StorageError::UnsupportedFormat { .. } | StorageError::UnsupportedSchema { .. } => {
                    (
                        "unsupported",
                        "Use a Jumpstart binary that supports this metadata format and schema.",
                    )
                }
                StorageError::UnsupportedPlatform => {
                    ("unsupported", "Run the offline check on Linux or macOS.")
                }
                StorageError::Integrity(_)
                | StorageError::IdentityMismatch
                | StorageError::MigrationMismatch
                | StorageError::InvalidMarker(_) => (
                    "invalid",
                    "Preserve the metadata and check the verified recovery copy before taking recovery action.",
                ),
                _ => (
                    "unavailable",
                    "Check the metadata path and access, then retry offline inspection.",
                ),
            };
            json!({"status": status, "detail": error.to_string(), "next_action": next_action})
        }
    }
}

/// Label the measured space using the existing reserve and warning policy.
///
/// `result` comes from filesystem inspection. Returns a JSON component with
/// observed bytes and thresholds, or an unknown result when it cannot measure
/// them. Status describes capacity evidence only.
fn capacity_component(result: jumpstart_storage::Result<CapacityReport>) -> Value {
    match result {
        Ok(observed) => {
            let (status, next_action) = if observed.assessment.at_or_below_safety_reserve {
                (
                    "at_or_below_reserve",
                    Some("Free or add storage before starting ordinary writes."),
                )
            } else if observed.assessment.below_warning_threshold {
                (
                    "low",
                    Some("Review available space before starting large writes."),
                )
            } else {
                ("observed", None)
            };
            json!({"status": status, "evidence": observed, "next_action": next_action})
        }
        Err(error) => json!({
            "status": "unknown",
            "detail": error.to_string(),
            "next_action": "Check that the filesystem is mounted and the directory is readable on a supported platform.",
        }),
    }
}

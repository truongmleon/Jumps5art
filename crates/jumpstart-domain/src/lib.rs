//! Shared identities and value types with the version 1 storage and wire invariants.

mod blob;
mod counter;
mod identity;

pub use blob::BlobId;
pub use counter::{Counter, MAX_COUNTER_VALUE, PositiveCounter};
pub use identity::{
    ClientInstanceId, DeviceId, ItemId, JobId, ResourceId, ServerId, SyncEpoch, VersionId,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// A malformed value cannot enter the domain through constructors or deserialization.
#[derive(Debug, Error)]
pub enum DomainError {
    #[error("{kind} must be a non-nil RFC 9562 UUIDv{expected_version}, got {value}")]
    InvalidUuid {
        kind: &'static str,
        expected_version: u8,
        value: Uuid,
    },
    #[error("invalid UUID: {0}")]
    InvalidUuidString(#[from] uuid::Error),
    #[error("counter {value} is outside {minimum}..=9223372036854775807")]
    CounterOutOfRange { value: u64, minimum: u64 },
    #[error("counter must be a canonical decimal string within the signed 64-bit range")]
    InvalidCounterString,
    #[error("blob ID must be b3: followed by exactly 64 lowercase hexadecimal digits")]
    InvalidBlobId,
}

/// An immutable command identity within one approved installation's local database.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct OperationId {
    pub device_id: DeviceId,
    pub client_instance_id: ClientInstanceId,
    pub seq: PositiveCounter,
}

/// The one app that owns a logical library item; display names never grant ownership.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum Owner {
    Muse,
    Photogenic,
    Mass,
    Bato,
    Flex,
}

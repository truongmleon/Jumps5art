//! Common data types that Jumpstart components use to describe the same things.
//!
//! For example, a song's library record has an ItemId, while the audio file's
//! contents have a separate BlobId fingerprint. OperationId identifies a command,
//! and Owner identifies the app responsible for an item, such as Muse.
//!
//! This file makes those types available and defines their validation errors.
//! Sharing them keeps stored data and app/server messages consistent. The types
//! check values; library updates, file storage, and device communication need
//! separate implementation.

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

/// Describes why an identity, counter, or blob ID failed domain validation.
///
/// Constructors and deserializers return these variants instead of constructing
/// values that violate the version 1 storage and wire rules.
#[derive(Debug, Error)]
pub enum DomainError {
    /// A parsed UUID has the wrong version or variant, or is nil.
    #[error("{kind} must be a non-nil RFC 9562 UUIDv{expected_version}, got {value}")]
    InvalidUuid {
        /// Name of the domain identity type being constructed.
        kind: &'static str,
        /// UUID version required by that identity type.
        expected_version: u8,
        /// Candidate UUID that failed validation.
        value: Uuid,
    },
    /// Text could not be parsed into a UUID by the UUID library.
    #[error("invalid UUID: {0}")]
    InvalidUuidString(#[from] uuid::Error),
    /// A numeric candidate lies outside the counter type's supported interval.
    #[error("counter {value} is outside {minimum}..=9223372036854775807")]
    CounterOutOfRange { value: u64, minimum: u64 },
    /// Counter text is not canonical decimal notation or cannot fit in `u64`.
    #[error("counter must be a canonical decimal string within the signed 64-bit range")]
    InvalidCounterString,
    /// A blob ID lacks its exact algorithm tag or lowercase hexadecimal digest.
    #[error("blob ID must be b3: followed by exactly 64 lowercase hexadecimal digits")]
    InvalidBlobId,
}

/// An immutable command identity within one approved installation's local database.
///
/// Combining the device, database instance, and positive sequence distinguishes
/// commands across installations without using client clocks.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct OperationId {
    /// Approved device whose key originated the command.
    pub device_id: DeviceId,
    /// Durable local application database that allocated the sequence.
    pub client_instance_id: ClientInstanceId,
    /// That instance's command number, beginning at one.
    pub seq: PositiveCounter,
}

/// The one app that owns a logical library item; display names never grant ownership.
///
/// Serde uses these exact variant names in the wire representation. Worlds does
/// not own catalog items through this enum.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum Owner {
    /// Music library ownership.
    Muse,
    /// Photo and video asset ownership.
    Photogenic,
    /// General file and remote-download ownership.
    Mass,
    /// Ebook and manga library ownership.
    Bato,
    /// Movie, television, and anime library ownership.
    Flex,
}

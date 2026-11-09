//! Defines the Mass catalog views shared by storage, the file browser, and mounts.
//!
//! The browser and the mount address the same item IDs. An open file also has
//! a version ID, so repeated reads refer to one immutable copy of its bytes.

use jumpstart_domain::{BlobId, Counter, ItemId, VersionId};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The largest file accepted in a single local preview import.
pub const MAX_MASS_IMPORT: usize = 32 * 1024 * 1024;
/// One independently verified file chunk returned to a reader.
pub const MASS_CHUNK: usize = 1024 * 1024;
/// The fixed upper bound of a directory or search response.
pub const MASS_PAGE: u16 = 100;

/// A browsable entry and, for a file, its current immutable contents.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MassItem {
    pub id: ItemId,
    pub parent: Option<ItemId>,
    pub name: String,
    pub primary_id: Uuid,
    pub created_at_ms: Option<Counter>,
    pub file: Option<MassFile>,
}

/// Identity and length used for a version-pinned read.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MassFile {
    pub version_id: VersionId,
    pub byte_length: Counter,
    pub blob_id: BlobId,
}

/// A bounded listing and the exclusive continuation cursor.
#[derive(Deserialize, Serialize)]
pub struct MassPage {
    pub items: Vec<MassItem>,
    pub next_after: Option<ItemId>,
}

/// A registered destination that an operator can choose explicitly.
#[derive(Deserialize, Serialize)]
pub struct MassPrimary {
    pub id: Uuid,
    pub label: String,
}

/// The namespace fields included in the digest of a local mutation.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MassDestination {
    pub request_id: Uuid,
    pub primary_id: Uuid,
    pub parent: Option<ItemId>,
    pub name: String,
}

/// The exact bytes an import is allowed to publish.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MassImport {
    pub destination: MassDestination,
    pub length: Counter,
    pub blob_id: BlobId,
}

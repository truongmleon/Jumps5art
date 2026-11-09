//! Owns Mass's initial local namespace and the publication transaction.
//!
//! The daemon checks the parent and primary, records the proposed IDs, promotes
//! verified content, and then makes the item visible in the shared catalog.

mod content;

use std::path::Path;

use jumpstart_domain::{BlobId, Counter, ItemId, ResourceId, VersionId};
use rusqlite::{OptionalExtension, Row, Transaction, params};
use serde::Serialize;
use unicode_normalization::is_nfc;
use uuid::Uuid;

use super::AdminMetadata;
use crate::{
    Result, StorageError,
    capacity::measure_open_directory,
    mass::{
        MASS_CHUNK, MASS_PAGE, MAX_MASS_IMPORT, MassDestination, MassFile, MassImport, MassItem,
        MassPage, MassPrimary,
    },
};

const COLUMNS: &str = "i.item_id,i.parent_id,i.name,i.home_primary_id,o.created_at_ms,
                       i.current_version_id,b.byte_length,b.digest,i.kind";
const JOINS: &str = "FROM items i
 LEFT JOIN mass_local_operations o ON o.item_id=i.item_id AND o.completed=1
 LEFT JOIN version_resources r ON r.version_id=i.current_version_id AND r.role='original'
 LEFT JOIN blobs b ON b.algorithm=r.algorithm AND b.digest=r.digest";
const LIVE: &str = "i.owner='mass' AND i.lifecycle='live' AND i.kind IN ('folder','file')";

/// Durable identity shared by the first attempt and later matching retries.
struct Operation {
    sequence: i64,
    item_id: ItemId,
    version_id: Option<VersionId>,
    resource_id: Option<ResourceId>,
    completed: bool,
}

/// The verified placement and hash that authorize one immutable range read.
struct ChunkRecord {
    primary: [u8; 16],
    length: i64,
    blob: [u8; 32],
    digest: [u8; 32],
}

/// Recovery manifest for one file. Fields are emitted in canonical key order.
#[derive(Serialize)]
struct Manifest<'a> {
    format_version: u32,
    home_primary_id: Uuid,
    item_id: ItemId,
    original_format: &'static str,
    owner: &'static str,
    resources: [Original<'a>; 1],
    version_id: VersionId,
}

/// The one original resource described by a Mass file version.
#[derive(Serialize)]
struct Original<'a> {
    blob_id: BlobId,
    byte_length: Counter,
    original_filename: &'a str,
    required: bool,
    resource_id: ResourceId,
    role: &'static str,
}

impl AdminMetadata {
    /// List the finalized primary registrations that Mass can select.
    pub fn mass_primaries(&self) -> Result<Vec<MassPrimary>> {
        self.require_mass()?;
        let mut statement = self.source.connection.prepare(
            "SELECT primary_id,root_path FROM volume_registrations WHERE role='primary'
             AND registered_volume_id IS NOT NULL ORDER BY root_path LIMIT 129",
        )?;
        let mut rows = statement.query([])?;
        let mut primaries = Vec::new();
        while let Some(row) = rows.next()? {
            if primaries.len() == 128 {
                return Err(StorageError::Mass(
                    "too many primary registrations for this preview",
                ));
            }
            let path: String = row.get(1)?;
            let label = Path::new(&path)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Primary")
                .to_owned();
            primaries.push(MassPrimary {
                id: Uuid::from_bytes(row.get(0)?),
                label,
            });
        }
        Ok(primaries)
    }

    /// Read one page of direct children in a folder or the drive root.
    pub fn mass_list(&self, parent: Option<ItemId>, after: Option<ItemId>) -> Result<MassPage> {
        self.require_mass()?;
        if let Some(parent) = parent
            && self.mass_item(parent)?.file.is_some()
        {
            return Err(StorageError::Mass("the selected item is not a folder"));
        }
        self.mass_page(
            "i.parent_id IS ?1",
            params![
                id_bytes(parent.as_ref()),
                id_bytes(after.as_ref()),
                MASS_PAGE + 1
            ],
        )
    }

    /// Find a bounded page of live Mass items by literal filename text.
    pub fn mass_search(&self, query: &str, after: Option<ItemId>) -> Result<MassPage> {
        self.require_mass()?;
        let query = query.trim();
        if query.is_empty() || query.len() > 255 {
            return Err(StorageError::Mass("enter a search between 1 and 255 bytes"));
        }
        self.mass_page(
            "instr(lower(i.name),lower(?1))>0",
            params![query, id_bytes(after.as_ref()), MASS_PAGE + 1],
        )
    }

    /// Resolve one exact sibling name for a native filesystem lookup.
    pub fn mass_lookup(&self, parent: Option<ItemId>, name: &str) -> Result<Option<MassItem>> {
        self.require_mass()?;
        if validate_name(name).is_err() {
            return Ok(None);
        }
        let sql =
            format!("SELECT {COLUMNS} {JOINS} WHERE {LIVE} AND i.parent_id IS ?1 AND i.name=?2");
        let mut statement = self.source.connection.prepare(&sql)?;
        let mut rows = statement.query(params![id_bytes(parent.as_ref()), name])?;
        rows.next()?.map(read_item).transpose()
    }

    /// Read the current metadata for one live Mass entry.
    pub fn mass_item(&self, item: ItemId) -> Result<MassItem> {
        self.require_mass()?;
        let sql = format!("SELECT {COLUMNS} {JOINS} WHERE {LIVE} AND i.item_id=?1");
        let mut statement = self.source.connection.prepare(&sql)?;
        let mut rows = statement.query([item.as_uuid().as_bytes().as_slice()])?;
        let row = rows
            .next()?
            .ok_or(StorageError::Mass("the item is unavailable"))?;
        read_item(row)
    }

    /// Create a folder on the chosen primary using a retryable local request ID.
    pub fn mass_create_folder(&mut self, destination: MassDestination) -> Result<MassItem> {
        let operation = self.prepare(&destination, None)?;
        if !operation.completed {
            let (directory, original) =
                self.registered_primary_root(destination.primary_id, true)?;
            let (_, current) = self.registered_primary_root(destination.primary_id, true)?;
            if original.mount_id != current.mount_id {
                return Err(StorageError::Mass("the primary mount changed"));
            }
            let transaction = self.source.connection.transaction()?;
            insert_item(&transaction, &destination, &operation)?;
            complete(&transaction, &operation)?;
            transaction.commit()?;
            drop(directory);
        }
        self.checkpoint_admin()?;
        self.mass_item(operation.item_id)
    }

    /// Verify and publish an upload, then atomically create its catalog version.
    pub fn mass_import(&mut self, import: MassImport, bytes: &[u8]) -> Result<MassItem> {
        if bytes.len() > MAX_MASS_IMPORT
            || bytes.len() as u64 != import.length.value()
            || BlobId::hash_bytes(bytes) != import.blob_id
        {
            return Err(StorageError::Mass(
                "upload length or BLAKE3 digest did not verify",
            ));
        }
        let operation = self.prepare(&import.destination, Some(&import))?;
        if !operation.completed {
            let id = import.destination.primary_id;
            let (directory, observed) = self.registered_primary_root(id, true)?;
            let capacity = measure_open_directory(&directory, &observed.path)?;
            if capacity.assessment.available_above_reserve_bytes.value()
                < import.length.value() + 65536
            {
                return Err(StorageError::Mass(
                    "waiting for storage above the primary safety reserve",
                ));
            }
            let version = operation
                .version_id
                .ok_or(StorageError::Integrity("missing Mass version ID"))?;
            let resource = operation
                .resource_id
                .ok_or(StorageError::Integrity("missing Mass resource ID"))?;
            let manifest = serde_json::to_vec(&Manifest {
                format_version: 1,
                home_primary_id: id,
                item_id: operation.item_id,
                original_format: "original_bytes",
                owner: "mass",
                resources: [Original {
                    blob_id: import.blob_id,
                    byte_length: import.length,
                    original_filename: &import.destination.name,
                    required: true,
                    resource_id: resource,
                    role: "original",
                }],
                version_id: version,
            })?;
            content::publish(
                &directory,
                import.blob_id,
                bytes,
                &format!("{version}.json"),
                &manifest,
            )?;
            let (_, current) = self.registered_primary_root(id, true)?;
            if observed.mount_id != current.mount_id {
                return Err(StorageError::Mass(
                    "the primary mount changed during publication",
                ));
            }
            let transaction = self.source.connection.transaction()?;
            insert_item(&transaction, &import.destination, &operation)?;
            insert_file(
                &transaction,
                &operation,
                &import,
                bytes,
                BlobId::hash_bytes(&manifest),
            )?;
            complete(&transaction, &operation)?;
            transaction.commit()?;
        }
        self.checkpoint_admin()?;
        self.mass_item(operation.item_id)
    }

    /// Read one verified chunk from a specific live file version and its home primary.
    pub fn mass_read(&self, item: ItemId, version: VersionId, index: u32) -> Result<Vec<u8>> {
        self.require_mass()?;
        let record = self
            .source
            .connection
            .query_row(
                "SELECT i.home_primary_id,b.byte_length,b.digest,c.digest FROM items i
             JOIN versions v ON v.item_id=i.item_id AND v.version_id=?2
             JOIN version_resources r ON r.version_id=v.version_id AND r.role='original'
             JOIN blobs b ON b.algorithm=r.algorithm AND b.digest=r.digest
             JOIN blob_placements p ON p.algorithm=b.algorithm AND p.digest=b.digest
                AND p.primary_id=i.home_primary_id AND p.state='verified'
             JOIN mass_content_chunks c ON c.version_id=v.version_id AND c.chunk_index=?3
             WHERE i.item_id=?1 AND i.owner='mass' AND i.lifecycle='live' AND i.kind='file'",
                params![
                    item.as_uuid().as_bytes().as_slice(),
                    version.as_uuid().as_bytes().as_slice(),
                    index
                ],
                |row| {
                    Ok(ChunkRecord {
                        primary: row.get(0)?,
                        length: row.get(1)?,
                        blob: row.get(2)?,
                        digest: row.get(3)?,
                    })
                },
            )
            .optional()?;
        let record = record.ok_or(StorageError::Mass("the file chunk is unavailable"))?;
        let (directory, _) =
            self.registered_primary_root(Uuid::from_bytes(record.primary), false)?;
        content::read_chunk(
            &directory,
            BlobId::from_bytes(record.blob),
            counter(record.length)?.value(),
            index,
            BlobId::from_bytes(record.digest),
        )
    }

    /// Refuse Mass access until the metadata lease and catalog schema verify.
    fn require_mass(&self) -> Result<()> {
        self.check_binding()?;
        if self.report().schema_version < 8 {
            return Err(StorageError::Mass("migrate metadata before opening Mass"));
        }
        Ok(())
    }

    /// Read one bounded page using a fixed, caller-selected namespace predicate.
    fn mass_page(&self, predicate: &str, values: impl rusqlite::Params) -> Result<MassPage> {
        let sql = format!(
            "SELECT {COLUMNS} {JOINS} WHERE {LIVE} AND {predicate}
                          AND (?2 IS NULL OR i.item_id>?2) ORDER BY i.item_id LIMIT ?3"
        );
        let mut statement = self.source.connection.prepare(&sql)?;
        let mut rows = statement.query(values)?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(read_item(row)?);
        }
        let next_after = if items.len() > usize::from(MASS_PAGE) {
            items.pop();
            items.last().map(|item| item.id)
        } else {
            None
        };
        Ok(MassPage { items, next_after })
    }

    /// Bind an operator request to immutable inputs and persist its IDs before publication.
    fn prepare(
        &mut self,
        destination: &MassDestination,
        import: Option<&MassImport>,
    ) -> Result<Operation> {
        self.require_mass()?;
        if self.maintenance()? == Some(true) {
            return Err(StorageError::AdminUnavailable("maintenance is enabled"));
        }
        validate_name(&destination.name)?;
        let digest = BlobId::hash_bytes(&serde_json::to_vec(&(
            destination,
            import.map(|value| (value.length, value.blob_id)),
        ))?);
        self.ensure_writer()?;
        let existing = load_operation(&self.source.connection, destination.request_id, digest)?;
        if existing
            .as_ref()
            .is_some_and(|operation| operation.completed)
        {
            return existing.ok_or(StorageError::Integrity("Mass receipt disappeared"));
        }
        if let Some(parent) = destination.parent {
            let parent = self.mass_item(parent)?;
            if parent.file.is_some() || parent.primary_id != destination.primary_id {
                return Err(StorageError::Mass("the folder is on a different primary"));
            }
        }
        let occupied: bool = self.source.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM items WHERE owner='mass' AND lifecycle='live' AND parent_id IS ?1 AND name=?2)",
            params![id_bytes(destination.parent.as_ref()), destination.name], |row| row.get(0),
        )?;
        if occupied {
            return Err(StorageError::Mass(
                "a file or folder with that name already exists",
            ));
        }
        self.registered_primary_root(destination.primary_id, true)?;
        if let Some(operation) = existing {
            self.checkpoint_admin()?;
            return Ok(operation);
        }
        let item = ItemId::new();
        let version = import.map(|_| VersionId::new());
        let resource = import.map(|_| ResourceId::new());
        self.source.connection.execute(
            "INSERT INTO mass_local_operations (request_id,request_digest,item_id,version_id,resource_id,created_at_ms,completed)
             VALUES (?1,?2,?3,?4,?5,CAST(unixepoch('subsec')*1000 AS INTEGER),0)",
            params![
                destination.request_id.as_bytes().as_slice(), digest.as_bytes().as_slice(), item.as_uuid().as_bytes().as_slice(),
                version.as_ref().map(|id| id.as_uuid().as_bytes().as_slice()),
                resource.as_ref().map(|id| id.as_uuid().as_bytes().as_slice()),
            ],
        )?;
        let sequence = self.source.connection.last_insert_rowid();
        self.checkpoint_admin()?;
        Ok(Operation {
            sequence,
            item_id: item,
            version_id: version,
            resource_id: resource,
            completed: false,
        })
    }
}

/// Reopen a saved operation only if the full request digest agrees.
fn load_operation(
    connection: &rusqlite::Connection,
    request_id: Uuid,
    digest: BlobId,
) -> Result<Option<Operation>> {
    let mut statement = connection.prepare(
        "SELECT sequence,request_digest,item_id,version_id,resource_id,completed
         FROM mass_local_operations WHERE request_id=?1",
    )?;
    let mut rows = statement.query([request_id.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    if row.get::<_, [u8; 32]>(1)? != *digest.as_bytes() {
        return Err(StorageError::Mass(
            "this request ID was previously used for different inputs",
        ));
    }
    Ok(Some(Operation {
        sequence: row.get(0)?,
        item_id: item_id(row.get(2)?)?,
        version_id: row
            .get::<_, Option<[u8; 16]>>(3)?
            .map(version_id)
            .transpose()?,
        resource_id: row
            .get::<_, Option<[u8; 16]>>(4)?
            .map(|raw| {
                ResourceId::from_uuid(Uuid::from_bytes(raw))
                    .map_err(|_| StorageError::Integrity("invalid Mass resource ID"))
            })
            .transpose()?,
        completed: row.get(5)?,
    }))
}

/// Add the namespace entry, deferring its version reference until commit.
fn insert_item(
    transaction: &Transaction<'_>,
    destination: &MassDestination,
    operation: &Operation,
) -> Result<()> {
    let version = operation
        .version_id
        .as_ref()
        .map(|id| id.as_uuid().as_bytes().as_slice());
    transaction.execute(
        "INSERT INTO items (item_id,incarnation,owner,kind,home_primary_id,parent_id,name,lifecycle,current_version_id,revision)
         VALUES (?1,?2,'mass',?3,?4,?5,?6,'live',?7,1)",
        params![
            operation.item_id.as_uuid().as_bytes().as_slice(), Uuid::new_v4().as_bytes().as_slice(),
            if version.is_some() { "file" } else { "folder" },
            destination.primary_id.as_bytes().as_slice(), id_bytes(destination.parent.as_ref()), destination.name, version,
        ],
    )?;
    Ok(())
}

/// Add a version, its original resource, its verified placement and its chunk hashes.
fn insert_file(
    transaction: &Transaction<'_>,
    operation: &Operation,
    import: &MassImport,
    bytes: &[u8],
    manifest: BlobId,
) -> Result<()> {
    let version = operation
        .version_id
        .ok_or(StorageError::Integrity("missing Mass version"))?;
    let resource = operation
        .resource_id
        .ok_or(StorageError::Integrity("missing Mass original"))?;
    let version = version.as_uuid().as_bytes().as_slice();
    let blob = import.blob_id.as_bytes().as_slice();
    let length = bytes.len() as i64;
    if transaction.execute(
        "INSERT INTO blobs VALUES ('b3',?1,?2) ON CONFLICT(algorithm,digest)
         DO UPDATE SET byte_length=excluded.byte_length WHERE blobs.byte_length=excluded.byte_length",
        params![blob, length],
    )? != 1 {
        return Err(StorageError::Integrity("Mass blob length conflicts with the catalog"));
    }
    if transaction.execute(
        "INSERT INTO blob_placements VALUES ('b3',?1,?2,'verified',CAST(unixepoch('subsec')*1000 AS INTEGER))
         ON CONFLICT(algorithm,digest,primary_id) DO UPDATE SET verified_at_ms=excluded.verified_at_ms
         WHERE blob_placements.state='verified'",
        params![blob, import.destination.primary_id.as_bytes().as_slice()],
    )? != 1 {
        return Err(StorageError::Mass("the original placement requires explicit repair"));
    }
    transaction.execute(
        "INSERT INTO versions VALUES (?1,?2,?3,1,'b3',?4)",
        params![
            version,
            operation.item_id.as_uuid().as_bytes().as_slice(),
            operation.sequence,
            manifest.as_bytes().as_slice()
        ],
    )?;
    transaction.execute(
        "INSERT INTO version_resources (version_id,resource_id,role,original_filename,algorithm,digest,required)
         VALUES (?1,?2,'original',?3,'b3',?4,1)",
        params![version, resource.as_uuid().as_bytes().as_slice(), import.destination.name, blob],
    )?;
    let mut statement = transaction.prepare("INSERT INTO mass_content_chunks VALUES (?1,?2,?3)")?;
    for (index, chunk) in bytes.chunks(MASS_CHUNK).enumerate() {
        let hash = BlobId::hash_bytes(chunk);
        statement.execute(params![version, index as i64, hash.as_bytes().as_slice()])?;
    }
    Ok(())
}

/// Commit the receipt with the catalog change so an acknowledged retry is a read.
fn complete(transaction: &Transaction<'_>, operation: &Operation) -> Result<()> {
    if transaction.execute(
        "UPDATE mass_local_operations SET completed=1 WHERE sequence=?1 AND completed=0",
        [operation.sequence],
    )? != 1
    {
        return Err(StorageError::Integrity(
            "Mass operation changed during publication",
        ));
    }
    Ok(())
}

/// Check the same UTF-8 NFC name rules before either the browser or mount can see it.
fn validate_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.len() > 255
        || matches!(name, "." | "..")
        || name.contains('/')
        || name.chars().any(char::is_control)
        || !is_nfc(name)
    {
        return Err(StorageError::Mass(
            "use an NFC filename of 1–255 bytes without slashes or control characters",
        ));
    }
    Ok(())
}

/// Decode one joined catalog row without inventing a version or a size.
fn read_item(row: &Row<'_>) -> Result<MassItem> {
    let file = match row.get::<_, Option<[u8; 16]>>(5)? {
        Some(version) => Some(MassFile {
            version_id: version_id(version)?,
            byte_length: counter(row.get(6)?)?,
            blob_id: BlobId::from_bytes(row.get(7)?),
        }),
        None => None,
    };
    if file.is_some() != (row.get::<_, String>(8)? == "file") {
        return Err(StorageError::Integrity(
            "Mass entry kind disagrees with its version",
        ));
    }
    Ok(MassItem {
        id: item_id(row.get(0)?)?,
        parent: row
            .get::<_, Option<[u8; 16]>>(1)?
            .map(item_id)
            .transpose()?,
        name: row.get(2)?,
        primary_id: Uuid::from_bytes(row.get(3)?),
        created_at_ms: row.get::<_, Option<i64>>(4)?.map(counter).transpose()?,
        file,
    })
}

/// Borrow the SQLite representation of an optional parent or cursor.
fn id_bytes(id: Option<&ItemId>) -> Option<&[u8]> {
    id.map(|id| id.as_uuid().as_bytes().as_slice())
}

/// Convert stored bytes into a checked item ID.
fn item_id(raw: [u8; 16]) -> Result<ItemId> {
    ItemId::from_uuid(Uuid::from_bytes(raw))
        .map_err(|_| StorageError::Integrity("invalid Mass item ID"))
}

/// Convert stored bytes into a checked version ID.
fn version_id(raw: [u8; 16]) -> Result<VersionId> {
    VersionId::from_uuid(Uuid::from_bytes(raw))
        .map_err(|_| StorageError::Integrity("invalid Mass version ID"))
}

/// Convert a nonnegative SQLite counter into its canonical wire representation.
fn counter(value: i64) -> Result<Counter> {
    u64::try_from(value)
        .ok()
        .and_then(|value| Counter::from_u64(value).ok())
        .ok_or(StorageError::Integrity("invalid Mass counter"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{Seek, SeekFrom, Write},
        os::fd::OwnedFd,
    };

    use super::*;

    /// Preserve verified chunks and refuse corruption without changing their catalog identity.
    #[test]
    fn file_publication_is_replayable_and_reads_refuse_a_changed_chunk() {
        let temp = tempfile::tempdir().unwrap();
        let directory: OwnedFd = File::open(temp.path()).unwrap().into();
        let mut bytes = vec![b'a'; MASS_CHUNK];
        bytes.extend_from_slice(b"tail");
        let hash = BlobId::hash_bytes(&bytes);
        let tail = BlobId::hash_bytes(b"tail");
        content::publish(&directory, hash, &bytes, "version.json", b"{}").unwrap();
        content::publish(&directory, hash, &bytes, "version.json", b"{}").unwrap();
        assert_eq!(
            content::read_chunk(&directory, hash, bytes.len() as u64, 1, tail).unwrap(),
            b"tail"
        );
        let digest = hash.to_string();
        let path = temp
            .path()
            .join("objects/b3")
            .join(&digest[3..5])
            .join(&digest[5..]);
        let mut file = File::options().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(MASS_CHUNK as u64)).unwrap();
        file.write_all(b"fail").unwrap();
        assert!(content::read_chunk(&directory, hash, bytes.len() as u64, 1, tail).is_err());
        assert!(content::publish(&directory, hash, &bytes, "version.json", b"{}").is_err());
    }

    /// Make a whole catalog version visible in one transaction after publication.
    #[test]
    fn publication_transaction_records_the_file_and_chunk_hash() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("metadata");
        crate::initialize(&root).unwrap();
        let mut lease = AdminMetadata::open(&root).unwrap();
        lease.ensure_writer().unwrap();
        let primary = Uuid::new_v4();
        lease
            .source
            .connection
            .execute(
                "INSERT INTO volumes VALUES (?1,'primary',?2,'fixture','unknown',NULL)",
                params![
                    Uuid::new_v4().as_bytes().as_slice(),
                    primary.as_bytes().as_slice()
                ],
            )
            .unwrap();
        let import = MassImport {
            destination: MassDestination {
                request_id: Uuid::new_v4(),
                primary_id: primary,
                parent: None,
                name: "sample.txt".into(),
            },
            length: Counter::from_u64(6).unwrap(),
            blob_id: BlobId::hash_bytes(b"sample"),
        };
        let operation = Operation {
            sequence: 1,
            item_id: ItemId::new(),
            version_id: Some(VersionId::new()),
            resource_id: Some(ResourceId::new()),
            completed: false,
        };
        let tx = lease.source.connection.transaction().unwrap();
        insert_item(&tx, &import.destination, &operation).unwrap();
        insert_file(
            &tx,
            &operation,
            &import,
            b"sample",
            BlobId::hash_bytes(b"{}"),
        )
        .unwrap();
        tx.commit().unwrap();
        lease.checkpoint_admin().unwrap();
        let file = lease.mass_item(operation.item_id).unwrap().file.unwrap();
        assert_eq!(file.blob_id, import.blob_id);
        assert_eq!(file.version_id, operation.version_id.unwrap());
        assert_eq!(
            lease.mass_search("SAM", None).unwrap().items[0].id,
            operation.item_id
        );
    }
}

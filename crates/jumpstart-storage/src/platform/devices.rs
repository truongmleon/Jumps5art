//! Saves device approval and revocation under the daemon's checked writer lease.
//!
//! A device's public key carries authority. Names are display text. Keeping
//! revoked keys in SQLite prevents a later invitation from restoring access.

use jumpstart_domain::{ClientInstanceId, Counter, DeviceId, Owner};
use rusqlite::{OptionalExtension, Row, params};
use serde::Serialize;
use uuid::Uuid;

use super::AdminMetadata;
use crate::{Result, StorageError};

const COLUMNS: &str =
    "device_id,public_key,name,platform,app,approved_at_ms,last_seen_at_ms,revoked_at_ms";
const MAX_PAGE: u16 = 100;

/// A grant assembled by the pairing service after authenticated local approval.
pub struct DeviceRegistration {
    pub device_id: DeviceId,
    pub public_key: [u8; 32],
    pub name: String,
    pub platform: String,
    pub app: Owner,
    pub instance_id: ClientInstanceId,
    pub signed_receipt: Vec<u8>,
}

/// Operator-visible installation identity and persisted authority state.
#[derive(Serialize)]
pub struct DeviceRecord {
    pub device_id: DeviceId,
    pub fingerprint: String,
    pub name: String,
    pub platform: String,
    pub app: String,
    pub approved_at_ms: Counter,
    pub last_seen_at_ms: Option<Counter>,
    pub revoked_at_ms: Option<Counter>,
}

/// One bounded page of installations with an exclusive continuation cursor.
#[derive(Serialize)]
pub struct DevicePage {
    pub devices: Vec<DeviceRecord>,
    pub next_after: Option<DeviceId>,
}

impl AdminMetadata {
    /// Read up to `limit` installations in ID order, optionally after a cursor.
    pub fn list_devices(&self, after: Option<DeviceId>, limit: u16) -> Result<DevicePage> {
        self.require_device_catalog()?;
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(StorageError::InvalidDevice(
                "device page limit must be in 1..=100",
            ));
        }
        let sql = format!(
            "SELECT {COLUMNS} FROM devices WHERE ?1 IS NULL OR device_id>?1
             ORDER BY device_id LIMIT ?2"
        );
        let mut statement = self.source.connection.prepare(&sql)?;
        let mut rows = statement.query(params![
            after.as_ref().map(|id| id.as_uuid().as_bytes().as_slice()),
            u32::from(limit) + 1,
        ])?;
        let mut devices = Vec::new();
        while let Some(row) = rows.next()? {
            devices.push(read_device(row)?);
        }
        let next_after = if devices.len() > usize::from(limit) {
            devices.pop();
            devices.last().map(|device| device.device_id)
        } else {
            None
        };
        Ok(DevicePage {
            devices,
            next_after,
        })
    }

    /// Revoke `device_id` durably and return the original outcome on retries.
    ///
    /// Revocation can still reduce access while the maintenance fence is active.
    pub fn revoke_device(&mut self, device_id: DeviceId) -> Result<DeviceRecord> {
        self.require_device_catalog()?;
        self.get_device(device_id)?;
        self.ensure_writer()?;
        self.source.connection.execute(
            "UPDATE devices SET revoked_at_ms=CAST(unixepoch('subsec')*1000 AS INTEGER)
             WHERE device_id=?1 AND revoked_at_ms IS NULL",
            [device_id.as_uuid().as_bytes().as_slice()],
        )?;
        self.checkpoint_admin()?;
        self.get_device(device_id)
    }

    /// Save an approved key and its first database together before replying.
    ///
    /// The authenticated pairing service supplies the signed receipt. This
    /// writer refuses reused keys and instance IDs, including revoked keys.
    pub fn register_device(&mut self, grant: DeviceRegistration) -> Result<DeviceRecord> {
        self.require_device_catalog()?;
        if self.maintenance()? == Some(true) {
            return Err(StorageError::AdminUnavailable("maintenance is enabled"));
        }
        validate_label(&grant.name, 100)?;
        validate_label(&grant.platform, 32)?;
        if !(1..=4096).contains(&grant.signed_receipt.len()) {
            return Err(StorageError::InvalidDevice(
                "pairing receipt is missing or oversized",
            ));
        }
        self.ensure_writer()?;
        let authority = self.report().server_id;
        let tx = self.source.connection.transaction()?;
        let key_in_use: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE public_key=?1)",
            [grant.public_key.as_slice()],
            |row| row.get(0),
        )?;
        if key_in_use {
            return Err(StorageError::InvalidDevice(
                "this key was previously registered",
            ));
        }
        tx.execute(
            "INSERT INTO devices (device_id,server_id,public_key,name,platform,app,approved_at_ms,pairing_receipt)
             VALUES (?1,?2,?3,?4,?5,?6,CAST(unixepoch('subsec')*1000 AS INTEGER),?7)",
            params![
                grant.device_id.as_uuid().as_bytes().as_slice(),
                authority.as_uuid().as_bytes().as_slice(),
                grant.public_key.as_slice(),
                grant.name,
                grant.platform,
                app_name(grant.app),
                grant.signed_receipt,
            ],
        )?;
        tx.execute(
            "INSERT INTO client_instances (instance_id,device_id,next_expected,last_acked_cursor)
             VALUES (?1,?2,1,0)",
            params![
                grant.instance_id.as_uuid().as_bytes().as_slice(),
                grant.device_id.as_uuid().as_bytes().as_slice(),
            ],
        )?;
        tx.commit()?;
        self.checkpoint_admin()?;
        self.get_device(grant.device_id)
    }

    /// Check whether this key was ever registered, including revoked keys.
    pub fn device_key_known(&self, key: &[u8; 32]) -> Result<bool> {
        self.require_device_catalog()?;
        Ok(self.source.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM devices WHERE public_key=?1)",
            [key.as_slice()],
            |row| row.get(0),
        )?)
    }

    /// Return an active pairing receipt only for a transport-authenticated key.
    pub fn device_receipt(&self, key: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        self.require_device_catalog()?;
        Ok(self
            .source
            .connection
            .query_row(
                "SELECT pairing_receipt FROM devices WHERE public_key=?1 AND revoked_at_ms IS NULL",
                [key.as_slice()],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Return the saved operator view of one exact device or refuse an unknown ID.
    fn get_device(&self, device_id: DeviceId) -> Result<DeviceRecord> {
        let sql = format!("SELECT {COLUMNS} FROM devices WHERE device_id=?1");
        let mut statement = self.source.connection.prepare(&sql)?;
        let mut rows = statement.query([device_id.as_uuid().as_bytes().as_slice()])?;
        let row = rows
            .next()?
            .ok_or(StorageError::InvalidDevice("unknown device ID"))?;
        read_device(row)
    }

    /// Ensure authority reads and writes refer to the checked schema and lease.
    fn require_device_catalog(&self) -> Result<()> {
        self.check_binding()?;
        if self.report().schema_version < 7 {
            return Err(StorageError::AdminUnavailable(
                "migrate offline metadata before managing devices",
            ));
        }
        Ok(())
    }
}

/// A short public-key fingerprint for the operator's comparison.
pub fn device_fingerprint(key: &[u8; 32]) -> String {
    blake3::hash(key).to_hex().as_str()[..16].to_owned()
}

/// Decode checked SQL values into the portable device view.
fn read_device(row: &Row<'_>) -> Result<DeviceRecord> {
    let key: [u8; 32] = row.get(1)?;
    let approved = row.get::<_, i64>(5)?;
    Ok(DeviceRecord {
        device_id: DeviceId::from_uuid(Uuid::from_bytes(row.get(0)?))
            .map_err(|_| StorageError::Integrity("invalid device ID"))?,
        fingerprint: device_fingerprint(&key),
        name: row.get(2)?,
        platform: row.get(3)?,
        app: row.get(4)?,
        approved_at_ms: timestamp(approved)?,
        last_seen_at_ms: row.get::<_, Option<i64>>(6)?.map(timestamp).transpose()?,
        revoked_at_ms: row.get::<_, Option<i64>>(7)?.map(timestamp).transpose()?,
    })
}

/// Validate an SQLite timestamp before exposing it as a wire counter.
fn timestamp(value: i64) -> Result<Counter> {
    u64::try_from(value)
        .ok()
        .and_then(|value| Counter::from_u64(value).ok())
        .ok_or(StorageError::Integrity("invalid device timestamp"))
}

/// Refuse empty, oversized, or terminal-control display fields.
fn validate_label(value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(StorageError::InvalidDevice(
            "invalid device name or platform",
        ));
    }
    Ok(())
}

/// Encode a library installation's app using the persisted catalog spelling.
fn app_name(app: Owner) -> &'static str {
    match app {
        Owner::Muse => "muse",
        Owner::Photogenic => "photogenic",
        Owner::Mass => "mass",
        Owner::Bato => "bato",
        Owner::Flex => "flex",
    }
}

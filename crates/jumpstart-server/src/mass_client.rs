//! Gives Mass frontends one checked connection API for catalog and file reads.
//!
//! The browser bridge and the mount call this client, so a mounted file and
//! the browser's Download action refer to the same version and byte ranges.

use std::{error::Error, path::PathBuf};

use jumpstart_domain::{BlobId, Counter, ItemId, VersionId};
use jumpstart_storage::mass::{
    MAX_MASS_IMPORT, MassDestination, MassImport, MassItem, MassPage, MassPrimary,
};
use serde::de::DeserializeOwned;

use crate::local_admin::{Command, MassCommand, call_data};

/// A service-user client for the local Mass namespace.
#[derive(Clone)]
pub struct MassClient {
    socket: PathBuf,
}

impl MassClient {
    /// Connect future Mass requests through the selected private socket.
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// List selectable primary destinations for new folders and imports.
    pub fn primaries(&self) -> Result<Vec<MassPrimary>, Box<dyn Error>> {
        self.get(MassCommand::Primaries)
    }

    /// List one page of direct children of a folder or the drive root.
    pub fn list(
        &self,
        parent: Option<ItemId>,
        after: Option<ItemId>,
    ) -> Result<MassPage, Box<dyn Error>> {
        self.get(MassCommand::List { parent, after })
    }

    /// Search one page of the drive by filename text.
    pub fn search(&self, query: String, after: Option<ItemId>) -> Result<MassPage, Box<dyn Error>> {
        self.get(MassCommand::Search { query, after })
    }

    /// Resolve one canonical name beneath a known parent for a filesystem lookup.
    pub fn lookup(
        &self,
        parent: Option<ItemId>,
        name: String,
    ) -> Result<Option<MassItem>, Box<dyn Error>> {
        self.get(MassCommand::Lookup { parent, name })
    }

    /// Return current catalog metadata for one stable entry ID.
    pub fn item(&self, id: ItemId) -> Result<MassItem, Box<dyn Error>> {
        self.get(MassCommand::Item { id })
    }

    /// Create a folder with the provided stable request ID.
    pub fn create_folder(&self, destination: MassDestination) -> Result<MassItem, Box<dyn Error>> {
        self.get(MassCommand::CreateFolder { destination })
    }

    /// Send original bytes with their exact length and BLAKE3 identity.
    pub fn import(
        &self,
        destination: MassDestination,
        bytes: &[u8],
    ) -> Result<MassItem, Box<dyn Error>> {
        if bytes.len() > MAX_MASS_IMPORT {
            return Err("the Mass upload exceeds the preview limit".into());
        }
        let import = MassImport {
            destination,
            length: Counter::from_u64(bytes.len() as u64)?,
            blob_id: BlobId::hash_bytes(bytes),
        };
        let (report, _) = call_data(
            &self.socket,
            Command::Mass {
                request: MassCommand::Import { import },
            },
            Some(bytes),
        )?;
        Ok(serde_json::from_value(report)?)
    }

    /// Read one bounded chunk of an immutable file version.
    pub fn read(
        &self,
        id: ItemId,
        version: VersionId,
        chunk: u32,
    ) -> Result<Vec<u8>, Box<dyn Error>> {
        let (_, bytes) = call_data(
            &self.socket,
            Command::Mass {
                request: MassCommand::Read { id, version, chunk },
            },
            None,
        )?;
        Ok(bytes)
    }

    /// Exchange a typed Mass control request and decode its report.
    fn get<T: DeserializeOwned>(&self, request: MassCommand) -> Result<T, Box<dyn Error>> {
        let (report, _) = call_data(&self.socket, Command::Mass { request }, None)?;
        Ok(serde_json::from_value(report)?)
    }
}

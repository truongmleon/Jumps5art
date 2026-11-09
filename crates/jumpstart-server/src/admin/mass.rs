//! Carries local Mass commands and bounded raw bytes across the protected socket.
//!
//! The checked metadata lease owns all catalog and disk changes. Browser and
//! filesystem clients submit typed requests rather than opening server paths.

use std::error::Error;

use jumpstart_server::local_admin::{MassCommand, Reply, write};
use jumpstart_storage::{AdminMetadata, StorageError, mass::MAX_MASS_IMPORT};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

/// Execute one Mass command after the daemon has authenticated its local caller.
pub(super) async fn reply(
    stream: &mut UnixStream,
    metadata: &mut Result<AdminMetadata, StorageError>,
    request: MassCommand,
) -> Result<(), Box<dyn Error>> {
    let result = execute(stream, metadata, request).await;
    match result {
        Ok((report, bytes)) => {
            write(stream, &Reply::Ok { report }).await?;
            if let Some(bytes) = bytes {
                stream.write_all(&bytes).await?;
            }
        }
        Err(error) => {
            write(
                stream,
                &Reply::Error {
                    message: error.to_string(),
                },
            )
            .await?
        }
    }
    Ok(())
}

/// Read exact raw upload bytes and run the matching storage operation.
async fn execute(
    stream: &mut UnixStream,
    metadata: &mut Result<AdminMetadata, StorageError>,
    request: MassCommand,
) -> Result<(Value, Option<Vec<u8>>), Box<dyn Error>> {
    if let MassCommand::Import { import } = request {
        let length = usize::try_from(import.length.value())?;
        if length > MAX_MASS_IMPORT {
            return Err("the Mass upload exceeds the preview limit".into());
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).await?;
        let lease = metadata.as_mut().map_err(|error| error.to_string())?;
        return Ok((
            serde_json::to_value(lease.mass_import(import, &bytes)?)?,
            None,
        ));
    }
    let lease = metadata.as_mut().map_err(|error| error.to_string())?;
    let report = match request {
        MassCommand::Primaries => serde_json::to_value(lease.mass_primaries()?)?,
        MassCommand::List { parent, after } => {
            serde_json::to_value(lease.mass_list(parent, after)?)?
        }
        MassCommand::Search { query, after } => {
            serde_json::to_value(lease.mass_search(&query, after)?)?
        }
        MassCommand::Lookup { parent, name } => {
            serde_json::to_value(lease.mass_lookup(parent, &name)?)?
        }
        MassCommand::Item { id } => serde_json::to_value(lease.mass_item(id)?)?,
        MassCommand::CreateFolder { destination } => {
            serde_json::to_value(lease.mass_create_folder(destination)?)?
        }
        MassCommand::Read { id, version, chunk } => {
            let bytes = lease.mass_read(id, version, chunk)?;
            return Ok((json!({"length": bytes.len()}), Some(bytes)));
        }
        MassCommand::Import { .. } => unreachable!("upload already handled"),
    };
    Ok((report, None))
}

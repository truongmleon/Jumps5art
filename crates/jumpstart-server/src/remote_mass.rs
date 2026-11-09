//! Defines the paired, read-only Mass RPC subset used by the Finder helper.
//!
//! Iroh authenticates the connection key. The receiver checks the paired
//! database instance separately before catalog metadata or bytes are returned.

use std::{error::Error, time::Duration};

use iroh::endpoint::{RecvStream, SendStream};
use jumpstart_domain::{BlobId, Counter, ItemId, Owner, VersionId};
use jumpstart_protocol::{
    Capability, ErrorCode, MAX_FRAME_BYTES, ProtocolError, Welcome, decode_body, write_frame,
};
use jumpstart_storage::mass::{MassItem, MassPage};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

/// The regular application protocol, separate from temporary pairing.
pub const ALPN: &[u8] = b"jumpstart/1";
/// Bounds one request and its verified range body.
pub const DEADLINE: Duration = Duration::from_secs(15);
/// A capability required by the native Mass helper.
pub const FEATURE: &str = "mass.read";

/// Typed catalog and transfer requests accepted by the Mass preview.
#[derive(Deserialize, Serialize)]
#[serde(tag = "method", deny_unknown_fields)]
pub enum Call {
    #[serde(rename = "Catalog.List")]
    List {
        request_id: Uuid,
        schema_version: u16,
        body: List,
    },
    #[serde(rename = "Catalog.Get")]
    Get {
        request_id: Uuid,
        schema_version: u16,
        body: Get,
    },
    #[serde(rename = "Transfer.ReadRange")]
    Range {
        request_id: Uuid,
        schema_version: u16,
        body: Range,
    },
}

impl Call {
    /// Return the request ID and payload version shared by all read families.
    pub fn envelope(&self) -> (Uuid, u16) {
        match self {
            Self::List {
                request_id,
                schema_version,
                ..
            }
            | Self::Get {
                request_id,
                schema_version,
                ..
            }
            | Self::Range {
                request_id,
                schema_version,
                ..
            } => (*request_id, *schema_version),
        }
    }
}

/// List direct children in a selected library using a stable cursor.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub owner: Owner,
    pub parent: Option<ItemId>,
    pub page_token: Option<ItemId>,
}

/// Fetch a bounded collection of items visible to the Mass device.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Get {
    pub ids: Vec<ItemId>,
}

/// Read the exact range of an identified immutable file version.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Range {
    pub item_id: ItemId,
    pub version_id: VersionId,
    pub offset: Counter,
    pub length: Counter,
}

/// Catalog pages use the application contract's entity and page token names.
#[derive(Deserialize, Serialize)]
pub struct CatalogPage {
    pub entities: Vec<MassItem>,
    pub next_page: Option<ItemId>,
}

impl From<MassPage> for CatalogPage {
    /// Preserve the catalog's ordering and continuation cursor across transports.
    fn from(page: MassPage) -> Self {
        Self {
            entities: page.items,
            next_page: page.next_after,
        }
    }
}

/// Returns requested items and distinguishes invisible or absent identities.
#[derive(Deserialize, Serialize)]
pub struct CatalogItems {
    pub entities: Vec<MassItem>,
    pub missing: Vec<ItemId>,
}

/// Exact byte bounds echoed before the raw range is sent.
#[derive(Deserialize, Serialize)]
pub struct AcceptedRange {
    pub offset: Counter,
    pub length: Counter,
}

/// Identifies the read and binds its body to the immutable blob and range digest.
#[derive(Deserialize, Serialize)]
pub struct RangeReply {
    pub read_id: Uuid,
    pub accepted_range: AcceptedRange,
    pub blob_id: BlobId,
    pub range_hash: BlobId,
}

/// The version negotiation result sent before any RPC.
#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Greeting {
    Ready { welcome: Welcome },
    Error { error: ProtocolError },
}

/// Match an RPC reply with the exact request that caused it.
#[derive(Deserialize, Serialize)]
pub struct Reply<T> {
    pub request_id: Uuid,
    pub result: Outcome<T>,
}

/// One typed success or structured refusal.
#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome<T> {
    Ok { value: T },
    Error { error: ProtocolError },
}

/// Construct a bounded failure without disclosing an internal path or SQL error.
pub fn refusal(code: ErrorCode, message: &'static str) -> ProtocolError {
    ProtocolError {
        code,
        state: "rejected".into(),
        message: message.into(),
        retryable: matches!(code, ErrorCode::Unavailable | ErrorCode::Quota),
        retry_after_ms: None,
        details: Default::default(),
    }
}

/// Return the versioned read capability carried by both sides of the handshake.
pub fn capability() -> Capability {
    Capability::new(FEATURE).expect("the fixed Mass capability is valid")
}

/// Read a bounded JSON frame without trusting the declared allocation size.
pub async fn read<T: DeserializeOwned>(
    stream: &mut RecvStream,
) -> Result<T, Box<dyn Error + Send + Sync>> {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).await?;
    let length = u32::from_be_bytes(prefix) as usize;
    if !(1..=MAX_FRAME_BYTES).contains(&length) {
        return Err("invalid Mass frame length".into());
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    Ok(decode_body(&body)?)
}

/// Write a protocol frame before any following raw bytes.
pub async fn write(
    stream: &mut SendStream,
    value: &impl Serialize,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, value)?;
    stream.write_all(&bytes).await?;
    Ok(())
}

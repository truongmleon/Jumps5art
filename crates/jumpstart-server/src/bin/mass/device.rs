//! Implements device enrollment and verified catalog and file operations for Finder.
//!
//! Each process opens one pinned iroh session. A file is materialized into a
//! private staging file only after its received ranges and full digest agree.

use std::{
    error::Error,
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    process::ExitCode,
    time::Duration,
};

use iroh::{
    Endpoint, EndpointAddr, PublicKey, SecretKey, Signature, TransportAddr,
    endpoint::{RecvStream, SendStream},
};
use jumpstart_domain::{
    BlobId, ClientInstanceId, Counter, DeviceId, ItemId, Owner, ServerId, VersionId,
};
use jumpstart_protocol::Hello;
use jumpstart_server::remote_mass::{
    ALPN, Call, CatalogItems, CatalogPage, Create, CreateContent, DEADLINE, Get, Greeting, List,
    Outcome, Range, RangeReply, Reply, capability, create_capability, read, write,
};
use jumpstart_storage::mass::{MASS_CHUNK, MAX_MASS_IMPORT, MassFile, MassItem};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{runtime::Builder, time::timeout};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const MAX_INPUT: u64 = 32 * 1024;
type Failure = Box<dyn Error + Send + Sync>;

/// One private request from the native app or extension.
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Generate,
    Enroll {
        profile: Profile,
        invitation: Invitation,
        name: String,
    },
    Probe {
        profile: Profile,
    },
    List {
        profile: Profile,
        parent: Option<ItemId>,
        after: Option<ItemId>,
    },
    Item {
        profile: Profile,
        id: ItemId,
    },
    Create {
        profile: Profile,
        request_id: Uuid,
        parent: ItemId,
        name: String,
        input: Option<PathBuf>,
        sha256: Option<String>,
    },
    Fetch {
        profile: Profile,
        id: ItemId,
        version: VersionId,
        output: PathBuf,
    },
}

/// The independently paired installation and the server identity it pinned.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    address: SocketAddr,
    server_key: PublicKey,
    server_id: ServerId,
    instance_id: ClientInstanceId,
    seed: [u8; 32],
}

impl Drop for Profile {
    /// Clear the serialized secret copy after the connection is finished.
    fn drop(&mut self) {
        self.seed.zeroize();
    }
}

/// One invitation from the server's temporary pairing command.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invitation {
    pairing_id: Uuid,
    server_id: ServerId,
    server_key: PublicKey,
    lan_endpoint: SocketAddr,
    token: [u8; 16],
}

impl Drop for Invitation {
    /// Clear the short-lived one-time token after enrollment.
    fn drop(&mut self) {
        self.token.zeroize();
    }
}

/// Validated metadata from the signed pairing receipt.
#[derive(Deserialize)]
struct Grant {
    format_version: u16,
    server_id: ServerId,
    server_key: PublicKey,
    device_id: DeviceId,
    public_key: [u8; 32],
    instance_id: ClientInstanceId,
    review: Value,
}

/// Retains the pinned endpoint and both sides of the sequential protocol stream.
struct Session {
    _endpoint: Endpoint,
    _connection: iroh::endpoint::Connection,
    output: SendStream,
    input: RecvStream,
    can_create: bool,
}

/// A destination owned by this helper until complete verification and sync.
struct Staging {
    path: PathBuf,
    file: File,
    complete: bool,
}

impl Drop for Staging {
    /// Remove incomplete staging bytes after a normal failure.
    fn drop(&mut self) {
        if !self.complete {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Read the private input and emit a final structured success or error.
pub(super) fn run() -> ExitCode {
    let result = (|| -> Result<(), Failure> {
        let mut bytes = Zeroizing::new(Vec::new());
        std::io::stdin()
            .take(MAX_INPUT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_INPUT {
            return Err("the private Mass helper request is too large".into());
        }
        let command = jumpstart_protocol::decode_body::<Command>(&bytes)
            .map_err(|_| "the private Mass helper request is invalid")?;
        drop(bytes);
        let value = Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(execute(command))?;
        emit(json!({"status": "ok", "value": value}))?;
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = emit(json!({"status": "error", "message": error.to_string()}));
            ExitCode::FAILURE
        }
    }
}

/// Execute a single request and return its final payload.
async fn execute(command: Command) -> Result<Value, Failure> {
    match command {
        Command::Generate => {
            let secret = SecretKey::generate();
            Ok(json!({"seed": secret.to_bytes(), "instance_id": ClientInstanceId::new()}))
        }
        Command::Enroll {
            profile,
            invitation,
            name,
        } => enroll(&profile, &invitation, name).await,
        Command::Probe { profile } => {
            let _ = Session::open(&profile).await?;
            Ok(json!({"connected": true}))
        }
        Command::List {
            profile,
            parent,
            after,
        } => {
            let mut session = Session::open(&profile).await?;
            let page: CatalogPage = session
                .call(Call::List {
                    request_id: Uuid::new_v4(),
                    schema_version: 1,
                    body: List {
                        owner: Owner::Mass,
                        parent,
                        page_token: after,
                    },
                })
                .await?;
            Ok(serde_json::to_value(page)?)
        }
        Command::Item { profile, id } => {
            let mut session = Session::open(&profile).await?;
            Ok(serde_json::to_value(session.item(id).await?)?)
        }
        Command::Create {
            profile,
            request_id,
            parent,
            name,
            input,
            sha256,
        } => {
            let mut session = Session::open(&profile).await?;
            Ok(serde_json::to_value(
                session
                    .create(request_id, parent, name, input, sha256)
                    .await?,
            )?)
        }
        Command::Fetch {
            profile,
            id,
            version,
            output,
        } => {
            let mut session = Session::open(&profile).await?;
            let item = session
                .item(id)
                .await?
                .ok_or("the selected Mass item is unavailable")?;
            let file = item
                .file
                .as_ref()
                .filter(|file| file.version_id == version)
                .ok_or("the selected Mass version is unavailable")?;
            materialize(&mut session, id, file, output).await?;
            Ok(serde_json::to_value(item)?)
        }
    }
}

impl Session {
    /// Dial the pinned server and negotiate the Mass read capability.
    async fn open(profile: &Profile) -> Result<Self, Failure> {
        timeout(DEADLINE, async {
            let endpoint = endpoint(profile).await?;
            let connection = endpoint
                .connect(target(profile.server_key, profile.address), ALPN)
                .await?;
            let (mut output, mut input) = connection.open_bi().await?;
            let feature = capability();
            let hello = Hello::new(
                env!("CARGO_PKG_VERSION"),
                profile.instance_id,
                vec![feature.clone(), create_capability()],
                vec![feature],
            )?;
            write(&mut output, &hello).await?;
            let can_create = match read::<Greeting>(&mut input).await? {
                Greeting::Ready { welcome } => {
                    welcome.validate_for(&hello)?;
                    if welcome.server_id != profile.server_id {
                        return Err(
                            "the Mass server identity changed; pair again explicitly".into()
                        );
                    }
                    welcome.capabilities.contains(&create_capability())
                }
                Greeting::Error { error } => return Err(error.message.into()),
            };
            Ok(Self {
                _endpoint: endpoint,
                _connection: connection,
                output,
                input,
                can_create,
            })
        })
        .await?
    }

    /// Send one request and validate its matching response before using it.
    async fn call<T: DeserializeOwned>(&mut self, request: Call) -> Result<T, Failure> {
        self.call_with_bytes(request, &[]).await
    }

    /// Send an optional exact raw body after its JSON control frame.
    async fn call_with_bytes<T: DeserializeOwned>(
        &mut self,
        request: Call,
        bytes: &[u8],
    ) -> Result<T, Failure> {
        timeout(DEADLINE, async {
            let (id, _) = request.envelope();
            write(&mut self.output, &request).await?;
            self.output.write_all(bytes).await?;
            let reply: Reply<T> = read(&mut self.input).await?;
            if reply.request_id != id {
                return Err("the Mass server replied to a different request".into());
            }
            match reply.result {
                Outcome::Ok { value } => Ok(value),
                Outcome::Error { error } => Err(error.message.into()),
            }
        })
        .await?
    }

    /// Send one sealed local create and verify the publication receipt.
    async fn create(
        &mut self,
        request_id: Uuid,
        parent: ItemId,
        name: String,
        input: Option<PathBuf>,
        sha256: Option<String>,
    ) -> Result<MassItem, Failure> {
        if !self.can_create {
            return Err("this server has no paired Mass create support".into());
        }
        let mut bytes = Vec::new();
        let content = if let Some(input) = input {
            let file = File::open(input)?;
            if !file.metadata()?.is_file() {
                return Err("the pending Mass source is not a regular file".into());
            }
            file.take(MAX_MASS_IMPORT as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > MAX_MASS_IMPORT {
                return Err("the pending Mass file exceeds 32 MiB".into());
            }
            let actual = format!("{:x}", Sha256::digest(&bytes));
            if sha256.as_deref() != Some(actual.as_str()) {
                return Err("the pending Mass file no longer matches its sealed digest".into());
            }
            Some((
                Counter::from_u64(bytes.len() as u64)?,
                BlobId::hash_bytes(&bytes),
            ))
        } else {
            if sha256.is_some() {
                return Err("the pending Mass source is missing".into());
            }
            None
        };
        let item: MassItem = self
            .call_with_bytes(
                Call::Create {
                    request_id,
                    schema_version: 1,
                    body: Create {
                        parent,
                        name: name.clone(),
                        content: content.map(|(length, blob_id)| CreateContent { length, blob_id }),
                    },
                },
                &bytes,
            )
            .await?;
        if item.parent != Some(parent)
            || item.name != name
            || item
                .file
                .as_ref()
                .map(|file| (file.byte_length, file.blob_id))
                != content
        {
            return Err("the Mass create receipt did not match the pending save".into());
        }
        Ok(item)
    }

    /// Retrieve the exact item by stable ID or report its absence.
    async fn item(&mut self, id: ItemId) -> Result<Option<MassItem>, Failure> {
        let mut response: CatalogItems = self
            .call(Call::Get {
                request_id: Uuid::new_v4(),
                schema_version: 1,
                body: Get { ids: vec![id] },
            })
            .await?;
        if response.entities.len() == 1
            && response.entities[0].id == id
            && response.missing.is_empty()
        {
            Ok(response.entities.pop())
        } else if response.entities.is_empty() && response.missing == [id] {
            Ok(None)
        } else {
            Err("the Mass server returned an unexpected item identity".into())
        }
    }

    /// Read one verified range from the pinned version with exact raw framing.
    async fn chunk(
        &mut self,
        id: ItemId,
        file: &MassFile,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, Failure> {
        let offset = Counter::from_u64(offset)?;
        let length = Counter::from_u64(length)?;
        let report: RangeReply = self
            .call(Call::Range {
                request_id: Uuid::new_v4(),
                schema_version: 1,
                body: Range {
                    item_id: id,
                    version_id: file.version_id,
                    offset,
                    length,
                },
            })
            .await?;
        if report.blob_id != file.blob_id
            || report.accepted_range.offset != offset
            || report.accepted_range.length != length
        {
            return Err("the Mass server changed the requested version or range".into());
        }
        let mut bytes = vec![0; length.value() as usize];
        timeout(DEADLINE, self.input.read_exact(&mut bytes)).await??;
        if BlobId::hash_bytes(&bytes) != report.range_hash {
            return Err("the Mass range did not pass verification".into());
        }
        Ok(bytes)
    }
}

/// Materialize a complete verified file in the File Provider staging directory.
async fn materialize(
    session: &mut Session,
    id: ItemId,
    file: &MassFile,
    output: PathBuf,
) -> Result<(), Failure> {
    let length = file.byte_length.value();
    if length > MAX_MASS_IMPORT as u64 {
        return Err("this file exceeds the Finder preview's materialization limit".into());
    }
    let mut stage = Staging {
        file: OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&output)?,
        path: output,
        complete: false,
    };
    let mut hash = blake3::Hasher::new();
    let mut offset = 0;
    while offset < length {
        let bytes = session
            .chunk(id, file, offset, (length - offset).min(MASS_CHUNK as u64))
            .await?;
        hash.update(&bytes);
        stage.file.write_all(&bytes)?;
        offset += bytes.len() as u64;
    }
    if BlobId::from_bytes(*hash.finalize().as_bytes()) != file.blob_id {
        return Err("the completed Mass file did not pass verification".into());
    }
    stage.file.sync_all()?;
    stage.complete = true;
    Ok(())
}

/// Present the comparison phrase, then verify the exact signed grant from the server.
async fn enroll(
    profile: &Profile,
    invitation: &Invitation,
    name: String,
) -> Result<Value, Failure> {
    if profile.server_id != invitation.server_id || profile.server_key != invitation.server_key {
        return Err("the invitation is for a different server".into());
    }
    let (endpoint, connection) = timeout(DEADLINE, async {
        let endpoint = endpoint(profile).await?;
        let connection = endpoint
            .connect(
                target(invitation.server_key, invitation.lan_endpoint),
                b"jumpstart-pair/1",
            )
            .await?;
        Ok::<_, Failure>((endpoint, connection))
    })
    .await??;
    let (mut output, mut input) = timeout(DEADLINE, connection.open_bi()).await??;
    let nonce = Uuid::new_v4().into_bytes();
    timeout(DEADLINE, write(&mut output, &json!({
        "method": "submit", "pairing_id": invitation.pairing_id,
        "token": invitation.token, "client_nonce": nonce,
        "instance_id": profile.instance_id, "app": Owner::Mass, "platform": "macos", "name": name,
    }))).await??;
    output.finish()?;
    let pending: Value = timeout(DEADLINE, read(&mut input)).await??;
    if pending["status"] != "pending" {
        return Err("the pairing invitation was refused or expired".into());
    }
    emit(json!({"status": "pending", "value": &pending["request"]}))?;
    let approved: Value = timeout(Duration::from_secs(300), read(&mut input)).await??;
    if approved["status"] != "approved" {
        return Err("the server did not approve this Mass installation".into());
    }
    let receipt = &approved["receipt"];
    let payload = receipt["payload"]
        .as_str()
        .ok_or("the pairing receipt has no payload")?;
    let signature: Signature = serde_json::from_value(receipt["signature"].clone())?;
    let mut signed = b"jumpstart pairing receipt v1\0".to_vec();
    signed.extend_from_slice(payload.as_bytes());
    profile.server_key.verify(&signed, &signature)?;
    let grant: Grant = jumpstart_protocol::decode_body(payload.as_bytes())?;
    let local_key = SecretKey::from_bytes(&profile.seed).public();
    if grant.format_version != 1
        || grant.server_id != profile.server_id
        || grant.server_key != profile.server_key
        || grant.public_key != *local_key.as_bytes()
        || grant.instance_id != profile.instance_id
        || grant.review != pending["request"]
    {
        return Err("the signed grant does not match the displayed request".into());
    }
    drop(connection);
    drop(endpoint);
    Ok(json!({"device_id": grant.device_id}))
}

/// Create an iroh endpoint with TLS and no ambient relay or discovery.
async fn endpoint(profile: &Profile) -> Result<Endpoint, Failure> {
    Ok(Endpoint::builder(iroh::endpoint::presets::Minimal)
        .secret_key(SecretKey::from_bytes(&profile.seed))
        .bind()
        .await?)
}

/// Pair the configured direct address with its pinned public key.
fn target(key: PublicKey, address: SocketAddr) -> EndpointAddr {
    EndpointAddr::from_parts(key, [TransportAddr::Ip(address)])
}

/// Send a newline-delimited helper event and flush it for the Mac app.
fn emit(value: impl Serialize) -> std::io::Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &value)?;
    writeln!(output)?;
    output.flush()
}

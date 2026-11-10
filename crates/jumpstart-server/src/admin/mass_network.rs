//! Serves Mass catalog reads and create-only writes over authenticated QUIC.
//!
//! Network tasks carry no database connection. They send requests to the
//! daemon's lease owner, which checks authority for every request and closes
//! sessions when a local administrator commits revocation.

use std::{collections::BTreeMap, error::Error, net::SocketAddr, sync::Arc};

use iroh::{Endpoint, SecretKey, endpoint::Connection};
use jumpstart_domain::{BlobId, ClientInstanceId, Counter, DeviceId, ItemId, Owner};
use jumpstart_protocol::{
    ErrorCode, Hello, ProtocolError, ProtocolLimits, ReadRange, ServerHandshake, negotiate,
};
use jumpstart_server::remote_mass::{
    ALPN, AcceptedRange, Call, CatalogItems, CatalogPage, Create, DEADLINE, Greeting, Outcome,
    Range, RangeReply, Reply, capability, create_capability, read, refusal, write,
};
use jumpstart_storage::{
    AdminMetadata, StorageError,
    mass::{MASS_CHUNK, MASS_PAGE, MAX_MASS_IMPORT, MassDestination, MassImport, MassItem},
};
use serde_json::Value;
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    time::timeout,
};
use uuid::Uuid;

const CONNECTIONS: usize = 16;
const CREATE_SLOTS: usize = 2;
type Failure = Box<dyn Error + Send + Sync>;

/// A request accompanied by its kernel-authenticated connection and reply channel.
pub(super) struct Event {
    connection: Connection,
    work: Work,
    reply: oneshot::Sender<Response>,
}

/// The first exchange authenticates an instance. Later exchanges carry that binding.
enum Work {
    Hello(Hello),
    Request {
        instance: ClientInstanceId,
        call: Call,
        inbound: Vec<u8>,
    },
}

/// A typed wire control message and its optional raw data.
struct Response {
    control: Value,
    bytes: Vec<u8>,
    authorized: bool,
}

/// Connections whose authority is owned by the daemon's metadata task.
#[derive(Default)]
pub(super) struct Sessions(BTreeMap<usize, (DeviceId, Connection)>);

impl Sessions {
    /// Authorize and execute one network event without yielding the metadata lease.
    pub(super) fn receive(
        &mut self,
        metadata: &mut Result<AdminMetadata, StorageError>,
        event: Event,
    ) {
        self.0
            .retain(|_, (_, connection)| connection.close_reason().is_none());
        let response = self.respond(metadata, &event.connection, event.work);
        let _ = event.reply.send(response);
    }

    /// Close all transport connections belonging to a durably revoked device.
    pub(super) fn revoke(&mut self, device: DeviceId) {
        self.0.retain(|_, (id, connection)| {
            if *id == device {
                connection.close(1u32.into(), b"device revoked");
                false
            } else {
                true
            }
        });
    }

    /// Check the connection's actual public key and the exact paired instance.
    fn respond(
        &mut self,
        metadata: &mut Result<AdminMetadata, StorageError>,
        connection: &Connection,
        work: Work,
    ) -> Response {
        let instance = match &work {
            Work::Hello(hello) => hello.client_instance_id,
            Work::Request { instance, .. } => *instance,
        };
        let lookup = |lease: &AdminMetadata| {
            lease
                .active_mass_device(connection.remote_id().as_bytes(), instance)
                .map_err(|_| unavailable())?
                .ok_or_else(|| {
                    refusal(
                        ErrorCode::Unauthorized,
                        "Pair this Mass installation before connecting.",
                    )
                })
        };
        let authority = metadata
            .as_mut()
            .map_err(|_| unavailable())
            .and_then(|lease| lookup(lease).map(|device| (lease, device)));
        match work {
            Work::Hello(hello) => {
                let result = authority.and_then(|(lease, device)| {
                    let limits = ProtocolLimits {
                        max_control_requests: count(1),
                        max_bulk_streams: count(1),
                        max_page_records: count(u64::from(MASS_PAGE)),
                        max_range_bytes: count(MASS_CHUNK as u64),
                        ..ProtocolLimits::default()
                    };
                    let report = lease.report();
                    let welcome = negotiate(
                        &hello,
                        &ServerHandshake {
                            server_id: report.server_id,
                            sync_epoch: report.sync_epoch,
                            server_build: env!("CARGO_PKG_VERSION").into(),
                            supported_features: vec![capability(), create_capability()],
                            limits,
                            latest_seq: Counter::ZERO,
                            minimum_seq: Counter::ZERO,
                        },
                    )
                    .map_err(ProtocolError::from)?;
                    self.0
                        .insert(connection.stable_id(), (device, connection.clone()));
                    Ok(welcome)
                });
                let authorized = result.is_ok();
                let greeting = match result {
                    Ok(welcome) => Greeting::Ready { welcome },
                    Err(error) => Greeting::Error { error },
                };
                Response {
                    control: to_value(greeting),
                    bytes: Vec::new(),
                    authorized,
                }
            }
            Work::Request { call, inbound, .. } => {
                let (request_id, _) = call.envelope();
                let authorized = authority.is_ok();
                let result =
                    authority.and_then(|(lease, device)| execute(lease, device, call, &inbound));
                let (result, bytes) = match result {
                    Ok((value, bytes)) => (Outcome::Ok { value }, bytes),
                    Err(error) => (Outcome::Error { error }, Vec::new()),
                };
                Response {
                    control: to_value(Reply { request_id, result }),
                    bytes,
                    authorized,
                }
            }
        }
    }
}

/// Start the application ALPN on the configured address without relay or discovery.
pub(super) async fn listen(
    address: SocketAddr,
    key: SecretKey,
) -> Result<mpsc::Receiver<Event>, Box<dyn Error>> {
    if address.ip().is_unspecified() || address.ip().is_multicast() || address.port() == 0 {
        return Err("select one concrete Mass listen address and port".into());
    }
    let endpoint = Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_ip_transports()
        .bind_addr(address)?
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    let (send, receive) = mpsc::channel(CONNECTIONS);
    tokio::spawn(accept(endpoint, send));
    Ok(receive)
}

/// Return an inactive receiver when the operator leaves the application port disabled.
pub(super) fn disabled() -> mpsc::Receiver<Event> {
    mpsc::channel(1).1
}

/// Admit bounded concurrent connections and refuse excess work before TLS negotiation.
async fn accept(endpoint: Endpoint, send: mpsc::Sender<Event>) {
    let capacity = Arc::new(Semaphore::new(CONNECTIONS));
    let writes = Arc::new(Semaphore::new(CREATE_SLOTS));
    while let Some(incoming) = endpoint.accept().await {
        let Ok(slot) = capacity.clone().try_acquire_owned() else {
            incoming.refuse();
            continue;
        };
        let send = send.clone();
        let writes = writes.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if let Ok(Ok(connection)) = timeout(DEADLINE, incoming).await {
                let _ = exchange(&connection, &send, &writes).await;
                connection.close(0u32.into(), b"Mass session closed");
            }
        });
    }
}

/// Run one sequential bidirectional stream with a deadline at each boundary.
async fn exchange(
    connection: &Connection,
    send: &mpsc::Sender<Event>,
    writes: &Semaphore,
) -> Result<(), Failure> {
    let (mut output, mut input) = timeout(DEADLINE, connection.accept_bi()).await??;
    let hello: Hello = timeout(DEADLINE, read(&mut input)).await??;
    let instance = hello.client_instance_id;
    let create_negotiated = hello.supported_features.contains(&create_capability());
    let welcome = timeout(DEADLINE, dispatch(connection, send, Work::Hello(hello))).await??;
    timeout(DEADLINE, write(&mut output, &welcome.control)).await??;
    if !welcome.authorized {
        output.finish()?;
        let _ = timeout(DEADLINE, output.stopped()).await;
        return Ok(());
    }
    while let Ok(Ok(call)) = timeout(DEADLINE, read::<Call>(&mut input)).await {
        // The greeting has already authenticated this peer. Bound bytes in
        // memory across all connections, then recheck authority at commit.
        let _slot;
        let mut inbound = Vec::new();
        if let Call::Create { body, .. } = &call {
            if !create_negotiated {
                return Err("this Mass session did not negotiate create support".into());
            }
            _slot = timeout(DEADLINE, writes.acquire()).await??;
            if let Some(content) = &body.content {
                let length = content.length.value();
                if length > MAX_MASS_IMPORT as u64 {
                    return Err("the Mass upload exceeds its byte limit".into());
                }
                inbound.resize(length as usize, 0);
                timeout(DEADLINE, input.read_exact(&mut inbound)).await??;
            }
        }
        let response = timeout(
            DEADLINE,
            dispatch(
                connection,
                send,
                Work::Request {
                    instance,
                    call,
                    inbound,
                },
            ),
        )
        .await??;
        timeout(DEADLINE, async {
            write(&mut output, &response.control).await?;
            output.write_all(&response.bytes).await?;
            Ok::<_, Failure>(())
        })
        .await??;
        if !response.authorized {
            break;
        }
    }
    output.finish()?;
    let _ = timeout(DEADLINE, output.stopped()).await;
    Ok(())
}

/// Ask the lease owner to recheck authority before producing data.
async fn dispatch(
    connection: &Connection,
    send: &mpsc::Sender<Event>,
    work: Work,
) -> Result<Response, Failure> {
    let (reply, receive) = oneshot::channel();
    send.send(Event {
        connection: connection.clone(),
        work,
        reply,
    })
    .await
    .map_err(|_| "the Mass writer is shutting down")?;
    Ok(receive.await?)
}

/// Apply each operation on the owner task with Mass-scoped catalog access.
fn execute(
    lease: &mut AdminMetadata,
    device: DeviceId,
    call: Call,
    inbound: &[u8],
) -> Result<(Value, Vec<u8>), ProtocolError> {
    if call.envelope().1 != 1 {
        return Err(refusal(
            ErrorCode::Unsupported,
            "This Mass payload version is unsupported.",
        ));
    }
    match call {
        Call::List { body, .. } => {
            if body.owner != Owner::Mass {
                return Err(refusal(
                    ErrorCode::Unauthorized,
                    "This preview reads only the Mass library.",
                ));
            }
            let page = lease
                .mass_list(body.parent, body.page_token)
                .map_err(|_| unavailable())?;
            Ok((to_value(CatalogPage::from(page)), Vec::new()))
        }
        Call::Get { body, .. } => {
            if body.ids.is_empty() || body.ids.len() > 32 {
                return Err(refusal(
                    ErrorCode::Invalid,
                    "Select between one and 32 items.",
                ));
            }
            let mut reply = CatalogItems {
                entities: Vec::new(),
                missing: Vec::new(),
            };
            for id in body.ids {
                match lease.mass_item(id) {
                    Ok(item) => reply.entities.push(item),
                    Err(StorageError::Mass("the item is unavailable")) => reply.missing.push(id),
                    Err(_) => return Err(unavailable()),
                }
            }
            Ok((to_value(reply), Vec::new()))
        }
        Call::Range { body, .. } => range(lease, body),
        Call::Create {
            request_id, body, ..
        } => {
            let item = create(lease, device, request_id, body, inbound)?;
            Ok((to_value(item), Vec::new()))
        }
    }
}

/// Publish a new Mass child using the parent's verified primary and a device-scoped receipt.
fn create(
    lease: &mut AdminMetadata,
    device: DeviceId,
    request_id: Uuid,
    request: Create,
    inbound: &[u8],
) -> Result<MassItem, ProtocolError> {
    let Create {
        parent,
        name,
        content,
    } = request;
    let primary_id = folder_primary(lease, parent)?;
    let mut receipt = blake3::Hasher::new_derive_key("jumpstart paired Mass create v1");
    receipt.update(device.as_uuid().as_bytes());
    receipt.update(request_id.as_bytes());
    let mut scoped = [0; 16];
    scoped.copy_from_slice(&receipt.finalize().as_bytes()[..16]);
    let destination = MassDestination {
        request_id: Uuid::from_bytes(scoped),
        primary_id,
        parent: Some(parent),
        name,
    };
    let result = if let Some(content) = content {
        lease.mass_import(
            MassImport {
                destination,
                length: content.length,
                blob_id: content.blob_id,
            },
            inbound,
        )
    } else {
        lease.mass_create_folder(destination)
    };
    result.map_err(|error| match error {
        StorageError::Mass("a file or folder with that name already exists") => refusal(
            ErrorCode::Conflict,
            "A Mass item already uses that name. Your local copy is retained.",
        ),
        StorageError::Mass("this request ID was previously used for different inputs") => refusal(
            ErrorCode::IdempotencyMismatch,
            "This pending save has different inputs from its original attempt.",
        ),
        StorageError::Mass("upload length or BLAKE3 digest did not verify") => refusal(
            ErrorCode::Invalid,
            "The uploaded bytes did not match their manifest.",
        ),
        _ => refusal(
            ErrorCode::Unavailable,
            "Mass could not publish the new item. Your local copy is retained.",
        ),
    })
}

/// Resolve destinations through the same Mass-only catalog used by reads.
fn folder_primary(lease: &AdminMetadata, parent: ItemId) -> Result<Uuid, ProtocolError> {
    lease
        .mass_item(parent)
        .ok()
        .filter(|item| item.file.is_none())
        .map(|item| item.primary_id)
        .ok_or_else(|| {
            refusal(
                ErrorCode::NotFound,
                "The destination Mass folder is unavailable.",
            )
        })
}

/// Verify every touched chunk before returning an exact raw range.
fn range(lease: &AdminMetadata, request: Range) -> Result<(Value, Vec<u8>), ProtocolError> {
    let item = lease
        .mass_item(request.item_id)
        .map_err(|_| unavailable())?;
    let file = item
        .file
        .filter(|file| file.version_id == request.version_id)
        .ok_or_else(|| {
            refusal(
                ErrorCode::NotFound,
                "The selected file version is unavailable.",
            )
        })?;
    let range = ReadRange::new(request.offset, request.length, file.byte_length).map_err(|_| {
        refusal(
            ErrorCode::Invalid,
            "The selected range is outside the file.",
        )
    })?;
    if range.length().value() > MASS_CHUNK as u64 {
        return Err(refusal(
            ErrorCode::Invalid,
            "Request at most one MiB per range.",
        ));
    }
    let mut bytes = Vec::with_capacity(range.length().value() as usize);
    let mut offset = range.offset().value();
    while offset < range.end().value() {
        let index = u32::try_from(offset / MASS_CHUNK as u64).map_err(|_| unavailable())?;
        let local = (offset % MASS_CHUNK as u64) as usize;
        let size = (range.end().value() - offset).min((MASS_CHUNK - local) as u64) as usize;
        let chunk = lease
            .mass_read(item.id, file.version_id, index)
            .map_err(|_| unavailable())?;
        bytes.extend_from_slice(chunk.get(local..local + size).ok_or_else(unavailable)?);
        offset += size as u64;
    }
    let report = RangeReply {
        read_id: Uuid::new_v4(),
        accepted_range: AcceptedRange {
            offset: range.offset(),
            length: range.length(),
        },
        blob_id: file.blob_id,
        range_hash: BlobId::hash_bytes(&bytes),
    };
    Ok((to_value(report), bytes))
}

/// Keep remote errors actionable without disclosing the host's filesystem.
fn unavailable() -> ProtocolError {
    refusal(
        ErrorCode::Unavailable,
        "The Mass catalog or verified file is unavailable.",
    )
}

/// Encode an internally validated response without accepting arbitrary map keys.
fn to_value(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("Mass wire values have infallible JSON representations")
}

/// Convert a fixed protocol limit into its exact wire counter.
fn count(value: u64) -> Counter {
    Counter::from_u64(value).expect("Mass limits fit a protocol counter")
}

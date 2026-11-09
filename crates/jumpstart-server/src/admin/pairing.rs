//! Runs the short-lived, direct-only pairing service and local approval flow.
//!
//! The network worker proves the peer's endpoint key and physical LAN route.
//! The local daemon owns the invitation, the operator decision and SQLite write,
//! so a LAN client can never approve its own request.

mod lan;

use std::{
    error::Error,
    fs::{File, OpenOptions},
    io::Read,
    net::{SocketAddr, SocketAddrV4},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, SecretKey,
    endpoint::{Connection, Incoming, IncomingAddr, SendStream},
};
use jumpstart_domain::{ClientInstanceId, DeviceId, Owner, ServerId};
use jumpstart_protocol::{
    decode_body,
    pairing::{PAIRING_LIFETIME, PairingObservation, PairingWindow},
};
use jumpstart_server::local_admin::write;
use jumpstart_storage::{
    AdminMetadata, DeviceRecord, DeviceRegistration, StorageError, device_fingerprint,
};
use rustix::{fs::OFlags, process::geteuid};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{mpsc, oneshot},
    time::{sleep, timeout},
};
use uuid::Uuid;

use self::lan::{LanLink, LanTransport};

const ALPN: &[u8] = b"jumpstart-pair/1";
const MAX_REQUEST: usize = 4096;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(5);
const CREDENTIAL_NAME: &str = "jumpstart-iroh.key";

/// The only requests accepted from the pairing ALPN.
#[derive(Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
enum NetworkRequest {
    Submit {
        pairing_id: Uuid,
        token: [u8; 16],
        client_nonce: [u8; 16],
        instance_id: ClientInstanceId,
        app: Owner,
        platform: String,
        name: String,
    },
    Receipt,
}

/// A request bound to its completed QUIC handshake and observed LAN source.
pub(super) struct NetworkEvent {
    window_id: Uuid,
    peer: SocketAddrV4,
    request: NetworkRequest,
    connection: Connection,
    send: SendStream,
}

/// The precise pending request the client and operator compare.
#[derive(Clone, Serialize)]
struct Review {
    request_id: Uuid,
    transcript: String,
    phrase: String,
    fingerprint: String,
    address: SocketAddr,
    platform: String,
    name: String,
    app: Owner,
}

/// Keeps the authenticated stream available for the signed approval reply.
struct Waiting {
    review: Review,
    digest: blake3::Hash,
    peer: SocketAddrV4,
    connection: Connection,
    send: SendStream,
}

/// The daemon's ephemeral pairing state. Dropping the stop signal closes the ALPN.
struct Active {
    id: Uuid,
    link: LanLink,
    key: SecretKey,
    window: PairingWindow,
    waiting: Option<Waiting>,
    _stop: oneshot::Sender<()>,
}

/// Serializes LAN events and operator decisions at the metadata writer boundary.
pub(super) struct PairingAdmin {
    active: Option<Active>,
    send: mpsc::Sender<NetworkEvent>,
}

impl PairingAdmin {
    /// Create the daemon-owned pairing service and its bounded network mailbox.
    pub(super) fn new() -> (Self, mpsc::Receiver<NetworkEvent>) {
        let (send, receive) = mpsc::channel(8);
        (Self { active: None, send }, receive)
    }

    /// Start one direct-only pairing window and return the one-time invitation.
    pub(super) async fn create(
        &mut self,
        lease: &AdminMetadata,
        interface: String,
        address: SocketAddrV4,
    ) -> Result<Value, Box<dyn Error>> {
        lease.list_devices(None, 1)?;
        if lease.maintenance()? == Some(true) {
            return Err("maintenance is enabled".into());
        }
        self.expire();
        if self.active.is_some() {
            return Err("a pairing window is already open".into());
        }
        let link = LanLink::inspect(interface, address).await?;
        let key = read_credential()?;
        let endpoint = bind_listener(&link, key.clone()).await?;
        let (window, invitation) = PairingWindow::open(
            Instant::now(),
            lease.report().server_id,
            *key.public().as_bytes(),
            link.address().into(),
        )?;
        let (stop, cancelled) = oneshot::channel();
        let id = invitation.pairing_id;
        tokio::spawn(network(
            endpoint,
            link.clone(),
            id,
            self.send.clone(),
            cancelled,
        ));
        self.active = Some(Active {
            id,
            link,
            key,
            window,
            waiting: None,
            _stop: stop,
        });
        Ok(json!({
            "scope": "pairing_invitation",
            "pairing_id": id,
            "server_id": invitation.server_id,
            "server_key": iroh::PublicKey::from_bytes(&invitation.server_key)?.to_string(),
            "lan_endpoint": invitation.lan_endpoint,
            "token": invitation.token,
            "expires_in_seconds": PAIRING_LIFETIME.as_secs(),
        }))
    }

    /// Show the pending transcript, short fingerprint and six words for comparison.
    pub(super) fn pending(&mut self) -> Value {
        self.expire();
        let pending = self
            .active
            .as_ref()
            .and_then(|active| active.waiting.as_ref());
        json!({"scope": "pairing_pending", "request": pending.map(|waiting| &waiting.review)})
    }

    /// Compare the reviewed request, then persist a signed grant or reject it.
    pub(super) async fn decide(
        &mut self,
        lease: &mut AdminMetadata,
        request_id: Uuid,
        transcript: &str,
        approve: bool,
    ) -> Result<Value, Box<dyn Error>> {
        self.expire();
        let active = self.active.as_mut().ok_or("no active pairing window")?;
        let waiting = active
            .waiting
            .as_ref()
            .ok_or("no pending pairing request")?;
        if waiting.review.request_id != request_id || waiting.review.transcript != transcript {
            return Err("the request ID or transcript differs from the reviewed pairing".into());
        }
        let digest = waiting.digest;
        if !approve {
            active.window.reject(Instant::now(), digest)?;
            if let Some(waiting) = active.waiting.take() {
                finish(
                    waiting.connection,
                    waiting.send,
                    json!({"status": "rejected"}),
                );
            }
            return Ok(json!({"scope": "pairing_rejection", "request_id": request_id}));
        }
        if lease.maintenance()? == Some(true) {
            return Err("maintenance is enabled".into());
        }
        if waiting.connection.close_reason().is_some()
            || !active.link.peer_is_on_link(waiting.peer).await
            || !connection_matches(&waiting.connection, waiting.peer)
        {
            return Err("the original LAN connection is no longer available".into());
        }
        let key = *waiting.connection.remote_id().as_bytes();
        let known = lease.device_key_known(&key)?;
        let grant = active.window.approve(Instant::now(), digest, |_| known)?;
        let waiting = active.waiting.take().ok_or("no pending pairing stream")?;
        let device_id = DeviceId::new();
        let observation = grant.observation;
        let receipt = sign_grant(
            &active.key,
            lease.report().server_id,
            device_id,
            &waiting.review,
            &observation,
        )?;
        let result = lease.register_device(DeviceRegistration {
            device_id,
            public_key: observation.public_key,
            name: observation.name,
            platform: observation.platform,
            app: observation.app,
            instance_id: observation.instance,
            signed_receipt: serde_json::to_vec(&receipt)?,
        });
        match result {
            Ok(device) => {
                finish(
                    waiting.connection,
                    waiting.send,
                    json!({"status": "approved", "receipt": receipt}),
                );
                Ok(approved_report(request_id, device))
            }
            Err(error) => {
                finish(
                    waiting.connection,
                    waiting.send,
                    json!({"status": "unavailable"}),
                );
                Err(error.into())
            }
        }
    }

    /// Handle one network event after rechecking the local window and authority.
    pub(super) async fn receive(
        &mut self,
        metadata: &Result<AdminMetadata, StorageError>,
        event: NetworkEvent,
    ) {
        self.expire();
        let _ = self.handle(metadata, event).await;
        self.expire();
    }

    /// Advance a submission or return a receipt scoped to its authenticated key.
    async fn handle(
        &mut self,
        metadata: &Result<AdminMetadata, StorageError>,
        event: NetworkEvent,
    ) -> Result<(), Box<dyn Error>> {
        let active = self
            .active
            .as_mut()
            .filter(|active| active.id == event.window_id)
            .ok_or("pairing window is closed")?;
        let lease = metadata.as_ref().map_err(ToString::to_string)?;
        let NetworkEvent {
            peer,
            request,
            connection,
            mut send,
            ..
        } = event;
        let public_key = *connection.remote_id().as_bytes();
        match request {
            NetworkRequest::Receipt => {
                let response = match lease.device_receipt(&public_key)? {
                    Some(receipt) => {
                        json!({"status": "approved", "receipt": serde_json::from_slice::<Value>(&receipt)?})
                    }
                    None => json!({"status": "unavailable"}),
                };
                finish(connection, send, response);
            }
            NetworkRequest::Submit {
                pairing_id,
                token,
                client_nonce,
                instance_id,
                app,
                platform,
                name,
            } => {
                if pairing_id != active.id
                    || !valid_label(&platform, 32)
                    || !valid_label(&name, 100)
                    || lease.maintenance()? == Some(true)
                {
                    finish(connection, send, json!({"status": "unavailable"}));
                    return Ok(());
                }
                let known = lease.device_key_known(&public_key)?;
                let result = active.window.submit(
                    Instant::now(),
                    token,
                    PairingObservation {
                        public_key,
                        remote: peer.into(),
                        client_nonce,
                        instance: instance_id,
                        app,
                        platform,
                        name,
                    },
                    |_| known,
                );
                let pending = match result {
                    Ok(pending) => pending,
                    Err(_) => {
                        finish(connection, send, json!({"status": "unavailable"}));
                        return Ok(());
                    }
                };
                let observation = &pending.observation;
                let review = Review {
                    request_id: pending.request_id,
                    transcript: pending.transcript.to_hex().to_string(),
                    phrase: phrase(pending.transcript),
                    fingerprint: device_fingerprint(&observation.public_key),
                    address: observation.remote,
                    platform: observation.platform.clone(),
                    name: observation.name.clone(),
                    app: observation.app,
                };
                let digest = pending.transcript;
                let reply = json!({"status": "pending", "request": &review});
                timeout(EXCHANGE_TIMEOUT, write(&mut send, &reply)).await??;
                active.waiting = Some(Waiting {
                    review,
                    digest,
                    peer,
                    connection,
                    send,
                });
            }
        }
        Ok(())
    }

    /// Drop expired or exhausted windows and tell the network task to close.
    fn expire(&mut self) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| !active.window.is_open(Instant::now()))
        {
            self.active = None;
        }
    }
}

/// Bind one pairing ALPN with TLS crypto and no relay or discovery services.
async fn bind_listener(
    link: &LanLink,
    key: SecretKey,
) -> Result<Endpoint, iroh::endpoint::BindError> {
    Endpoint::builder(iroh::endpoint::presets::Minimal)
        .clear_ip_transports()
        .add_custom_transport(Arc::new(LanTransport(link.clone())))
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
}

/// Listen on this window's only endpoint until it is canceled or expires.
async fn network(
    endpoint: Endpoint,
    link: LanLink,
    id: Uuid,
    events: mpsc::Sender<NetworkEvent>,
    cancelled: oneshot::Receiver<()>,
) {
    tokio::select! {
        _ = cancelled => {}
        _ = sleep(PAIRING_LIFETIME) => {}
        _ = async {
            while let Some(incoming) = endpoint.accept().await {
                if let Ok(Ok(event)) = timeout(EXCHANGE_TIMEOUT, authenticate(incoming, &link, id)).await
                    && events.send(event).await.is_err()
                {
                    break;
                }
            }
        } => {}
    }
    endpoint.close().await;
}

/// Validate a route and iroh handshake before reading any pairing content.
async fn authenticate(
    incoming: Incoming,
    link: &LanLink,
    window_id: Uuid,
) -> Result<NetworkEvent, Box<dyn Error + Send + Sync>> {
    let IncomingAddr::Custom(remote) = incoming.remote_addr() else {
        incoming.ignore();
        return Err("unsupported pairing path".into());
    };
    let Some(peer) = lan::decode(&remote) else {
        incoming.ignore();
        return Err("unrecognized pairing path".into());
    };
    if !link.peer_is_on_link(peer).await {
        incoming.ignore();
        return Err("peer route is outside the selected LAN".into());
    }
    let connection = incoming.await?;
    if connection.alpn() != ALPN {
        return Err("pairing connection negotiated an unexpected protocol".into());
    }
    let (send, mut receive) = connection.accept_bi().await?;
    let mut prefix = [0; 4];
    receive.read_exact(&mut prefix).await?;
    let count = u32::from_be_bytes(prefix) as usize;
    if !(1..=MAX_REQUEST).contains(&count) {
        return Err("invalid pairing request length".into());
    }
    let mut body = vec![0; count];
    receive.read_exact(&mut body).await?;
    let request = decode_body(&body)?;
    if !connection_matches(&connection, peer) {
        return Err("pairing connection did not retain its selected route".into());
    }
    Ok(NetworkEvent {
        window_id,
        peer,
        request,
        connection,
        send,
    })
}

/// Require every observed QUIC path to remain on the original pairing source.
fn connection_matches(connection: &Connection, peer: SocketAddrV4) -> bool {
    let paths = connection.paths();
    let mut observed = paths.iter().peekable();
    observed.peek().is_some() && observed.all(|path| {
        matches!(path.remote_addr(), iroh::TransportAddr::Custom(address) if lan::decode(address) == Some(peer))
    })
}

/// Complete a network reply with a short grace period for QUIC acknowledgment.
fn finish(connection: Connection, mut send: SendStream, reply: Value) {
    tokio::spawn(async move {
        let _ = timeout(EXCHANGE_TIMEOUT, async {
            write(&mut send, &reply).await?;
            send.finish()?;
            send.stopped().await?;
            Ok::<(), Box<dyn Error>>(())
        })
        .await;
        drop(connection);
    });
}

/// Load the server signing key supplied by systemd's encrypted credential mechanism.
fn read_credential() -> Result<SecretKey, Box<dyn Error>> {
    let directory = std::env::var_os("CREDENTIALS_DIRECTORY")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or("the systemd pairing credential is unavailable")?;
    let file: File = OpenOptions::new()
        .read(true)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(directory.join(CREDENTIAL_NAME))?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.mode() & 0o077 != 0
        || ![0, geteuid().as_raw()].contains(&metadata.uid())
        || metadata.len() != 32
    {
        return Err("the systemd pairing credential must be a private 32-byte key".into());
    }
    let mut bytes = [0; 32];
    (&file).read_exact(&mut bytes)?;
    Ok(SecretKey::from_bytes(&bytes))
}

/// Derive six comparison words from the first 66 bits of the bound transcript.
fn phrase(digest: blake3::Hash) -> String {
    let mut prefix = [0; 16];
    prefix[..9].copy_from_slice(&digest.as_bytes()[..9]);
    let bits = u128::from_be_bytes(prefix);
    let words = bip39::Language::English.word_list();
    (0..6)
        .map(|index| words[((bits >> (128 - 11 * (index + 1))) & 2047) as usize])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sign and encode a versioned receipt for this key, transcript and first database.
fn sign_grant(
    key: &SecretKey,
    server_id: ServerId,
    device_id: DeviceId,
    review: &Review,
    observation: &PairingObservation,
) -> Result<Value, serde_json::Error> {
    let body = json!({
        "format_version": 1,
        "server_id": server_id,
        "server_key": key.public().to_string(),
        "device_id": device_id,
        "public_key": observation.public_key,
        "instance_id": observation.instance,
        "review": review,
    });
    let payload = serde_json::to_string(&body)?;
    let mut signed = b"jumpstart pairing receipt v1\0".to_vec();
    signed.extend_from_slice(payload.as_bytes());
    Ok(json!({"payload": payload, "signature": key.sign(&signed)}))
}

/// Format the safe local grant summary without exposing the network receipt.
fn approved_report(request_id: Uuid, device: DeviceRecord) -> Value {
    json!({"scope": "pairing_approval", "request_id": request_id, "device": device})
}

/// Reject unbounded or terminal-control display metadata before it is shown.
fn valid_label(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use iroh::{EndpointAddr, TransportAddr};
    use jumpstart_protocol::FRAME_PREFIX_BYTES;
    use tokio::io::AsyncReadExt;

    use super::*;

    /// Drive standard iroh over the real Linux socket and exercise durable approval.
    #[tokio::test]
    async fn pinned_lan_exchange_requires_exact_review_and_revoked_keys_lose_their_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("metadata");
        let server = jumpstart_storage::initialize(&root).unwrap();
        let mut metadata = Ok(AdminMetadata::open(&root).unwrap());
        let link = LanLink::loopback_fixture();
        let server_key = SecretKey::from_bytes(&[31; 32]);
        let server_endpoint = bind_listener(&link, server_key.clone()).await.unwrap();
        let client_key = SecretKey::from_bytes(&[33; 32]);
        let client = Endpoint::builder(iroh::endpoint::presets::Minimal)
            .clear_ip_transports()
            .bind_addr("127.0.0.2:0")
            .unwrap()
            .secret_key(client_key.clone())
            .bind()
            .await
            .unwrap();
        let (mut admin, mut events) = PairingAdmin::new();
        let (window, invitation) = PairingWindow::open(
            Instant::now(),
            server.server_id,
            *server_key.public().as_bytes(),
            link.address().into(),
        )
        .unwrap();
        let (stop, cancelled) = oneshot::channel();
        tokio::spawn(network(
            server_endpoint,
            link.clone(),
            invitation.pairing_id,
            admin.send.clone(),
            cancelled,
        ));
        admin.active = Some(Active {
            id: invitation.pairing_id,
            link,
            key: server_key.clone(),
            window,
            waiting: None,
            _stop: stop,
        });

        timeout(Duration::from_secs(15), async {
            let target = EndpointAddr::from_parts(
                server_key.public(),
                [TransportAddr::Ip(invitation.lan_endpoint)],
            );
            let connection = client.connect(target.clone(), ALPN).await.unwrap();
            let (mut send, mut receive) = connection.open_bi().await.unwrap();
            let instance = ClientInstanceId::new();
            write(
                &mut send,
                &json!({
                    "method": "submit",
                    "pairing_id": invitation.pairing_id,
                    "token": invitation.token,
                    "client_nonce": ([9_u8; 16]),
                    "instance_id": instance,
                    "app": Owner::Mass,
                    "platform": "linux",
                    "name": "Mass test client",
                }),
            )
            .await
            .unwrap();
            send.finish().unwrap();
            admin.receive(&metadata, events.recv().await.unwrap()).await;
            let pending = read_value(&mut receive).await;
            assert_eq!(pending["status"], "pending");
            let request = &admin.pending()["request"];
            assert_eq!(request, &pending["request"]);
            assert_eq!(
                request["phrase"]
                    .as_str()
                    .unwrap()
                    .split_whitespace()
                    .count(),
                6
            );
            let request_id = serde_json::from_value(request["request_id"].clone()).unwrap();
            let digest = request["transcript"].as_str().unwrap();
            let lease = metadata.as_mut().unwrap();
            assert!(
                admin
                    .decide(lease, request_id, "different", true)
                    .await
                    .is_err()
            );
            assert!(lease.list_devices(None, 1).unwrap().devices.is_empty());
            let report = admin.decide(lease, request_id, digest, true).await.unwrap();
            let id: DeviceId =
                serde_json::from_value(report["device"]["device_id"].clone()).unwrap();
            let result = read_value(&mut receive).await;
            assert_eq!(result["status"], "approved");
            let receipt = &result["receipt"];
            let signature: iroh::Signature =
                serde_json::from_value(receipt["signature"].clone()).unwrap();
            let mut bytes = b"jumpstart pairing receipt v1\0".to_vec();
            bytes.extend_from_slice(receipt["payload"].as_str().unwrap().as_bytes());
            server_key.public().verify(&bytes, &signature).unwrap();
            let key = *client_key.public().as_bytes();
            assert!(lease.device_receipt(&key).unwrap().is_some());
            lease.revoke_device(id).unwrap();
            assert!(lease.device_receipt(&key).unwrap().is_none());

            let retry = client.connect(target, ALPN).await.unwrap();
            let (mut send, mut receive) = retry.open_bi().await.unwrap();
            write(&mut send, &json!({"method": "receipt"}))
                .await
                .unwrap();
            send.finish().unwrap();
            admin.receive(&metadata, events.recv().await.unwrap()).await;
            let refusal = read_value(&mut receive).await;
            assert_eq!(refusal["status"], "unavailable");
            assert!(refusal.get("receipt").is_none());
        })
        .await
        .expect("bounded pairing exchange");
        client.close().await;
    }

    /// Read one framed JSON response from the fixture's authenticated peer.
    async fn read_value(receive: &mut (impl tokio::io::AsyncRead + Unpin)) -> Value {
        let mut prefix = [0; FRAME_PREFIX_BYTES];
        receive.read_exact(&mut prefix).await.unwrap();
        let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
        receive.read_exact(&mut body).await.unwrap();
        decode_body(&body).unwrap()
    }
}

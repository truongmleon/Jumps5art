//! Serves Mass's browser UI through a private loopback bridge.
//!
//! The random session capability grants the browser access to the service
//! user's Unix socket. HTTP stays on loopback and never talks to SQLite.

use std::{error::Error, path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use jumpstart_domain::{BlobId, ItemId};
use jumpstart_server::mass_client::MassClient;
use jumpstart_storage::mass::{
    MASS_CHUNK, MAX_MASS_IMPORT, MassDestination, MassItem, MassPage, MassPrimary,
};
use serde::Deserialize;
use tokio::{net::TcpListener, runtime::Builder, sync::Semaphore};
use uuid::Uuid;

const INDEX: &str = include_str!("../../../ui/mass/index.html");
const CSS: &str = include_str!("../../../ui/mass/drive.css");
const JS: &str = include_str!("../../../ui/mass/drive.js");
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// The socket client and access boundary for one local browser session.
struct App {
    client: MassClient,
    host: String,
    origin: String,
    token_hash: blake3::Hash,
    slots: Semaphore,
}

/// The folder or search page requested by the browser.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    parent: Option<ItemId>,
    query: Option<String>,
    after: Option<ItemId>,
}

/// An actionable refusal printed by the browser without interpreting HTML.
struct Problem(StatusCode, String);

impl IntoResponse for Problem {
    /// Encode a route failure as a small JSON message.
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}

/// Verify the real daemon is reachable and start a loopback-only HTTP session.
pub(super) fn run(socket: PathBuf) -> Result<(), Box<dyn Error>> {
    let client = MassClient::new(socket);
    client.primaries()?;
    Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(client))
}

/// Bind a random local port and print the URL with its capability in the fragment.
async fn serve(client: MassClient) -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let host = listener.local_addr()?.to_string();
    let origin = format!("http://{host}");
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let app = Arc::new(App {
        client,
        host,
        origin: origin.clone(),
        token_hash: blake3::hash(token.as_bytes()),
        slots: Semaphore::new(4),
    });
    let routes = Router::new()
        .route(
            "/",
            get(|| async { asset("text/html; charset=utf-8", INDEX) }),
        )
        .route(
            "/drive.css",
            get(|| async { asset("text/css; charset=utf-8", CSS) }),
        )
        .route(
            "/drive.js",
            get(|| async { asset("text/javascript; charset=utf-8", JS) }),
        )
        .route("/api/primaries", get(primaries))
        .route("/api/files", get(files))
        .route("/api/files/{id}", get(item))
        .route("/api/files/{id}/download", get(download))
        .route("/api/folders", post(folder))
        .route(
            "/api/import",
            put(upload).layer(DefaultBodyLimit::max(MAX_MASS_IMPORT)),
        )
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app);
    println!("Open Mass in your browser\n{origin}/#{token}");
    axum::serve(listener, routes)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Validate the loopback origin and session capability before touching the daemon.
async fn guard(State(app): State<Arc<App>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let header_text = |name| headers.get(name).and_then(|value| value.to_str().ok());
    if header_text(header::HOST) != Some(app.host.as_str())
        || header_text(header::ORIGIN).is_some_and(|origin| origin != app.origin)
    {
        return Problem(
            StatusCode::FORBIDDEN,
            "Open the local Mass session URL.".into(),
        )
        .into_response();
    }
    let mut permit = None;
    if request.uri().path().starts_with("/api/") {
        let token =
            header_text(header::AUTHORIZATION).and_then(|text| text.strip_prefix("Bearer "));
        if !token.is_some_and(|token| {
            token.len() == 64 && blake3::hash(token.as_bytes()) == app.token_hash
        }) {
            return Problem(
                StatusCode::UNAUTHORIZED,
                "Open the Mass session link printed by the helper.".into(),
            )
            .into_response();
        }
        let Ok(slot) = app.slots.try_acquire() else {
            return Problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "Mass is busy. Try again shortly.".into(),
            )
            .into_response();
        };
        permit = Some(slot);
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    drop(permit);
    response
}

/// Return an embedded app asset with its explicit content type.
fn asset(mime: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, mime)], body).into_response()
}

/// List the daemon's available registered primary destinations.
async fn primaries(State(app): State<Arc<App>>) -> Result<Json<Vec<MassPrimary>>, Problem> {
    rpc(app, |client| client.primaries()).await.map(Json)
}

/// Load one page of directory entries or filename search results.
async fn files(
    State(app): State<Arc<App>>,
    Query(page): Query<PageQuery>,
) -> Result<Json<MassPage>, Problem> {
    rpc(app, move |client| match page.query {
        Some(query) => client.search(query, page.after),
        None => client.list(page.parent, page.after),
    })
    .await
    .map(Json)
}

/// Return a stable item's current metadata for details and folder navigation.
async fn item(
    State(app): State<Arc<App>>,
    Path(id): Path<ItemId>,
) -> Result<Json<MassItem>, Problem> {
    rpc(app, move |client| client.item(id)).await.map(Json)
}

/// Create an empty folder at the selected destination.
async fn folder(
    State(app): State<Arc<App>>,
    Json(destination): Json<MassDestination>,
) -> Result<Json<MassItem>, Problem> {
    rpc(app, move |client| client.create_folder(destination))
        .await
        .map(Json)
}

/// Forward a bounded upload to the daemon with its stable request ID.
async fn upload(
    State(app): State<Arc<App>>,
    Query(destination): Query<MassDestination>,
    bytes: Bytes,
) -> Result<Json<MassItem>, Problem> {
    rpc(app, move |client| client.import(destination, &bytes))
        .await
        .map(Json)
}

/// Return a complete version only after its exact length and digest agree.
async fn download(
    State(app): State<Arc<App>>,
    Path(id): Path<ItemId>,
) -> Result<Response, Problem> {
    let bytes = rpc(app, move |client| {
        let file = client.item(id)?.file.ok_or("select a file to download")?;
        let length = usize::try_from(file.byte_length.value())?;
        if length > MAX_MASS_IMPORT {
            return Err("this file exceeds the local browser's download limit".into());
        }
        let mut contents = Vec::with_capacity(length);
        for index in 0..length.div_ceil(MASS_CHUNK) {
            let chunk = client.read(id, file.version_id, index as u32)?;
            let expected = (length - contents.len()).min(MASS_CHUNK);
            if chunk.len() != expected {
                return Err("the file changed or returned an incomplete chunk".into());
            }
            contents.extend_from_slice(&chunk);
        }
        if contents.len() != length || BlobId::hash_bytes(&contents) != file.blob_id {
            return Err("the file did not pass download verification".into());
        }
        Ok(contents)
    })
    .await?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CONTENT_DISPOSITION, "attachment"),
        ],
        bytes,
    )
        .into_response())
}

/// Run a checked blocking socket call off the HTTP runtime.
async fn rpc<T: Send + 'static>(
    app: Arc<App>,
    request: impl FnOnce(MassClient) -> Result<T, Box<dyn Error>> + Send + 'static,
) -> Result<T, Problem> {
    let client = app.client.clone();
    tokio::task::spawn_blocking(move || request(client).map_err(|error| error.to_string()))
        .await
        .map_err(|_| {
            Problem(
                StatusCode::SERVICE_UNAVAILABLE,
                "The local Mass helper is unavailable.".into(),
            )
        })?
        .map_err(|error| Problem(StatusCode::CONFLICT, error))
}

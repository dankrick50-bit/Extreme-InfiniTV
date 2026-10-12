// Cast bridge: lets "Play on TV" reach Chromecasts and DLNA smart TVs, which cannot run this app.
//
// Each saved TV is exposed to the sender as if it were one of the app's own receivers: a small
// server on this device (loopback only) answers the receiver protocol and a driver translates it
// to Google Cast or DLNA. The existing picker, pill and remote therefore work unchanged.

mod chromecast;
mod discovery;
mod dlna;
mod session;
mod xml;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::Manager;
use tokio::sync::{broadcast, mpsc, oneshot};

use discovery::FoundDevice;
use session::{Command, PlayRequest, Refresher, SessionHandle, SessionShared, Target, TargetKind};

const STORE_FILE_NAME: &str = "cast-bridge.json";
/// Next to the receiver's own 47815-47819 range, so the two never compete for a port.
const BRIDGE_PORT: u16 = 47830;
const PORT_ATTEMPTS: u16 = 10;
const PROTOCOL_VERSION: u32 = 1;
const AUTH_HEADER: &str = "x-xt-key";
const LOOPBACK: &str = "127.0.0.1";

const MAX_TARGETS: usize = 16;
const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_URL_LEN: usize = 8 * 1024;
const MAX_TITLE_LEN: usize = 512;
/// The sender gives a state request 4s; answer inside that even while a TV is still being probed.
const STATE_READY_WAIT: Duration = Duration::from_millis(2500);
const SOCKET_READY_WAIT: Duration = Duration::from_millis(2000);

// ---------------------------------------------------------------------------
// Persistence: `{app_config_dir}/cast-bridge.json`
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    v: u32,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    targets: Vec<Target>,
}

fn is_valid_key(key: &str) -> bool {
    key.len() == 32 && key.chars().all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
}

fn is_lan_host(host: &str) -> bool {
    host.parse::<Ipv4Addr>().is_ok_and(|address| address.is_private())
}

fn is_valid_target(target: &Target) -> bool {
    if !is_valid_key(&target.key) || target.id.is_empty() || !is_lan_host(&target.host) {
        return false;
    }
    match target.kind {
        TargetKind::Chromecast => true,
        TargetKind::Dlna => target.dlna.is_some(),
    }
}

/// Tolerant: a corrupt or foreign file yields an empty store rather than an error.
fn parse_store(contents: &str) -> StoreFile {
    match serde_json::from_str::<StoreFile>(contents) {
        Ok(mut file) if file.v == 1 => {
            file.targets.retain(is_valid_target);
            file.targets.truncate(MAX_TARGETS);
            file
        }
        _ => StoreFile { v: 1, ..StoreFile::default() },
    }
}

fn load_store(config_dir: &Path) -> StoreFile {
    match std::fs::read_to_string(config_dir.join(STORE_FILE_NAME)) {
        Ok(contents) => parse_store(&contents),
        Err(_) => StoreFile { v: 1, ..StoreFile::default() },
    }
}

/// Temp file + rename: a crash mid-write can't truncate the store.
fn save_store(config_dir: &Path, port: Option<u16>, targets: &[Target]) -> std::io::Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let contents = serde_json::to_string(&StoreFile { v: 1, port, targets: targets.to_vec() })
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temp_path = config_dir.join(format!("{STORE_FILE_NAME}.tmp"));
    std::fs::write(&temp_path, contents)?;
    std::fs::rename(&temp_path, config_dir.join(STORE_FILE_NAME))
}

fn generate_hex(byte_count: usize) -> String {
    use rand::RngCore;
    let mut bytes = vec![0u8; byte_count];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct BridgeInner {
    config_dir: Mutex<Option<PathBuf>>,
    targets: Mutex<Vec<Target>>,
    preferred_port: Mutex<Option<u16>>,
    port: Mutex<Option<u16>>,
    sessions: Mutex<HashMap<String, SessionHandle>>,
    /// The latest scan, so the picker can name a result without sending its details back.
    found: Mutex<Vec<FoundDevice>>,
    /// One scan at a time: a second caller waits and reuses nothing, but never doubles the traffic.
    scan: tokio::sync::Mutex<()>,
}

pub struct BridgeState {
    inner: Arc<BridgeInner>,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            inner: Arc::new(BridgeInner {
                config_dir: Mutex::new(None),
                targets: Mutex::new(Vec::new()),
                preferred_port: Mutex::new(None),
                port: Mutex::new(None),
                sessions: Mutex::new(HashMap::new()),
                found: Mutex::new(Vec::new()),
                scan: tokio::sync::Mutex::new(()),
            }),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (byte_left, byte_right) in left.iter().zip(right.iter()) {
        diff |= byte_left ^ byte_right;
    }
    diff == 0
}

/// What the sender stores for a bridged TV: it talks to the bridge, never to the TV directly.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgedDevice {
    id: String,
    name: String,
    host: String,
    port: u16,
    key: String,
    kind: TargetKind,
    /// The TV's real address, for display only.
    address: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundDeviceSummary {
    found_id: String,
    kind: TargetKind,
    name: String,
    address: String,
    model: Option<String>,
    already_added: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    port: Option<u16>,
    devices: Vec<BridgedDevice>,
}

impl BridgeInner {
    fn persist(&self) {
        let Some(config_dir) = lock(&self.config_dir).clone() else {
            return;
        };
        let bound = *lock(&self.port);
        let port = bound.or(*lock(&self.preferred_port));
        let targets = lock(&self.targets).clone();
        if let Err(error) = save_store(&config_dir, port, &targets) {
            log::warn!("[cast-bridge] failed to save {STORE_FILE_NAME}: {error}");
        }
    }

    fn target_by_key(&self, key: &str) -> Option<Target> {
        lock(&self.targets).iter().find(|target| constant_time_eq(target.key.as_bytes(), key.as_bytes())).cloned()
    }

    fn bridged(&self, target: &Target, port: u16) -> BridgedDevice {
        BridgedDevice {
            id: target.id.clone(),
            name: target.name.clone(),
            host: LOOPBACK.to_string(),
            port,
            key: target.key.clone(),
            kind: target.kind,
            address: target.host.clone(),
        }
    }

    fn status(&self) -> BridgeStatus {
        let port = *lock(&self.port);
        let devices = match port {
            Some(port) => lock(&self.targets).iter().map(|target| self.bridged(target, port)).collect(),
            None => Vec::new(),
        };
        BridgeStatus { port, devices }
    }

    /// Binds synchronously so the port is already listening by the time any page script runs.
    fn ensure_server(self: &Arc<Self>) -> Result<u16, String> {
        let mut bound = lock(&self.port);
        if let Some(port) = *bound {
            return Ok(port);
        }
        let preferred = *lock(&self.preferred_port);
        let candidates = preferred.into_iter().chain((0..PORT_ATTEMPTS).map(|offset| BRIDGE_PORT + offset));
        for candidate in candidates {
            let Ok(listener) = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, candidate)) else {
                continue;
            };
            let port = serve(self.clone(), listener).map_err(|error| format!("OTHER:cast bridge failed to start: {error}"))?;
            *bound = Some(port);
            log::info!("[cast-bridge] listening on {LOOPBACK}:{port}");
            return Ok(port);
        }
        Err("OTHER:no free port for the cast bridge".to_string())
    }

    fn session_for(self: &Arc<Self>, target: &Target) -> SessionHandle {
        let mut sessions = lock(&self.sessions);
        if let Some(existing) = sessions.get(&target.id) {
            return existing.clone();
        }
        let shared = SessionShared::new();
        let (commands, receiver) = mpsc::channel(16);
        let refresher = self.refresher_for(target.id.clone());
        match target.kind {
            TargetKind::Chromecast => {
                tauri::async_runtime::spawn(chromecast::run(
                    target.clone(),
                    shared.clone(),
                    receiver,
                    chromecast::tls_connector(),
                    Some(refresher),
                ));
            }
            TargetKind::Dlna => {
                tauri::async_runtime::spawn(dlna::run(target.clone(), shared.clone(), receiver, Some(refresher)));
            }
        }
        let handle = SessionHandle { shared, commands };
        sessions.insert(target.id.clone(), handle.clone());
        handle
    }

    fn refresher_for(self: &Arc<Self>, id: String) -> Refresher {
        let inner = Arc::downgrade(self);
        Arc::new(move || -> session::BoxFuture<Option<Target>> {
            let inner = inner.clone();
            let id = id.clone();
            Box::pin(async move { inner.upgrade()?.refresh_target(&id).await })
        })
    }

    async fn scan_network(&self) -> Vec<FoundDevice> {
        let _one_at_a_time = self.scan.lock().await;
        let found = discovery::discover(discovery::DISCOVERY_WINDOW).await;
        *lock(&self.found) = found.clone();
        found
    }

    /// Finds a saved TV on the network again and stores where it is now.
    async fn refresh_target(&self, id: &str) -> Option<Target> {
        let current = lock(&self.targets).iter().find(|target| target.id == id).cloned()?;
        let found = self.scan_network().await;
        let hit = found.iter().find(|candidate| discovery::same_device(&current, candidate))?;
        let unchanged = hit.host == current.host && hit.port == current.port && hit.dlna == current.dlna;
        if unchanged {
            return None;
        }
        let updated = Target { host: hit.host.clone(), port: hit.port, dlna: hit.dlna.clone(), ..current };
        if !is_valid_target(&updated) {
            return None;
        }
        if let Some(slot) = lock(&self.targets).iter_mut().find(|target| target.id == id) {
            *slot = updated.clone();
        }
        self.persist();
        Some(updated)
    }

    fn add_target(self: &Arc<Self>, found: &FoundDevice) -> Result<BridgedDevice, String> {
        if !is_lan_host(&found.host) {
            return Err("INVALID:tv is not on the home network".to_string());
        }
        let port = self.ensure_server()?;
        let target = {
            let mut targets = lock(&self.targets);
            let existing = targets.iter().position(|target| {
                target.kind == found.kind && (discovery::same_device(target, found) || (target.host == found.host && target.port == found.port))
            });
            match existing {
                // Adding a TV twice refreshes its details and keeps its key, so the sender's entry stays valid.
                Some(index) => {
                    let target = &mut targets[index];
                    target.name = found.name.clone();
                    target.host = found.host.clone();
                    target.port = found.port;
                    target.uid = found.uid.clone().or(target.uid.take());
                    target.manufacturer = found.manufacturer.clone();
                    target.dlna = found.dlna.clone();
                    target.clone()
                }
                None => {
                    if targets.len() >= MAX_TARGETS {
                        return Err("LIMIT:too many tvs saved".to_string());
                    }
                    let kind = match found.kind {
                        TargetKind::Chromecast => "chromecast",
                        TargetKind::Dlna => "dlna",
                    };
                    let target = Target {
                        id: format!("ext-{kind}-{}", generate_hex(6)),
                        kind: found.kind,
                        name: found.name.clone(),
                        host: found.host.clone(),
                        port: found.port,
                        key: generate_hex(16),
                        uid: found.uid.clone(),
                        manufacturer: found.manufacturer.clone(),
                        dlna: found.dlna.clone(),
                    };
                    targets.push(target.clone());
                    target
                }
            }
        };
        if !is_valid_target(&target) {
            lock(&self.targets).retain(|existing| existing.id != target.id);
            return Err("INVALID:tv details are incomplete".to_string());
        }
        // A driver built for the old details would keep talking to the old address.
        lock(&self.sessions).remove(&target.id);
        self.persist();
        log::info!("[cast-bridge] saved {:?} tv {} at {}", target.kind, target.name, target.host);
        Ok(self.bridged(&target, port))
    }

    /// Drops every saved TV the sender no longer lists, then reports what is left.
    fn sync(self: &Arc<Self>, known_ids: &[String]) -> BridgeStatus {
        let removed: Vec<String> = {
            let mut targets = lock(&self.targets);
            let removed = targets.iter().filter(|target| !known_ids.contains(&target.id)).map(|target| target.id.clone()).collect::<Vec<_>>();
            targets.retain(|target| known_ids.contains(&target.id));
            removed
        };
        if !removed.is_empty() {
            let mut sessions = lock(&self.sessions);
            for id in &removed {
                // Dropping the handle closes the command channel, which ends the driver.
                sessions.remove(id);
            }
            drop(sessions);
            self.persist();
            log::info!("[cast-bridge] forgot {} tv(s)", removed.len());
        }
        if !lock(&self.targets).is_empty() {
            if let Err(error) = self.ensure_server() {
                log::warn!("[cast-bridge] {error}");
            }
        }
        self.status()
    }
}

// ---------------------------------------------------------------------------
// Loopback server speaking the app's receiver protocol
// ---------------------------------------------------------------------------

fn serve(inner: Arc<BridgeInner>, listener: std::net::TcpListener) -> std::io::Result<u16> {
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                log::warn!("[cast-bridge] could not adopt the listener: {error}");
                return;
            }
        };
        if let Err(error) = axum::serve(listener, build_router(inner)).await {
            log::warn!("[cast-bridge] server stopped: {error}");
        }
    });
    Ok(port)
}

fn build_router(inner: Arc<BridgeInner>) -> axum::Router {
    axum::Router::new()
        .route("/info", get(handle_info))
        .route("/play", post(handle_play))
        .route("/ambient", post(handle_ambient))
        .route("/pause", post(handle_pause))
        .route("/resume", post(handle_resume))
        .route("/stop", post(handle_stop))
        .route("/seek", post(handle_seek))
        .route("/volume", post(handle_volume))
        .route("/state", get(handle_state))
        .route("/logs", get(handle_logs))
        .route("/events", get(handle_events))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(inner)
}

fn authenticate(inner: &BridgeInner, headers: &HeaderMap) -> Option<Target> {
    let key = headers.get(AUTH_HEADER)?.to_str().ok()?;
    inner.target_by_key(key)
}

fn unauthorized_response() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response()
}

fn bad_request_response(reason: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": reason}))).into_response()
}

fn ok_response() -> Response {
    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

/// The sender only checks this for reachability; it names the bridge, not any one TV.
async fn handle_info() -> Response {
    Json(json!({"v": PROTOCOL_VERSION, "app": "extreme-infinitv", "name": "Cast bridge", "id": "cast-bridge"})).into_response()
}

fn is_http_url(value: &str) -> bool {
    tauri::Url::parse(value).is_ok_and(|url| url.scheme() == "http" || url.scheme() == "https")
}

async fn handle_play(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap, raw_body: Bytes) -> Response {
    let Some(target) = authenticate(&inner, &headers) else {
        return unauthorized_response();
    };
    let Ok(mut request) = serde_json::from_slice::<PlayRequest>(&raw_body) else {
        return bad_request_response("badRequest");
    };
    if request.v != PROTOCOL_VERSION {
        return bad_request_response("unsupportedVersion");
    }
    // A TV fetches the link itself, so only plain web links can work.
    if request.src.len() > MAX_URL_LEN || !is_http_url(&request.src) {
        return bad_request_response("invalidSrc");
    }
    if request.title.as_deref().is_some_and(|title| title.len() > MAX_TITLE_LEN) {
        return bad_request_response("titleTooLong");
    }
    if request.logo.as_deref().is_some_and(|logo| logo.len() > MAX_URL_LEN || !is_http_url(logo)) {
        request.logo = None;
    }

    let session = inner.session_for(&target);
    // Shown at once, so the sender never reads the previous film's state as this one's.
    session.shared.begin_loading(request.title.clone(), request.known_duration());
    if session.commands.send(Command::Play(Box::new(request))).await.is_err() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "unavailable"}))).into_response();
    }
    ok_response()
}

async fn dispatch(inner: &Arc<BridgeInner>, headers: &HeaderMap, command: Command) -> Response {
    let Some(target) = authenticate(inner, headers) else {
        return unauthorized_response();
    };
    let session = inner.session_for(&target);
    if matches!(command, Command::Stop) {
        session.shared.set_idle();
    }
    let _ = session.commands.send(command).await;
    ok_response()
}

async fn handle_pause(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap) -> Response {
    dispatch(&inner, &headers, Command::Pause).await
}

async fn handle_resume(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap) -> Response {
    dispatch(&inner, &headers, Command::Resume).await
}

async fn handle_stop(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap) -> Response {
    dispatch(&inner, &headers, Command::Stop).await
}

#[derive(Debug, Deserialize)]
struct SeekRequest {
    // Option, not f64: NaN/Infinity arrive as `null`, which must still be rejected.
    seconds: Option<f64>,
}

async fn handle_seek(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap, raw_body: Bytes) -> Response {
    if authenticate(&inner, &headers).is_none() {
        return unauthorized_response();
    }
    let seconds = serde_json::from_slice::<SeekRequest>(&raw_body).ok().and_then(|body| body.seconds);
    let Some(seconds) = seconds.filter(|seconds| seconds.is_finite() && *seconds >= 0.0) else {
        return bad_request_response("invalidSeconds");
    };
    dispatch(&inner, &headers, Command::Seek(seconds)).await
}

#[derive(Debug, Deserialize)]
struct VolumeRequest {
    level: Option<f64>,
    muted: bool,
}

async fn handle_volume(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap, raw_body: Bytes) -> Response {
    if authenticate(&inner, &headers).is_none() {
        return unauthorized_response();
    }
    let Ok(body) = serde_json::from_slice::<VolumeRequest>(&raw_body) else {
        return bad_request_response("badRequest");
    };
    let Some(level) = body.level.filter(|level| level.is_finite() && (0.0..=1.0).contains(level)) else {
        return bad_request_response("invalidLevel");
    };
    dispatch(&inner, &headers, Command::Volume { level, muted: body.muted }).await
}

/// The app's receivers show artwork while idle; a third-party TV has nowhere to put it.
async fn handle_ambient(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap) -> Response {
    if authenticate(&inner, &headers).is_none() {
        return unauthorized_response();
    }
    (StatusCode::OK, Json(json!({"ok": true, "accepted": 0}))).into_response()
}

async fn handle_logs(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap) -> Response {
    if authenticate(&inner, &headers).is_none() {
        return unauthorized_response();
    }
    (StatusCode::OK, [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], String::new()).into_response()
}

async fn handle_state(State(inner): State<Arc<BridgeInner>>, headers: HeaderMap) -> Response {
    let Some(target) = authenticate(&inner, &headers) else {
        return unauthorized_response();
    };
    let session = inner.session_for(&target);
    // Only an idle answer can be wrong about a TV that was already playing when the app started.
    if session.shared.snapshot().state == "idle" {
        session.shared.wait_ready(STATE_READY_WAIT).await;
    }
    // With nothing playing, the bridge itself always answers, even for a TV that is switched off,
    // so the sender's "is it online" check would always pass. Ask the TV before saying yes.
    if session.shared.snapshot().state == "idle" {
        let (done, checked) = oneshot::channel();
        if session.commands.send(Command::Probe(done)).await.is_ok() {
            let _ = tokio::time::timeout(STATE_READY_WAIT, checked).await;
        }
        if !session.shared.is_reachable() {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "unreachable"}))).into_response();
        }
    }
    Json(session.shared.snapshot()).into_response()
}

async fn handle_events(
    State(inner): State<Arc<BridgeInner>>,
    Query(query): Query<HashMap<String, String>>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let key = query.get("key").cloned().unwrap_or_default();
    let Some(target) = inner.target_by_key(&key) else {
        return unauthorized_response();
    };
    let session = inner.session_for(&target);
    upgrade.on_upgrade(move |socket| handle_socket(socket, inner, key, session))
}

async fn handle_socket(mut socket: WebSocket, inner: Arc<BridgeInner>, key: String, session: SessionHandle) {
    let mut updates = session.shared.subscribe();
    if session.shared.snapshot().state == "idle" {
        session.shared.wait_ready(SOCKET_READY_WAIT).await;
    }
    if socket.send(Message::Text(session.shared.snapshot_json().into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            received = updates.recv() => {
                // A TV the user removed must stop reporting to a socket opened before that.
                if inner.target_by_key(&key).is_none() {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
                match received {
                    Ok(state_json) => {
                        if socket.send(Message::Text(state_json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(_)) => continue,
                    _ => break,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// App wiring
// ---------------------------------------------------------------------------

/// Loads the saved TVs and, when there are any, opens the bridge before the first page loads.
pub fn init(app: &tauri::AppHandle) {
    let state = app.state::<BridgeState>();
    let config_dir = match app.path().app_config_dir() {
        Ok(config_dir) => config_dir,
        Err(error) => {
            log::warn!("[cast-bridge] app_config_dir unavailable: {error}");
            return;
        }
    };
    let store = load_store(&config_dir);
    let has_targets = !store.targets.is_empty();
    *lock(&state.inner.config_dir) = Some(config_dir);
    *lock(&state.inner.preferred_port) = store.port;
    *lock(&state.inner.targets) = store.targets;
    if has_targets {
        if let Err(error) = state.inner.ensure_server() {
            log::warn!("[cast-bridge] {error}");
        }
    }
}

/// Scans the home network for Chromecasts and DLNA TVs (takes about three seconds).
#[tauri::command]
pub async fn cast_bridge_discover(state: tauri::State<'_, BridgeState>) -> Result<Vec<FoundDeviceSummary>, String> {
    let inner = state.inner.clone();
    let found = inner.scan_network().await;
    let targets = lock(&inner.targets).clone();
    Ok(found
        .iter()
        .map(|device| FoundDeviceSummary {
            found_id: device.found_id(),
            kind: device.kind,
            name: device.name.clone(),
            address: device.host.clone(),
            model: device.model.clone(),
            already_added: targets.iter().any(|target| discovery::same_device(target, device) && target.host == device.host),
        })
        .collect())
}

/// Saves one result of the latest scan and returns the entry the sender should store for it.
#[tauri::command]
pub async fn cast_bridge_add(state: tauri::State<'_, BridgeState>, found_id: String) -> Result<BridgedDevice, String> {
    let inner = state.inner.clone();
    let found = lock(&inner.found).iter().find(|device| device.found_id() == found_id).cloned();
    let Some(found) = found else {
        return Err("STALE:scan again and pick the tv".to_string());
    };
    inner.add_target(&found)
}

/// Saves a Chromecast by its address, for networks that hide TVs from the scan.
#[tauri::command]
pub async fn cast_bridge_add_manual(state: tauri::State<'_, BridgeState>, host: String) -> Result<BridgedDevice, String> {
    let inner = state.inner.clone();
    let address: Ipv4Addr = host.trim().parse().map_err(|_| "INVALID:enter the tv's address, like 192.168.1.20".to_string())?;
    if !address.is_private() {
        return Err("INVALID:that address is not on the home network".to_string());
    }
    let Some(found) = discovery::describe_chromecast_at(address).await else {
        return Err("UNREACHABLE:no chromecast answered at that address".to_string());
    };
    inner.add_target(&found)
}

/// Reconciles with the sender's own list: forgets TVs it removed and reports the bridge's port.
#[tauri::command]
pub async fn cast_bridge_sync(state: tauri::State<'_, BridgeState>, known_ids: Vec<String>) -> Result<BridgeStatus, String> {
    Ok(state.inner.sync(&known_ids))
}

#[cfg(test)]
mod tests {
    use super::dlna::test_support::{idle_tv, start_fake_tv, FakeTv};
    use super::*;

    fn bridge_with(targets: Vec<Target>) -> Arc<BridgeInner> {
        let state = BridgeState::default();
        *lock(&state.inner.targets) = targets;
        state.inner
    }

    /// Serves the bridge on a free port; the saved TVs are whatever the test put in.
    fn start_bridge(inner: &Arc<BridgeInner>) -> String {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = serve(inner.clone(), listener).unwrap();
        *lock(&inner.port) = Some(port);
        format!("http://127.0.0.1:{port}")
    }

    async fn wait_for_state(client: &reqwest::Client, base_url: &str, key: &str, wanted: &str) -> serde_json::Value {
        let mut last = serde_json::Value::Null;
        for _ in 0..300 {
            last = client.get(format!("{base_url}/state")).header(AUTH_HEADER, key).send().await.unwrap().json().await.unwrap();
            if last["state"] == wanted {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("state {wanted} never arrived, last: {last}");
    }

    fn chromecast_target(id: &str, host: &str) -> Target {
        Target {
            id: id.to_string(),
            kind: TargetKind::Chromecast,
            name: "Living Room TV".to_string(),
            host: host.to_string(),
            port: chromecast::CAST_PORT,
            key: generate_hex(16),
            uid: Some("abc".to_string()),
            manufacturer: None,
            dlna: None,
        }
    }

    #[test]
    fn store_round_trips_and_drops_invalid_entries() {
        let dir = std::env::temp_dir().join(format!("xt-cast-bridge-store-{}", generate_hex(6)));
        let good = chromecast_target("ext-chromecast-1", "192.168.1.50");
        let off_lan = chromecast_target("ext-chromecast-2", "8.8.8.8");
        let bad_key = Target { key: "nope".to_string(), ..chromecast_target("ext-chromecast-3", "192.168.1.51") };
        let dlna_without_endpoints = Target { kind: TargetKind::Dlna, ..chromecast_target("ext-dlna-4", "192.168.1.52") };
        save_store(&dir, Some(47833), &[good.clone(), off_lan, bad_key, dlna_without_endpoints]).unwrap();

        let loaded = load_store(&dir);
        assert_eq!(loaded.port, Some(47833));
        assert_eq!(loaded.targets, vec![good]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_or_missing_store_is_an_empty_store() {
        assert!(parse_store("not json").targets.is_empty());
        assert!(parse_store(r#"{"v":2,"targets":[]}"#).targets.is_empty());
        let missing = std::env::temp_dir().join(format!("xt-cast-bridge-missing-{}", generate_hex(6)));
        assert!(load_store(&missing).targets.is_empty());
    }

    #[test]
    fn generated_keys_pass_validation() {
        assert!(is_valid_key(&generate_hex(16)));
        assert!(!is_valid_key(&generate_hex(8)));
    }

    #[tokio::test]
    async fn sync_forgets_tvs_the_sender_no_longer_lists() {
        let kept = chromecast_target("ext-chromecast-keep", "192.168.1.50");
        let dropped = chromecast_target("ext-chromecast-drop", "192.168.1.51");
        let inner = bridge_with(vec![kept.clone(), dropped.clone()]);
        start_bridge(&inner);

        let status = inner.sync(&[kept.id.clone()]);
        assert_eq!(status.devices.len(), 1);
        assert_eq!(status.devices[0].id, kept.id);
        assert_eq!(status.devices[0].host, LOOPBACK);
        assert_eq!(status.devices[0].address, "192.168.1.50");
        assert!(inner.target_by_key(&dropped.key).is_none());
    }

    #[tokio::test]
    async fn adding_the_same_tv_twice_keeps_its_key() {
        let inner = bridge_with(Vec::new());
        start_bridge(&inner);
        let found = FoundDevice {
            kind: TargetKind::Chromecast,
            name: "Living Room TV".to_string(),
            host: "192.168.1.50".to_string(),
            port: chromecast::CAST_PORT,
            uid: Some("abc".to_string()),
            manufacturer: None,
            model: None,
            dlna: None,
        };
        let first = inner.add_target(&found).unwrap();
        let moved = FoundDevice { host: "192.168.1.77".to_string(), name: "Lounge TV".to_string(), ..found };
        let second = inner.add_target(&moved).unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.key, second.key);
        assert_eq!(second.address, "192.168.1.77");
        assert_eq!(second.name, "Lounge TV");
        assert_eq!(lock(&inner.targets).len(), 1);
    }

    #[tokio::test]
    async fn a_tv_outside_the_home_network_cannot_be_added() {
        let inner = bridge_with(Vec::new());
        let found = FoundDevice {
            kind: TargetKind::Chromecast,
            name: "Elsewhere".to_string(),
            host: "203.0.113.9".to_string(),
            port: chromecast::CAST_PORT,
            uid: None,
            manufacturer: None,
            model: None,
            dlna: None,
        };
        assert!(inner.add_target(&found).is_err());
        assert!(lock(&inner.targets).is_empty());
    }

    #[tokio::test]
    async fn requests_without_the_tvs_key_are_refused() {
        let (target, _tv) = start_fake_tv(idle_tv()).await;
        let inner = bridge_with(vec![target]);
        let base_url = start_bridge(&inner);
        let client = reqwest::Client::new();

        let info = client.get(format!("{base_url}/info")).send().await.unwrap();
        assert_eq!(info.status(), 200);
        let info: serde_json::Value = info.json().await.unwrap();
        assert_eq!(info["app"], "extreme-infinitv");
        assert_eq!(info["v"], 1);

        let no_key = client.get(format!("{base_url}/state")).send().await.unwrap();
        assert_eq!(no_key.status(), 401);
        let wrong_key = client.post(format!("{base_url}/stop")).header(AUTH_HEADER, "0".repeat(32)).send().await.unwrap();
        assert_eq!(wrong_key.status(), 401);
        let socket = client
            .get(format!("{base_url}/events?key=wrong"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(socket.status(), 401);
    }

    #[tokio::test]
    async fn play_is_validated_before_it_reaches_a_tv() {
        let (target, tv) = start_fake_tv(idle_tv()).await;
        let key = target.key.clone();
        let inner = bridge_with(vec![target]);
        let base_url = start_bridge(&inner);
        let client = reqwest::Client::new();
        let play = |body: serde_json::Value| client.post(format!("{base_url}/play")).header(AUTH_HEADER, key.clone()).json(&body).send();

        assert_eq!(play(json!({"v": 2, "src": "http://h/1.mp4", "isLive": false})).await.unwrap().status(), 400);
        assert_eq!(play(json!({"v": 1, "src": "file:///etc/passwd", "isLive": false})).await.unwrap().status(), 400);
        assert_eq!(play(json!({"v": 1, "src": "rtsp://cam/1", "isLive": true})).await.unwrap().status(), 400);
        assert_eq!(play(json!({"src": "http://h/1.mp4"})).await.unwrap().status(), 400);
        assert!(tv.lock().unwrap().current_uri.is_none());
    }

    #[tokio::test]
    async fn a_cast_runs_end_to_end_through_the_receiver_protocol() {
        let (target, tv) = start_fake_tv(FakeTv { position: "0:05:00".to_string(), duration: "1:30:00".to_string(), ..idle_tv() }).await;
        let key = target.key.clone();
        let inner = bridge_with(vec![target]);
        let base_url = start_bridge(&inner);
        let client = reqwest::Client::new();

        let play = client
            .post(format!("{base_url}/play"))
            .header(AUTH_HEADER, &key)
            .json(&json!({"v": 1, "src": "http://provider/movie/u/p/9.mp4", "mime": "video/mp4", "isLive": false, "title": "A Film"}))
            .send()
            .await
            .unwrap();
        assert_eq!(play.status(), 200);

        let playing = wait_for_state(&client, &base_url, &key, "playing").await;
        assert_eq!(playing["title"], "A Film");
        assert_eq!(playing["durationSeconds"], 5400.0);
        assert_eq!(tv.lock().unwrap().current_uri.as_deref(), Some("http://provider/movie/u/p/9.mp4"));

        let pause = client.post(format!("{base_url}/pause")).header(AUTH_HEADER, &key).json(&json!({})).send().await.unwrap();
        assert_eq!(pause.status(), 200);
        wait_for_state(&client, &base_url, &key, "paused").await;

        let seek = client.post(format!("{base_url}/seek")).header(AUTH_HEADER, &key).json(&json!({"seconds": 90})).send().await.unwrap();
        assert_eq!(seek.status(), 200);
        let bad_seek = client.post(format!("{base_url}/seek")).header(AUTH_HEADER, &key).json(&json!({"seconds": -4})).send().await.unwrap();
        assert_eq!(bad_seek.status(), 400);

        let stop = client.post(format!("{base_url}/stop")).header(AUTH_HEADER, &key).json(&json!({})).send().await.unwrap();
        assert_eq!(stop.status(), 200);
        let idle = wait_for_state(&client, &base_url, &key, "idle").await;
        assert!(idle.get("title").is_none());
        assert_eq!(tv.lock().unwrap().seek_target.as_deref(), Some("0:01:30"));
    }

    #[tokio::test]
    async fn an_idle_tv_that_answers_reads_as_online() {
        let (target, _tv) = start_fake_tv(idle_tv()).await;
        let key = target.key.clone();
        let inner = bridge_with(vec![target]);
        let base_url = start_bridge(&inner);

        let state = reqwest::Client::new().get(format!("{base_url}/state")).header(AUTH_HEADER, &key).send().await.unwrap();
        assert_eq!(state.status(), 200);
        let state: serde_json::Value = state.json().await.unwrap();
        assert_eq!(state["state"], "idle");
        assert_eq!(state["volume"], 0.3);
    }

    #[tokio::test]
    async fn a_tv_that_is_switched_off_reads_as_offline() {
        let closed = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let closed_port = closed.local_addr().unwrap().port();
        drop(closed);
        let (mut target, _tv) = start_fake_tv(idle_tv()).await;
        if let Some(endpoints) = target.dlna.as_mut() {
            endpoints.av_transport_url = format!("http://127.0.0.1:{closed_port}/control");
        }
        let key = target.key.clone();
        let inner = bridge_with(vec![target]);
        let base_url = start_bridge(&inner);

        let state = reqwest::Client::new().get(format!("{base_url}/state")).header(AUTH_HEADER, &key).send().await.unwrap();
        assert_eq!(state.status(), 503);
    }
}

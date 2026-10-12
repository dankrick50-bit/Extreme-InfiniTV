// Google Cast driver: one task per Chromecast. It speaks CASTV2 to the TV's stock media player
// and turns the TV's status messages into the report shape the app's own receiver uses.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_rustls::rustls;

use super::session::{describe_source, url_extension, Command, PlayRequest, Refresher, SessionShared, Target};

const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";

/// Google's stock player, present on every Cast device; it needs no registration.
const DEFAULT_MEDIA_RECEIVER: &str = "CC1AD845";
const SENDER_ID: &str = "sender-0";
const PLATFORM_ID: &str = "receiver-0";

pub const CAST_PORT: u16 = 8009;

const TICK: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(4);
const PING_INTERVAL: Duration = Duration::from_secs(5);
const SILENCE_LIMIT: Duration = Duration::from_secs(20);
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Shorter than the sender's own 25s stall guard, so the specific reason below is what the user reads.
const LOAD_DEADLINE: Duration = Duration::from_secs(20);
const IDLE_DISCONNECT_AFTER: Duration = Duration::from_secs(20);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(3);
const MAX_RECONNECT_FAILURES: u32 = 5;
const PROBE_DEADLINE: Duration = Duration::from_secs(3);
/// A reachability answer this fresh is reused rather than opening another connection.
const PROBE_REUSE: Duration = Duration::from_secs(5);
const MAX_FRAME_BYTES: usize = 1024 * 1024;

const MSG_UNREACHABLE: &str = "Could not reach the TV. Check that it is switched on and on the same Wi-Fi.";
const MSG_UNSUPPORTED: &str = "This TV could not play the video. Its format may not be supported by Chromecast.";
const MSG_LAUNCH: &str = "The TV could not start its player.";
const MSG_TIMEOUT: &str = "The TV did not start playing in time.";
const MSG_LOST: &str = "Lost the connection to the TV.";
const MSG_PROTECTED: &str = "Protected channels cannot be sent to this TV.";

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

pub type BoxedReader = Box<dyn AsyncRead + Send + Unpin>;
pub type BoxedWriter = Box<dyn AsyncWrite + Send + Unpin>;
pub type ConnectFuture = Pin<Box<dyn Future<Output = std::io::Result<(BoxedReader, BoxedWriter)>> + Send>>;
/// Opens the byte stream to a TV. Swapped for an in-memory pipe in tests.
pub type Connector = Arc<dyn Fn(String, u16) -> ConnectFuture + Send + Sync>;

pub fn tls_connector() -> Connector {
    Arc::new(|host: String, port: u16| -> ConnectFuture { Box::pin(connect_tls(host, port)) })
}

/// Cast devices present a certificate issued per device by Google's Cast CA for no hostname, so
/// there is nothing a normal web check could match it against. Like every third-party sender, the
/// link is encrypted but the device is trusted by its address on the home network.
#[derive(Debug)]
struct AcceptDeviceCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptDeviceCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_client() -> std::io::Result<tokio_rustls::TlsConnector> {
    static CLIENT: OnceLock<Option<tokio_rustls::TlsConnector>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .ok()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptDeviceCertificate(provider)))
                .with_no_client_auth();
            Some(tokio_rustls::TlsConnector::from(Arc::new(config)))
        })
        .clone()
        .ok_or_else(|| std::io::Error::other("tls setup failed"))
}

async fn connect_tls(host: String, port: u16) -> std::io::Result<(BoxedReader, BoxedWriter)> {
    let ip: std::net::IpAddr = host
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "tv address is not an ip address"))?;
    let tcp = tokio::net::TcpStream::connect((ip, port)).await?;
    let _ = tcp.set_nodelay(true);
    let server_name = rustls::pki_types::ServerName::IpAddress(ip.into());
    let stream = tls_client()?.connect(server_name, tcp).await?;
    let (reader, writer) = tokio::io::split(stream);
    Ok((Box::new(reader), Box::new(writer)))
}

/// True when something answers the Cast port with a working TLS handshake.
pub async fn can_connect(host: &str, port: u16, limit: Duration) -> bool {
    matches!(tokio::time::timeout(limit, connect_tls(host.to_string(), port)).await, Ok(Ok(_)))
}

// ---------------------------------------------------------------------------
// Framing: a 4-byte length, then one protobuf CastMessage carrying a JSON string
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct CastMessage {
    pub source: String,
    pub destination: String,
    pub namespace: String,
    pub payload: String,
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_string(out: &mut Vec<u8>, tag: u8, text: &str) {
    out.push(tag);
    put_varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

pub fn encode_frame(source: &str, destination: &str, namespace: &str, payload: &str) -> Vec<u8> {
    let mut body = Vec::with_capacity(payload.len() + source.len() + destination.len() + namespace.len() + 16);
    body.extend_from_slice(&[0x08, 0x00]); // protocol_version = CASTV2_1_0
    put_string(&mut body, 0x12, source);
    put_string(&mut body, 0x1a, destination);
    put_string(&mut body, 0x22, namespace);
    body.extend_from_slice(&[0x28, 0x00]); // payload_type = STRING
    put_string(&mut body, 0x32, payload);

    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

fn read_varint(body: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value: u64 = 0;
    for shift in (0..64).step_by(7) {
        let byte = *body.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Decodes the message body (without its length prefix); unknown fields are skipped.
pub fn decode_message(body: &[u8]) -> Option<CastMessage> {
    let mut message =
        CastMessage { source: String::new(), destination: String::new(), namespace: String::new(), payload: String::new() };
    let mut cursor = 0usize;
    while cursor < body.len() {
        let tag = read_varint(body, &mut cursor)?;
        let field = tag >> 3;
        match tag & 0x07 {
            0 => {
                read_varint(body, &mut cursor)?;
            }
            1 => cursor = cursor.checked_add(8)?,
            2 => {
                let length = usize::try_from(read_varint(body, &mut cursor)?).ok()?;
                let end = cursor.checked_add(length)?;
                let bytes = body.get(cursor..end)?;
                cursor = end;
                let slot = match field {
                    2 => Some(&mut message.source),
                    3 => Some(&mut message.destination),
                    4 => Some(&mut message.namespace),
                    6 => Some(&mut message.payload),
                    _ => None,
                };
                if let Some(slot) = slot {
                    *slot = String::from_utf8_lossy(bytes).into_owned();
                }
            }
            5 => cursor = cursor.checked_add(4)?,
            _ => return None,
        }
    }
    (cursor == body.len()).then_some(message)
}

async fn read_frames<R: AsyncRead + Unpin>(mut reader: R, messages: mpsc::Sender<CastMessage>) {
    loop {
        let mut length_bytes = [0u8; 4];
        if reader.read_exact(&mut length_bytes).await.is_err() {
            return;
        }
        let length = u32::from_be_bytes(length_bytes) as usize;
        if length == 0 || length > MAX_FRAME_BYTES {
            return;
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        let Some(message) = decode_message(&body) else {
            continue;
        };
        if messages.send(message).await.is_err() {
            return;
        }
    }
}

struct Connection {
    writer: BoxedWriter,
    incoming: mpsc::Receiver<CastMessage>,
    reader_task: tokio::task::JoinHandle<()>,
    last_heard: Instant,
}

impl Connection {
    fn open(reader: BoxedReader, writer: BoxedWriter) -> Self {
        let (sender, incoming) = mpsc::channel(32);
        let reader_task = tokio::spawn(read_frames(reader, sender));
        Self { writer, incoming, reader_task, last_heard: Instant::now() }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

async fn next_message(connection: &mut Option<Connection>) -> Option<CastMessage> {
    match connection {
        Some(connection) => connection.incoming.recv().await,
        None => std::future::pending().await,
    }
}

// ---------------------------------------------------------------------------
// What to tell the TV the stream is
// ---------------------------------------------------------------------------

/// The stock player picks its playback path from this label, not from the stream itself. Plain
/// files go in as MP4: that path hands the link to the TV's video element, which then recognises
/// MKV by its content, whereas labelling it Matroska is refused outright.
pub fn cast_content_type(request: &PlayRequest) -> &'static str {
    match url_extension(&request.src).as_deref() {
        Some("m3u8") => "application/x-mpegURL",
        Some("mpd") => "application/dash+xml",
        Some("ts") => "video/mp2t",
        Some("webm") => "video/webm",
        Some("mp3") => "audio/mpeg",
        Some(_) => "video/mp4",
        // Providers serve an extensionless live link as a raw transport stream.
        None if request.is_live => "video/mp2t",
        None => "video/mp4",
    }
}

fn load_payload(request: &PlayRequest, request_id: u64, app_session_id: Option<&str>) -> Value {
    let mut metadata = json!({"metadataType": 0, "title": request.title_text()});
    if let Some(logo) = request.logo.as_deref().filter(|logo| logo.starts_with("http://") || logo.starts_with("https://")) {
        metadata["images"] = json!([{"url": logo}]);
    }
    let mut media = json!({
        "contentId": request.src,
        "contentType": cast_content_type(request),
        "streamType": if request.is_live { "LIVE" } else { "BUFFERED" },
        "metadata": metadata,
    });
    if let Some(duration) = request.known_duration() {
        media["duration"] = json!(duration);
    }
    let mut load = json!({"type": "LOAD", "requestId": request_id, "media": media, "autoplay": true});
    if let Some(resume) = request.resume_position() {
        load["currentTime"] = json!(resume);
    }
    if let Some(session) = app_session_id {
        load["sessionId"] = json!(session);
    }
    load
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

struct PendingLoad {
    request: PlayRequest,
    launch_sent: bool,
    load_request_id: Option<u64>,
    deadline: Instant,
}

/// The TV only reports position on a state change; between reports it is extrapolated.
struct PositionClock {
    base: f64,
    at: Instant,
    running: bool,
}

impl PositionClock {
    fn new() -> Self {
        Self { base: 0.0, at: Instant::now(), running: false }
    }

    fn set(&mut self, seconds: f64, running: bool) {
        self.base = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
        self.at = Instant::now();
        self.running = running;
    }

    fn now(&self) -> f64 {
        if self.running {
            self.base + self.at.elapsed().as_secs_f64()
        } else {
            self.base
        }
    }
}

struct Driver {
    target: Target,
    shared: Arc<SessionShared>,
    connector: Connector,
    refresher: Option<Refresher>,
    connection: Option<Connection>,
    next_request_id: u64,
    transport_id: Option<String>,
    app_session_id: Option<String>,
    media_session_id: Option<i64>,
    previous_media_session_id: Option<i64>,
    pending: Option<PendingLoad>,
    seen_playing: bool,
    media_active: bool,
    stop_requested: bool,
    clock: PositionClock,
    duration: Option<f64>,
    last_ping: Instant,
    last_status_poll: Instant,
    idle_since: Option<Instant>,
    reconnect_failures: u32,
    next_reconnect: Instant,
    probe_deadline: Option<Instant>,
    last_connect_attempt: Option<Instant>,
}

fn media_session_id_of(entry: &Value) -> Option<i64> {
    entry.get("mediaSessionId").and_then(Value::as_i64)
}

fn player_state_of(entry: &Value) -> &str {
    entry.get("playerState").and_then(Value::as_str).unwrap_or_default()
}

impl Driver {
    fn new(target: Target, shared: Arc<SessionShared>, connector: Connector, refresher: Option<Refresher>) -> Self {
        let now = Instant::now();
        Self {
            target,
            shared,
            connector,
            refresher,
            connection: None,
            next_request_id: 1,
            transport_id: None,
            app_session_id: None,
            media_session_id: None,
            previous_media_session_id: None,
            pending: None,
            seen_playing: false,
            media_active: false,
            stop_requested: false,
            clock: PositionClock::new(),
            duration: None,
            last_ping: now,
            last_status_poll: now,
            idle_since: None,
            reconnect_failures: 0,
            next_reconnect: now,
            probe_deadline: None,
            last_connect_attempt: None,
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id += 1;
        id
    }

    async fn connect(&mut self, limit: Duration) -> bool {
        let attempt = (self.connector)(self.target.host.clone(), self.target.port);
        let outcome = tokio::time::timeout(limit, attempt).await;
        self.last_connect_attempt = Some(Instant::now());
        self.shared.set_reachable(matches!(outcome, Ok(Ok(_))));
        let (reader, writer) = match outcome {
            Ok(Ok(streams)) => streams,
            Ok(Err(error)) => {
                log::info!("[cast-bridge] chromecast {} connect failed: {error}", self.target.name);
                return false;
            }
            Err(_) => {
                log::info!("[cast-bridge] chromecast {} connect timed out", self.target.name);
                return false;
            }
        };
        self.connection = Some(Connection::open(reader, writer));
        // The app-level link and any request in flight belonged to the old socket.
        self.transport_id = None;
        if let Some(pending) = self.pending.as_mut() {
            pending.launch_sent = false;
            pending.load_request_id = None;
        }
        self.last_ping = Instant::now();
        self.idle_since = None;
        self.send(PLATFORM_ID, NS_CONNECTION, json!({"type": "CONNECT"})).await
    }

    /// Connects, looking the TV up again once if its saved address no longer answers.
    async fn connect_for_play(&mut self) -> bool {
        if self.connection.is_some() || self.connect(CONNECT_TIMEOUT).await {
            return true;
        }
        let Some(refresher) = self.refresher.clone() else {
            return false;
        };
        let Some(updated) = refresher().await else {
            return false;
        };
        log::info!("[cast-bridge] chromecast {} moved to {}", updated.name, updated.host);
        self.target = updated;
        self.connect(CONNECT_TIMEOUT).await
    }

    async fn send(&mut self, destination: &str, namespace: &str, payload: Value) -> bool {
        let Some(connection) = self.connection.as_mut() else {
            return false;
        };
        let frame = encode_frame(SENDER_ID, destination, namespace, &payload.to_string());
        let written = tokio::time::timeout(WRITE_TIMEOUT, async {
            connection.writer.write_all(&frame).await?;
            connection.writer.flush().await
        })
        .await;
        if matches!(written, Ok(Ok(()))) {
            return true;
        }
        log::info!("[cast-bridge] chromecast {} write failed, dropping the connection", self.target.name);
        self.on_disconnected();
        false
    }

    async fn request_receiver_status(&mut self) {
        let id = self.next_id();
        self.send(PLATFORM_ID, NS_RECEIVER, json!({"type": "GET_STATUS", "requestId": id})).await;
    }

    async fn request_media_status(&mut self) {
        let Some(transport) = self.transport_id.clone() else {
            return;
        };
        let id = self.next_id();
        self.send(&transport, NS_MEDIA, json!({"type": "GET_STATUS", "requestId": id})).await;
    }

    fn finish_probe(&mut self) {
        self.probe_deadline = None;
        self.shared.mark_ready();
    }

    fn on_disconnected(&mut self) {
        self.connection = None;
        self.transport_id = None;
        self.next_reconnect = Instant::now() + TICK;
        self.finish_probe();
    }

    async fn close_connection(&mut self) {
        if self.connection.is_none() {
            return;
        }
        self.send(PLATFORM_ID, NS_CONNECTION, json!({"type": "CLOSE"})).await;
        self.connection = None;
        self.transport_id = None;
        self.idle_since = None;
    }

    fn end_media(&mut self) {
        self.media_active = false;
        if let Some(id) = self.media_session_id.take() {
            self.previous_media_session_id = Some(id);
        }
    }

    fn fail(&mut self, message: &str) {
        log::warn!("[cast-bridge] chromecast {}: {message}", self.target.name);
        self.pending = None;
        self.end_media();
        self.clock.set(0.0, false);
        self.shared.fail(message);
    }

    fn publish(&mut self, state: &str) {
        let position = self.capped_position();
        let duration = self.duration;
        self.shared.update(|report| {
            report.state = state.to_string();
            report.position_seconds = position;
            if duration.is_some() {
                report.duration_seconds = duration;
            }
            report.error = None;
        });
    }

    fn capped_position(&self) -> f64 {
        let position = self.clock.now();
        match self.duration {
            Some(duration) => position.min(duration),
            None => position,
        }
    }

    // -- commands -----------------------------------------------------------

    async fn on_command(&mut self, command: Command) {
        match command {
            Command::Play(request) => self.start_play(*request).await,
            Command::Pause => {
                self.media_command("PAUSE", json!({})).await;
            }
            Command::Resume => {
                self.media_command("PLAY", json!({})).await;
            }
            Command::Seek(seconds) => {
                if self.media_command("SEEK", json!({"currentTime": seconds})).await {
                    let running = self.clock.running;
                    self.clock.set(seconds, running);
                    let position = self.capped_position();
                    self.shared.update(|report| report.position_seconds = position);
                }
            }
            Command::Stop => self.stop().await,
            Command::Volume { level, muted } => self.set_volume(level, muted).await,
            Command::Probe(done) => {
                self.check_reachable().await;
                let _ = done.send(());
            }
        }
    }

    /// Answers "is the TV there?" for the sender's online check, without disturbing playback.
    async fn check_reachable(&mut self) {
        // The sender is still watching, so keep the link it is asking about.
        self.idle_since = None;
        if self.connection.is_some() {
            return;
        }
        let fresh = self.last_connect_attempt.is_some_and(|attempt| attempt.elapsed() < PROBE_REUSE);
        if !fresh && self.connect(PROBE_CONNECT_TIMEOUT).await {
            self.request_receiver_status().await;
        }
    }

    async fn start_play(&mut self, request: PlayRequest) {
        log::info!("[cast-bridge] chromecast {} play {}", self.target.name, describe_source(&request));
        self.end_media();
        self.seen_playing = false;
        self.stop_requested = false;
        self.reconnect_failures = 0;
        self.duration = request.known_duration();
        self.clock.set(0.0, false);
        self.shared.begin_loading(request.title.clone(), request.known_duration());
        if request.is_protected() {
            self.fail(MSG_PROTECTED);
            return;
        }
        self.pending = Some(PendingLoad {
            request,
            launch_sent: false,
            load_request_id: None,
            deadline: Instant::now() + LOAD_DEADLINE,
        });
        if !self.connect_for_play().await {
            self.fail(MSG_UNREACHABLE);
            return;
        }
        // The answer says whether the player is already open; the rest follows from there.
        self.request_receiver_status().await;
    }

    async fn media_command(&mut self, kind: &str, extra: Value) -> bool {
        let (Some(transport), Some(media_session_id)) = (self.transport_id.clone(), self.media_session_id) else {
            return false;
        };
        let mut payload = extra;
        payload["type"] = json!(kind);
        payload["mediaSessionId"] = json!(media_session_id);
        payload["requestId"] = json!(self.next_id());
        self.send(&transport, NS_MEDIA, payload).await
    }

    async fn stop(&mut self) {
        self.pending = None;
        self.end_media();
        self.clock.set(0.0, false);
        self.shared.set_idle();
        if self.connection.is_none() && !self.connect(PROBE_CONNECT_TIMEOUT).await {
            return;
        }
        match self.app_session_id.take() {
            Some(session) => {
                let id = self.next_id();
                self.send(PLATFORM_ID, NS_RECEIVER, json!({"type": "STOP", "sessionId": session, "requestId": id})).await;
                self.transport_id = None;
            }
            // Not attached to the player yet: ask what is running and close it when the answer lands.
            None => {
                self.stop_requested = true;
                self.request_receiver_status().await;
            }
        }
    }

    async fn set_volume(&mut self, level: f64, muted: bool) {
        if self.connection.is_none() && !self.connect(CONNECT_TIMEOUT).await {
            return;
        }
        let level = level.clamp(0.0, 1.0);
        let level_id = self.next_id();
        let sent = self
            .send(PLATFORM_ID, NS_RECEIVER, json!({"type": "SET_VOLUME", "volume": {"level": level}, "requestId": level_id}))
            .await;
        if !sent {
            return;
        }
        let mute_id = self.next_id();
        self.send(PLATFORM_ID, NS_RECEIVER, json!({"type": "SET_VOLUME", "volume": {"muted": muted}, "requestId": mute_id}))
            .await;
        self.shared.set_volume(Some(level), Some(muted));
    }

    // -- messages from the TV -----------------------------------------------

    async fn on_message(&mut self, message: CastMessage) {
        if let Some(connection) = self.connection.as_mut() {
            connection.last_heard = Instant::now();
        }
        let Ok(payload) = serde_json::from_str::<Value>(&message.payload) else {
            return;
        };
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
        match (message.namespace.as_str(), kind.as_str()) {
            (NS_HEARTBEAT, "PING") => {
                self.send(PLATFORM_ID, NS_HEARTBEAT, json!({"type": "PONG"})).await;
            }
            (NS_CONNECTION, "CLOSE") => self.on_remote_close(&message.source),
            (NS_RECEIVER, "RECEIVER_STATUS") => self.on_receiver_status(&payload).await,
            (NS_RECEIVER, "LAUNCH_ERROR") => {
                if self.pending.is_some() {
                    self.fail(MSG_LAUNCH);
                }
            }
            (NS_MEDIA, "MEDIA_STATUS") => self.on_media_status(&payload),
            (NS_MEDIA, "LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST") => self.on_load_rejected(&payload),
            _ => {}
        }
    }

    fn on_remote_close(&mut self, source: &str) {
        if source == PLATFORM_ID {
            self.on_disconnected();
            return;
        }
        if self.transport_id.as_deref() != Some(source) {
            return;
        }
        // The player app itself went away (another app took the TV, or it was closed there).
        self.transport_id = None;
        self.app_session_id = None;
        if self.pending.is_none() && self.media_active {
            self.end_media();
            self.clock.set(0.0, false);
            self.shared.set_idle();
        }
    }

    async fn on_receiver_status(&mut self, payload: &Value) {
        let status = payload.get("status");
        if let Some(volume) = status.and_then(|status| status.get("volume")) {
            self.shared.set_volume(
                volume.get("level").and_then(Value::as_f64),
                volume.get("muted").and_then(Value::as_bool),
            );
        }
        let player = status
            .and_then(|status| status.get("applications"))
            .and_then(Value::as_array)
            .and_then(|applications| {
                applications
                    .iter()
                    .find(|application| application.get("appId").and_then(Value::as_str) == Some(DEFAULT_MEDIA_RECEIVER))
            });
        let transport = player.and_then(|player| player.get("transportId")).and_then(Value::as_str).map(str::to_string);
        let session = player.and_then(|player| player.get("sessionId")).and_then(Value::as_str).map(str::to_string);

        let Some(transport) = transport else {
            self.transport_id = None;
            self.app_session_id = None;
            self.stop_requested = false;
            let needs_launch = self.pending.as_ref().is_some_and(|pending| !pending.launch_sent);
            if needs_launch {
                if let Some(pending) = self.pending.as_mut() {
                    pending.launch_sent = true;
                    pending.load_request_id = None;
                }
                let id = self.next_id();
                self.send(PLATFORM_ID, NS_RECEIVER, json!({"type": "LAUNCH", "appId": DEFAULT_MEDIA_RECEIVER, "requestId": id}))
                    .await;
            } else if self.pending.is_none() {
                if self.media_active {
                    self.end_media();
                    self.clock.set(0.0, false);
                    self.shared.set_idle();
                }
                self.finish_probe();
            }
            return;
        };

        if self.stop_requested {
            self.stop_requested = false;
            if let Some(session) = session {
                let id = self.next_id();
                self.send(PLATFORM_ID, NS_RECEIVER, json!({"type": "STOP", "sessionId": session, "requestId": id})).await;
            }
            return;
        }

        let is_new_link = self.transport_id.as_deref() != Some(transport.as_str());
        if is_new_link {
            self.transport_id = Some(transport.clone());
            self.app_session_id = session;
            if !self.send(&transport, NS_CONNECTION, json!({"type": "CONNECT"})).await {
                return;
            }
        }
        let load_due = self.pending.as_ref().is_some_and(|pending| pending.load_request_id.is_none());
        if load_due {
            self.send_load(&transport).await;
        } else if is_new_link && self.pending.is_none() {
            // Found the player already open (app restart, or a dropped connection): pick its state up.
            self.request_media_status().await;
        }
    }

    async fn send_load(&mut self, transport: &str) {
        let id = self.next_id();
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let payload = load_payload(&pending.request, id, self.app_session_id.as_deref());
        if !self.send(transport, NS_MEDIA, payload).await {
            return;
        }
        if let Some(pending) = self.pending.as_mut() {
            pending.load_request_id = Some(id);
        }
    }

    fn on_load_rejected(&mut self, payload: &Value) {
        let request_id = payload.get("requestId").and_then(Value::as_u64);
        let is_our_load = self.pending.as_ref().is_some_and(|pending| pending.load_request_id.is_some() && pending.load_request_id == request_id);
        if is_our_load {
            self.fail(MSG_UNSUPPORTED);
        }
    }

    fn on_media_status(&mut self, payload: &Value) {
        let request_id = payload.get("requestId").and_then(Value::as_u64).unwrap_or(0);
        let entries: Vec<Value> = payload.get("status").and_then(Value::as_array).cloned().unwrap_or_default();

        if let Some(pending) = self.pending.as_ref() {
            // Until our LOAD is out, anything arriving describes whatever played before it.
            let Some(load_id) = pending.load_request_id else {
                return;
            };
            let answers_load = request_id == load_id;
            let previous = self.previous_media_session_id;
            let adopted = entries.iter().find(|entry| {
                let id = media_session_id_of(entry);
                answers_load || (id.is_some() && id != previous && player_state_of(entry) != "IDLE")
            });
            let Some(entry) = adopted.cloned() else {
                return;
            };
            self.media_session_id = media_session_id_of(&entry);
            self.pending = None;
            self.apply_media_entry(&entry);
            return;
        }

        let entry = match self.media_session_id {
            Some(id) => entries.iter().find(|entry| media_session_id_of(entry) == Some(id)).cloned(),
            None => entries.first().cloned(),
        };
        // Nothing loaded, or only something another sender put there: ours is over either way.
        let Some(entry) = entry else {
            if self.media_active {
                self.end_media();
                self.clock.set(0.0, false);
                self.shared.set_idle();
            }
            self.finish_probe();
            return;
        };
        if self.media_session_id.is_none() {
            self.media_session_id = media_session_id_of(&entry);
        }
        self.apply_media_entry(&entry);
        self.finish_probe();
    }

    fn apply_media_entry(&mut self, entry: &Value) {
        let reported_time = entry.get("currentTime").and_then(Value::as_f64).unwrap_or_else(|| self.clock.now());
        if let Some(duration) =
            entry.pointer("/media/duration").and_then(Value::as_f64).filter(|duration| duration.is_finite() && *duration > 0.0)
        {
            self.duration = Some(duration);
        }
        match player_state_of(entry) {
            "PLAYING" => {
                self.seen_playing = true;
                self.media_active = true;
                self.clock.set(reported_time, true);
                self.publish("playing");
            }
            "PAUSED" => {
                self.media_active = true;
                self.clock.set(reported_time, false);
                self.publish("paused");
            }
            "BUFFERING" | "LOADING" => {
                self.media_active = true;
                self.clock.set(reported_time, false);
                self.publish(if self.seen_playing { "buffering" } else { "loading" });
            }
            "IDLE" => match entry.get("idleReason").and_then(Value::as_str) {
                Some("FINISHED") => {
                    let end = self.duration.unwrap_or(reported_time);
                    self.clock.set(end, false);
                    self.end_media();
                    self.publish("ended");
                }
                Some("ERROR") => self.fail(MSG_UNSUPPORTED),
                // No reason given while a load is still settling: not a stop.
                None if !self.seen_playing => {
                    self.media_active = true;
                    self.publish("loading");
                }
                _ => {
                    self.end_media();
                    self.clock.set(0.0, false);
                    self.shared.set_idle();
                }
            },
            _ => {}
        }
    }

    // -- housekeeping -------------------------------------------------------

    async fn on_tick(&mut self) {
        let now = Instant::now();
        if self.probe_deadline.is_some_and(|deadline| now >= deadline) {
            self.finish_probe();
        }
        if self.pending.as_ref().is_some_and(|pending| now >= pending.deadline) {
            self.fail(MSG_TIMEOUT);
        }

        let silence = self.connection.as_ref().map(|connection| now.duration_since(connection.last_heard));
        match silence {
            Some(silence) if silence >= SILENCE_LIMIT => {
                log::info!("[cast-bridge] chromecast {} went silent", self.target.name);
                self.on_disconnected();
            }
            Some(_) => {
                if now.duration_since(self.last_ping) >= PING_INTERVAL {
                    self.last_ping = now;
                    self.send(PLATFORM_ID, NS_HEARTBEAT, json!({"type": "PING"})).await;
                }
                if self.media_active && now.duration_since(self.last_status_poll) >= STATUS_POLL_INTERVAL {
                    self.last_status_poll = now;
                    self.request_media_status().await;
                }
                let busy = self.media_active || self.pending.is_some() || self.probe_deadline.is_some() || self.stop_requested;
                if busy {
                    self.idle_since = None;
                } else {
                    let idle_since = *self.idle_since.get_or_insert(now);
                    if now.duration_since(idle_since) >= IDLE_DISCONNECT_AFTER {
                        self.close_connection().await;
                    }
                }
            }
            None => {
                let wanted = self.media_active || self.pending.is_some();
                if wanted && now >= self.next_reconnect {
                    if self.connect(CONNECT_TIMEOUT).await {
                        self.reconnect_failures = 0;
                        self.request_receiver_status().await;
                    } else {
                        self.reconnect_failures += 1;
                        self.next_reconnect = Instant::now() + RECONNECT_INTERVAL;
                        if self.reconnect_failures >= MAX_RECONNECT_FAILURES {
                            self.fail(MSG_LOST);
                        }
                    }
                }
            }
        }

        if self.media_active && self.clock.running {
            let position = self.capped_position();
            self.shared.update(|report| report.position_seconds = position);
        }
    }
}

pub async fn run(
    target: Target,
    shared: Arc<SessionShared>,
    mut commands: mpsc::Receiver<Command>,
    connector: Connector,
    refresher: Option<Refresher>,
) {
    let mut driver = Driver::new(target, shared, connector, refresher);

    // The TV may already be playing something this app started before it was closed.
    if driver.connect(PROBE_CONNECT_TIMEOUT).await {
        driver.probe_deadline = Some(Instant::now() + PROBE_DEADLINE);
        driver.request_receiver_status().await;
    } else {
        driver.finish_probe();
    }

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(command) => driver.on_command(command).await,
                None => break,
            },
            message = next_message(&mut driver.connection) => match message {
                Some(message) => driver.on_message(message).await,
                None => driver.on_disconnected(),
            },
            _ = ticker.tick() => driver.on_tick().await,
        }
    }
    driver.close_connection().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cast_bridge::session::{Report, TargetKind};
    use std::sync::Mutex;

    fn play_request(json: Value) -> PlayRequest {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn frame_round_trips_through_encode_and_decode() {
        let payload = r#"{"type":"PING"}"#;
        let frame = encode_frame("sender-0", "receiver-0", NS_HEARTBEAT, payload);
        let length = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(length, frame.len() - 4);
        let decoded = decode_message(&frame[4..]).unwrap();
        assert_eq!(decoded.source, "sender-0");
        assert_eq!(decoded.destination, "receiver-0");
        assert_eq!(decoded.namespace, NS_HEARTBEAT);
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn frame_matches_the_documented_wire_bytes() {
        let frame = encode_frame("a", "b", "c", "d");
        assert_eq!(
            &frame[4..],
            &[0x08, 0x00, 0x12, 0x01, b'a', 0x1a, 0x01, b'b', 0x22, 0x01, b'c', 0x28, 0x00, 0x32, 0x01, b'd']
        );
    }

    #[test]
    fn long_payloads_use_a_multi_byte_length() {
        let payload = "x".repeat(300);
        let frame = encode_frame("s", "r", "n", &payload);
        assert_eq!(decode_message(&frame[4..]).unwrap().payload, payload);
    }

    #[test]
    fn decode_skips_unknown_fields_and_rejects_truncated_input() {
        let mut body = encode_frame("s", "r", "n", "p")[4..].to_vec();
        body.extend_from_slice(&[0x3a, 0x02, 0xff, 0xfe]); // field 7 (binary payload)
        body.extend_from_slice(&[0x40, 0x05]); // an unknown varint field
        assert_eq!(decode_message(&body).unwrap().payload, "p");

        let truncated = &encode_frame("sender", "receiver", "namespace", "payload")[4..12];
        assert!(decode_message(truncated).is_none());
    }

    #[test]
    fn content_type_follows_the_link_not_the_senders_label() {
        let live_ts = play_request(json!({"v": 1, "src": "http://h/live/u/p/1.ts", "mime": "application/x-mpegURL", "isLive": true}));
        assert_eq!(cast_content_type(&live_ts), "video/mp2t");
        let live_bare = play_request(json!({"v": 1, "src": "http://h/u/p/1", "isLive": true}));
        assert_eq!(cast_content_type(&live_bare), "video/mp2t");
        let live_hls = play_request(json!({"v": 1, "src": "http://h/live/u/p/1.m3u8?x=1", "isLive": true}));
        assert_eq!(cast_content_type(&live_hls), "application/x-mpegURL");
        let mkv = play_request(json!({"v": 1, "src": "http://h/movie/u/p/1.mkv", "mime": "video/x-matroska", "isLive": false}));
        assert_eq!(cast_content_type(&mkv), "video/mp4");
        let mp4 = play_request(json!({"v": 1, "src": "http://h/movie/u/p/1.mp4", "isLive": false}));
        assert_eq!(cast_content_type(&mp4), "video/mp4");
    }

    #[test]
    fn load_payload_carries_resume_title_and_artwork() {
        let request = play_request(json!({
            "v": 1, "src": "http://h/movie/u/p/1.mp4", "isLive": false, "title": "A Film",
            "logo": "https://img/poster.jpg", "resumeSeconds": 120.0, "durationSeconds": 5400.0
        }));
        let payload = load_payload(&request, 7, Some("session-1"));
        assert_eq!(payload["type"], "LOAD");
        assert_eq!(payload["requestId"], 7);
        assert_eq!(payload["sessionId"], "session-1");
        assert_eq!(payload["currentTime"], 120.0);
        assert_eq!(payload["media"]["contentId"], "http://h/movie/u/p/1.mp4");
        assert_eq!(payload["media"]["streamType"], "BUFFERED");
        assert_eq!(payload["media"]["duration"], 5400.0);
        assert_eq!(payload["media"]["metadata"]["title"], "A Film");
        assert_eq!(payload["media"]["metadata"]["images"][0]["url"], "https://img/poster.jpg");
    }

    #[test]
    fn load_payload_for_live_has_no_seek_or_duration() {
        let request = play_request(json!({"v": 1, "src": "http://h/live/u/p/1.ts", "isLive": true, "title": "News", "resumeSeconds": 50.0}));
        let payload = load_payload(&request, 1, None);
        assert_eq!(payload["media"]["streamType"], "LIVE");
        assert!(payload.get("currentTime").is_none());
        assert!(payload["media"].get("duration").is_none());
        assert!(payload.get("sessionId").is_none());
    }

    // -- a scripted TV on the other end of an in-memory pipe ------------------

    #[derive(Default)]
    struct FakeTv {
        player_open: bool,
        load_outcome: LoadOutcome,
        received: Vec<(String, Value)>,
        connections: u32,
    }

    #[derive(Default, Clone, Copy, PartialEq)]
    enum LoadOutcome {
        #[default]
        Plays,
        Fails,
    }

    async fn write_message<W: AsyncWrite + Unpin>(writer: &mut W, source: &str, namespace: &str, payload: Value) {
        let frame = encode_frame(source, "*", namespace, &payload.to_string());
        let _ = writer.write_all(&frame).await;
    }

    fn receiver_status(player_open: bool, request_id: u64) -> Value {
        let applications = if player_open {
            json!([{"appId": DEFAULT_MEDIA_RECEIVER, "transportId": "transport-1", "sessionId": "session-1"}])
        } else {
            json!([])
        };
        json!({"type": "RECEIVER_STATUS", "requestId": request_id, "status": {"applications": applications, "volume": {"level": 0.5, "muted": false}}})
    }

    async fn serve_fake_tv(stream: tokio::io::DuplexStream, tv: Arc<Mutex<FakeTv>>) {
        let (reader, mut writer) = tokio::io::split(stream);
        let (sender, mut incoming) = mpsc::channel(32);
        let reader_task = tokio::spawn(read_frames(reader, sender));
        while let Some(message) = incoming.recv().await {
            let payload: Value = serde_json::from_str(&message.payload).unwrap();
            let kind = payload["type"].as_str().unwrap_or_default().to_string();
            let request_id = payload["requestId"].as_u64().unwrap_or(0);
            tv.lock().unwrap().received.push((message.destination.clone(), payload.clone()));
            match (message.namespace.as_str(), kind.as_str()) {
                (NS_RECEIVER, "GET_STATUS") => {
                    let open = tv.lock().unwrap().player_open;
                    write_message(&mut writer, PLATFORM_ID, NS_RECEIVER, receiver_status(open, request_id)).await;
                }
                (NS_RECEIVER, "LAUNCH") => {
                    tv.lock().unwrap().player_open = true;
                    write_message(&mut writer, PLATFORM_ID, NS_RECEIVER, receiver_status(true, request_id)).await;
                }
                (NS_RECEIVER, "STOP") => {
                    tv.lock().unwrap().player_open = false;
                    write_message(&mut writer, PLATFORM_ID, NS_RECEIVER, receiver_status(false, request_id)).await;
                }
                (NS_MEDIA, "LOAD") => {
                    let outcome = tv.lock().unwrap().load_outcome;
                    if outcome == LoadOutcome::Fails {
                        write_message(&mut writer, "transport-1", NS_MEDIA, json!({"type": "LOAD_FAILED", "requestId": request_id})).await;
                        continue;
                    }
                    let start = payload["currentTime"].as_f64().unwrap_or(0.0);
                    write_message(&mut writer, "transport-1", NS_MEDIA, json!({
                        "type": "MEDIA_STATUS", "requestId": request_id,
                        "status": [{"mediaSessionId": 4, "playerState": "BUFFERING", "currentTime": start}]
                    })).await;
                    write_message(&mut writer, "transport-1", NS_MEDIA, json!({
                        "type": "MEDIA_STATUS", "requestId": 0,
                        "status": [{"mediaSessionId": 4, "playerState": "PLAYING", "currentTime": start, "media": {"duration": 5400.0}}]
                    })).await;
                }
                (NS_MEDIA, "PAUSE") => {
                    write_message(&mut writer, "transport-1", NS_MEDIA, json!({
                        "type": "MEDIA_STATUS", "requestId": request_id,
                        "status": [{"mediaSessionId": 4, "playerState": "PAUSED", "currentTime": 33.0}]
                    })).await;
                }
                (NS_MEDIA, "GET_STATUS") => {
                    write_message(&mut writer, "transport-1", NS_MEDIA, json!({
                        "type": "MEDIA_STATUS", "requestId": request_id,
                        "status": [{"mediaSessionId": 4, "playerState": "PLAYING", "currentTime": 900.0, "media": {"duration": 5400.0}}]
                    })).await;
                }
                _ => {}
            }
        }
        reader_task.abort();
    }

    fn fake_connector(tv: Arc<Mutex<FakeTv>>) -> Connector {
        Arc::new(move |_host: String, _port: u16| -> ConnectFuture {
            let tv = tv.clone();
            Box::pin(async move {
                tv.lock().unwrap().connections += 1;
                let (near, far) = tokio::io::duplex(64 * 1024);
                tokio::spawn(serve_fake_tv(far, tv));
                let (reader, writer) = tokio::io::split(near);
                Ok((Box::new(reader) as BoxedReader, Box::new(writer) as BoxedWriter))
            })
        })
    }

    fn target() -> Target {
        Target {
            id: "ext-chromecast-test".to_string(),
            kind: TargetKind::Chromecast,
            name: "Living Room TV".to_string(),
            host: "192.168.1.50".to_string(),
            port: CAST_PORT,
            key: "k".repeat(32),
            uid: None,
            manufacturer: None,
            dlna: None,
        }
    }

    struct Harness {
        shared: Arc<SessionShared>,
        commands: mpsc::Sender<Command>,
        tv: Arc<Mutex<FakeTv>>,
    }

    fn start(tv: FakeTv) -> Harness {
        let tv = Arc::new(Mutex::new(tv));
        let shared = SessionShared::new();
        let (commands, receiver) = mpsc::channel(8);
        tokio::spawn(run(target(), shared.clone(), receiver, fake_connector(tv.clone()), None));
        Harness { shared, commands, tv }
    }

    async fn wait_for(shared: &SessionShared, accept: impl Fn(&Report) -> bool) -> Report {
        for _ in 0..200 {
            let report = shared.snapshot();
            if accept(&report) {
                return report;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("state never arrived, last report: {:?}", shared.snapshot());
    }

    fn film() -> Command {
        Command::Play(Box::new(play_request(json!({
            "v": 1, "src": "http://provider/movie/u/p/9.mkv", "mime": "video/x-matroska", "isLive": false,
            "title": "A Film", "resumeSeconds": 600.0
        }))))
    }

    #[tokio::test]
    async fn play_launches_the_player_loads_the_link_and_reports_playing() {
        let harness = start(FakeTv::default());
        harness.commands.send(film()).await.unwrap();

        let report = wait_for(&harness.shared, |report| report.state == "playing").await;
        assert_eq!(report.title.as_deref(), Some("A Film"));
        assert_eq!(report.duration_seconds, Some(5400.0));
        assert!(report.position_seconds >= 600.0);
        assert_eq!(report.volume, Some(0.5));

        let tv = harness.tv.lock().unwrap();
        let kinds: Vec<&str> = tv.received.iter().map(|(_, payload)| payload["type"].as_str().unwrap_or_default()).collect();
        assert!(kinds.contains(&"LAUNCH"));
        let (destination, load) = tv.received.iter().find(|(_, payload)| payload["type"] == "LOAD").unwrap();
        assert_eq!(destination, "transport-1");
        assert_eq!(load["media"]["contentId"], "http://provider/movie/u/p/9.mkv");
        assert_eq!(load["media"]["contentType"], "video/mp4");
        assert_eq!(load["currentTime"], 600.0);
        assert_eq!(kinds.iter().filter(|kind| **kind == "LOAD").count(), 1, "the link must be loaded exactly once");
    }

    #[tokio::test]
    async fn an_already_open_player_is_reused_without_relaunching() {
        let harness = start(FakeTv { player_open: true, ..FakeTv::default() });
        harness.commands.send(film()).await.unwrap();
        wait_for(&harness.shared, |report| report.state == "playing" && report.title.is_some()).await;
        let tv = harness.tv.lock().unwrap();
        assert!(!tv.received.iter().any(|(_, payload)| payload["type"] == "LAUNCH"));
    }

    #[tokio::test]
    async fn pause_and_stop_reach_the_tv() {
        let harness = start(FakeTv::default());
        harness.commands.send(film()).await.unwrap();
        wait_for(&harness.shared, |report| report.state == "playing").await;

        harness.commands.send(Command::Pause).await.unwrap();
        let paused = wait_for(&harness.shared, |report| report.state == "paused").await;
        assert_eq!(paused.position_seconds, 33.0);

        harness.commands.send(Command::Stop).await.unwrap();
        wait_for(&harness.shared, |report| report.state == "idle").await;
        for _ in 0..100 {
            if !harness.tv.lock().unwrap().player_open {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let tv = harness.tv.lock().unwrap();
        assert!(!tv.player_open, "stop must close the player on the TV");
        let (_, pause) = tv.received.iter().find(|(_, payload)| payload["type"] == "PAUSE").unwrap();
        assert_eq!(pause["mediaSessionId"], 4);
    }

    #[tokio::test]
    async fn a_refused_load_reports_a_readable_error() {
        let harness = start(FakeTv { load_outcome: LoadOutcome::Fails, ..FakeTv::default() });
        harness.commands.send(film()).await.unwrap();
        let report = wait_for(&harness.shared, |report| report.state == "error").await;
        assert_eq!(report.error.as_deref(), Some(MSG_UNSUPPORTED));
    }

    #[tokio::test]
    async fn protected_streams_are_refused_before_touching_the_tv() {
        let harness = start(FakeTv::default());
        let protected = Command::Play(Box::new(play_request(json!({
            "v": 1, "src": "http://provider/live/1.mpd", "isLive": true, "drm": {"drmScheme": "widevine", "licenseKey": "k"}
        }))));
        harness.commands.send(protected).await.unwrap();
        let report = wait_for(&harness.shared, |report| report.state == "error").await;
        assert_eq!(report.error.as_deref(), Some(MSG_PROTECTED));
        assert!(!harness.tv.lock().unwrap().received.iter().any(|(_, payload)| payload["type"] == "LOAD"));
    }

    #[tokio::test]
    async fn a_tv_that_cannot_be_reached_reports_unreachable() {
        let shared = SessionShared::new();
        let (commands, receiver) = mpsc::channel(8);
        let refused: Connector = Arc::new(|_host: String, _port: u16| -> ConnectFuture {
            Box::pin(async {
                Err::<(BoxedReader, BoxedWriter), _>(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused"))
            })
        });
        tokio::spawn(run(target(), shared.clone(), receiver, refused, None));
        commands.send(film()).await.unwrap();
        let report = wait_for(&shared, |report| report.state == "error").await;
        assert_eq!(report.error.as_deref(), Some(MSG_UNREACHABLE));
    }

    #[tokio::test]
    async fn a_player_found_already_playing_is_picked_up_on_start() {
        let harness = start(FakeTv { player_open: true, ..FakeTv::default() });
        harness.shared.wait_ready(Duration::from_secs(2)).await;
        let report = harness.shared.snapshot();
        assert_eq!(report.state, "playing");
        assert!(report.position_seconds >= 900.0);
        assert_eq!(report.duration_seconds, Some(5400.0));
    }
}

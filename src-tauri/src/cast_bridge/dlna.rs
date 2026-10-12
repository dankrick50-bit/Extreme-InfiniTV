// DLNA driver for smart TVs (Samsung, LG, Sony and most others): hands the TV a link over UPnP
// and polls it for progress, since these TVs do not push their state.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::session::{describe_source, url_extension, Command, DlnaEndpoints, PlayRequest, Refresher, SessionShared, Target};
use super::xml::{escape, tag_text};

const TICK: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// A TV may sit in "stopped" for a while before a new link starts; past this it is a failure.
const START_GRACE: Duration = Duration::from_secs(20);
const MAX_POLL_FAILURES: u32 = 5;
const END_MARGIN_SECONDS: f64 = 15.0;
/// A reachability answer this fresh is reused rather than asking the TV again.
const PROBE_REUSE: Duration = Duration::from_secs(5);

const MSG_UNREACHABLE: &str = "Could not reach the TV. Check that it is switched on and on the same Wi-Fi.";
const MSG_REFUSED: &str = "The TV refused the video. Its format may not be supported.";
const MSG_NOT_STARTED: &str = "The TV did not start playing. The video format may not be supported.";
const MSG_LOST: &str = "Lost the connection to the TV.";
const MSG_PROTECTED: &str = "Protected channels cannot be sent to this TV.";

#[derive(Debug, PartialEq)]
enum SoapError {
    /// The TV never answered: off, asleep, or at a different address now.
    Unreachable,
    /// The TV answered but declined, with its UPnP error code when it gave one.
    Fault(Option<u32>),
}

fn soap_envelope(service_type: &str, action: &str, arguments: &[(&str, &str)]) -> String {
    let mut body = String::new();
    for (name, value) in arguments {
        body.push_str(&format!("<{name}>{}</{name}>", escape(value)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
<s:Body><u:{action} xmlns:u=\"{service_type}\">{body}</u:{action}></s:Body></s:Envelope>"
    )
}

async fn soap_call(
    client: &reqwest::Client,
    control_url: &str,
    service_type: &str,
    action: &str,
    arguments: &[(&str, &str)],
    limit: Duration,
) -> Result<String, SoapError> {
    let response = client
        .post(control_url)
        .timeout(limit)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .header("SOAPACTION", format!("\"{service_type}#{action}\""))
        .body(soap_envelope(service_type, action, arguments))
        .send()
        .await
        .map_err(|_| SoapError::Unreachable)?;
    let succeeded = response.status().is_success();
    let text = response.text().await.unwrap_or_default();
    if succeeded {
        Ok(text)
    } else {
        Err(SoapError::Fault(tag_text(&text, "errorCode").and_then(|code| code.parse().ok())))
    }
}

/// "1:02:03" or "1:02:03.500"; TVs answer NOT_IMPLEMENTED or all zeros when they have no value.
fn parse_clock(text: &str) -> Option<f64> {
    let mut parts = text.trim().split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || hours < 0.0 || minutes < 0.0 || seconds < 0.0 {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn format_clock(seconds: f64) -> String {
    let total = if seconds.is_finite() { seconds.max(0.0) as u64 } else { 0 };
    format!("{}:{:02}:{:02}", total / 3600, (total / 60) % 60, total % 60)
}

const MAX_ACCEPTED_MIMES: usize = 48;
const TRANSPORT_STREAM_MIMES: &[&str] = &["video/mpeg", "video/vnd.dlna.mpeg-tts", "video/mp2t"];

/// The formats in a TV's "Sink" list, which reads `http-get:*:video/mp4:DLNA.ORG_PN=...,...`.
fn parse_sink_mimes(sink: &str) -> Vec<String> {
    let mut mimes: Vec<String> = sink
        .split(',')
        .filter_map(|entry| entry.split(':').nth(2))
        .map(|mime| mime.trim().to_ascii_lowercase())
        .filter(|mime| !mime.is_empty() && mime != "*")
        .collect();
    mimes.sort();
    mimes.dedup();
    mimes
}

/// Asks a renderer which formats it accepts; `None` when it would not say.
pub(super) async fn accepted_mimes(client: &reqwest::Client, control_url: &str, service_type: &str) -> Option<Vec<String>> {
    let reply = soap_call(client, control_url, service_type, "GetProtocolInfo", &[], PROBE_TIMEOUT).await.ok()?;
    Some(parse_sink_mimes(&tag_text(&reply, "Sink").unwrap_or_default()))
}

/// Soundbars and speakers answer as renderers too: they list formats, but none of them video.
pub(super) fn is_audio_only(mimes: &[String]) -> bool {
    !mimes.is_empty() && !mimes.iter().any(|mime| mime.starts_with("video/"))
}

/// The part of the list worth keeping with a saved TV.
pub(super) fn video_mimes(mimes: Vec<String>) -> Vec<String> {
    mimes.into_iter().filter(|mime| mime.starts_with("video/") || mime.contains("mpegurl")).take(MAX_ACCEPTED_MIMES).collect()
}

/// The label a video is sent under, which the TV matches against the formats it accepts. The
/// same file goes by different names per brand, so the TV's own list decides when there is one.
fn dlna_mime(request: &PlayRequest, accepted: &[String], manufacturer: Option<&str>) -> &'static str {
    let samsung = manufacturer.is_some_and(|manufacturer| manufacturer.to_ascii_lowercase().contains("samsung"));
    let candidates: &[&'static str] = match url_extension(&request.src).as_deref() {
        Some("mp4" | "m4v" | "mov") => &["video/mp4"],
        // Samsung only knows Matroska by its own name; without a list to go by, trust the brand.
        Some("mkv") if samsung => &["video/x-mkv", "video/x-matroska"],
        Some("mkv") => &["video/x-matroska", "video/x-mkv"],
        Some("avi") => &["video/x-msvideo", "video/avi", "video/x-avi"],
        Some("ts" | "mpg" | "mpeg") => TRANSPORT_STREAM_MIMES,
        Some("m3u8") => &["application/vnd.apple.mpegurl", "application/x-mpegurl"],
        Some("webm") => &["video/webm"],
        Some("mp3") => &["audio/mpeg"],
        // Providers serve an extensionless live link as a raw transport stream.
        _ if request.is_live => TRANSPORT_STREAM_MIMES,
        _ => &["video/mp4"],
    };
    candidates.iter().copied().find(|candidate| accepted.iter().any(|mime| mime == candidate)).unwrap_or(candidates[0])
}

/// The item description TVs expect alongside the link; several refuse a bare link without it.
fn didl_metadata(request: &PlayRequest, mime: &str) -> String {
    // OP=01 tells the TV it may seek by byte range; a live stream has nothing to seek in.
    let seekable = if request.is_live { "00" } else { "01" };
    let title = match request.title_text() {
        title if title.trim().is_empty() => "Video".to_string(),
        title => title,
    };
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><item id=\"0\" parentID=\"-1\" restricted=\"1\">\
<dc:title>{}</dc:title><upnp:class>object.item.videoItem</upnp:class>\
<res protocolInfo=\"http-get:*:{mime}:DLNA.ORG_OP={seekable};DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000\">{}</res>\
</item></DIDL-Lite>",
        escape(&title),
        escape(&request.src)
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TransportState {
    Playing,
    Paused,
    Transitioning,
    Stopped,
}

fn parse_transport_state(text: &str) -> Option<TransportState> {
    match text.trim() {
        "PLAYING" => Some(TransportState::Playing),
        "PAUSED_PLAYBACK" => Some(TransportState::Paused),
        "TRANSITIONING" => Some(TransportState::Transitioning),
        "STOPPED" | "NO_MEDIA_PRESENT" => Some(TransportState::Stopped),
        _ => None,
    }
}

/// A stop counts as "finished" only when the film had all but reached its end.
fn stopped_near_end(position: f64, duration: Option<f64>) -> bool {
    duration.is_some_and(|duration| duration > 0.0 && position >= duration - END_MARGIN_SECONDS.max(duration * 0.02))
}

struct Driver {
    target: Target,
    endpoints: DlnaEndpoints,
    shared: Arc<SessionShared>,
    client: reqwest::Client,
    refresher: Option<Refresher>,
    active: bool,
    seen_playing: bool,
    started_at: Instant,
    pending_seek: Option<f64>,
    poll_failures: u32,
    position: f64,
    duration: Option<f64>,
    is_live: bool,
    last_probe: Option<Instant>,
}

impl Driver {
    async fn transport(&self, action: &str, arguments: &[(&str, &str)], limit: Duration) -> Result<String, SoapError> {
        let mut all = vec![("InstanceID", "0")];
        all.extend_from_slice(arguments);
        soap_call(&self.client, &self.endpoints.av_transport_url, &self.endpoints.av_transport_type, action, &all, limit).await
    }

    async fn rendering(&self, action: &str, arguments: &[(&str, &str)]) -> Result<String, SoapError> {
        let (Some(url), Some(service_type)) = (&self.endpoints.rendering_control_url, &self.endpoints.rendering_control_type) else {
            return Err(SoapError::Fault(None));
        };
        let mut all = vec![("InstanceID", "0"), ("Channel", "Master")];
        all.extend_from_slice(arguments);
        soap_call(&self.client, url, service_type, action, &all, REQUEST_TIMEOUT).await
    }

    fn fail(&mut self, message: &str) {
        log::warn!("[cast-bridge] dlna {}: {message}", self.target.name);
        self.active = false;
        self.pending_seek = None;
        self.shared.fail(message);
    }

    fn publish(&self, state: &str) {
        let position = self.position;
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

    async fn read_volume(&self) {
        let Ok(reply) = self.rendering("GetVolume", &[]).await else {
            return;
        };
        let level = tag_text(&reply, "CurrentVolume").and_then(|level| level.parse::<f64>().ok()).map(|level| level / 100.0);
        let muted = match self.rendering("GetMute", &[]).await {
            Ok(reply) => tag_text(&reply, "CurrentMute").map(|muted| muted == "1" || muted.eq_ignore_ascii_case("true")),
            Err(_) => None,
        };
        if level.is_some() {
            self.shared.set_volume(level, Some(muted.unwrap_or(false)));
        }
    }

    async fn read_position(&mut self) {
        let Ok(reply) = self.transport("GetPositionInfo", &[], REQUEST_TIMEOUT).await else {
            return;
        };
        if let Some(position) = tag_text(&reply, "RelTime").and_then(|text| parse_clock(&text)) {
            self.position = position;
        }
        if !self.is_live {
            if let Some(duration) = tag_text(&reply, "TrackDuration").and_then(|text| parse_clock(&text)).filter(|duration| *duration > 0.0) {
                self.duration = Some(duration);
            }
        }
    }

    /// Answers "is the TV there?" for the sender's online check. A fault still means it answered.
    async fn check_reachable(&mut self) {
        if self.last_probe.is_some_and(|probe| probe.elapsed() < PROBE_REUSE) {
            return;
        }
        let outcome = self.transport("GetTransportInfo", &[], PROBE_TIMEOUT).await;
        self.last_probe = Some(Instant::now());
        self.shared.set_reachable(outcome != Err(SoapError::Unreachable));
    }

    /// Picks up a film this app started before it was closed.
    async fn probe_existing(&mut self) {
        let probe = self.transport("GetTransportInfo", &[], PROBE_TIMEOUT).await;
        self.last_probe = Some(Instant::now());
        self.shared.set_reachable(probe != Err(SoapError::Unreachable));
        if let Ok(reply) = probe {
            let state = tag_text(&reply, "CurrentTransportState").and_then(|state| parse_transport_state(&state));
            if matches!(state, Some(TransportState::Playing | TransportState::Paused)) {
                self.active = true;
                self.seen_playing = true;
                self.read_position().await;
                self.publish(if state == Some(TransportState::Playing) { "playing" } else { "paused" });
            }
            self.read_volume().await;
        }
        self.shared.mark_ready();
    }

    async fn hand_over(&self, request: &PlayRequest) -> Result<(), SoapError> {
        let mime = dlna_mime(request, &self.endpoints.accepted_mimes, self.target.manufacturer.as_deref());
        let metadata = didl_metadata(request, mime);
        // Clears whatever was on screen; a TV with nothing loaded answers with a fault, which is fine.
        if self.transport("Stop", &[], REQUEST_TIMEOUT).await == Err(SoapError::Unreachable) {
            return Err(SoapError::Unreachable);
        }
        self.transport("SetAVTransportURI", &[("CurrentURI", &request.src), ("CurrentURIMetaData", &metadata)], REQUEST_TIMEOUT)
            .await?;
        match self.transport("Play", &[("Speed", "1")], REQUEST_TIMEOUT).await {
            // Some TVs are still loading the link when Play arrives; one more try settles it.
            Err(SoapError::Fault(_)) => {
                tokio::time::sleep(Duration::from_millis(800)).await;
                self.transport("Play", &[("Speed", "1")], REQUEST_TIMEOUT).await.map(|_| ())
            }
            other => other.map(|_| ()),
        }
    }

    async fn start_play(&mut self, request: PlayRequest) {
        log::info!("[cast-bridge] dlna {} play {}", self.target.name, describe_source(&request));
        self.active = false;
        self.seen_playing = false;
        self.poll_failures = 0;
        self.position = 0.0;
        self.is_live = request.is_live;
        self.duration = request.known_duration();
        self.pending_seek = None;
        self.shared.begin_loading(request.title.clone(), request.known_duration());
        if request.is_protected() {
            self.fail(MSG_PROTECTED);
            return;
        }

        let mut outcome = self.hand_over(&request).await;
        if outcome == Err(SoapError::Unreachable) {
            // TVs move: a new address from the router, or a new control port after a restart.
            if let Some(updated) = self.refresh().await {
                log::info!("[cast-bridge] dlna {} moved to {}:{}", updated.name, updated.host, updated.port);
                outcome = self.hand_over(&request).await;
            }
        }
        self.shared.set_reachable(outcome != Err(SoapError::Unreachable));
        match outcome {
            Ok(()) => {
                self.active = true;
                self.started_at = Instant::now();
                self.pending_seek = request.resume_position();
            }
            Err(SoapError::Unreachable) => self.fail(MSG_UNREACHABLE),
            Err(SoapError::Fault(code)) => {
                log::warn!("[cast-bridge] dlna {} fault code {code:?}", self.target.name);
                self.fail(MSG_REFUSED);
            }
        }
    }

    async fn refresh(&mut self) -> Option<Target> {
        let refresher = self.refresher.clone()?;
        let updated = refresher().await?;
        let endpoints = updated.dlna.clone()?;
        self.endpoints = endpoints;
        self.target = updated.clone();
        Some(updated)
    }

    async fn poll(&mut self) {
        let reply = match self.transport("GetTransportInfo", &[], REQUEST_TIMEOUT).await {
            Ok(reply) => reply,
            Err(_) => {
                self.poll_failures += 1;
                if self.poll_failures >= MAX_POLL_FAILURES {
                    self.fail(MSG_LOST);
                }
                return;
            }
        };
        self.poll_failures = 0;
        // The TV saying outright that it could not open the link beats waiting out the grace period.
        let gave_up = tag_text(&reply, "CurrentTransportStatus").is_some_and(|status| status == "ERROR_OCCURRED");
        if gave_up && !self.seen_playing {
            self.fail(MSG_NOT_STARTED);
            return;
        }
        let Some(state) = tag_text(&reply, "CurrentTransportState").and_then(|state| parse_transport_state(&state)) else {
            return;
        };
        match state {
            TransportState::Playing => {
                self.seen_playing = true;
                if let Some(seconds) = self.pending_seek.take() {
                    let _ = self.transport("Seek", &[("Unit", "REL_TIME"), ("Target", &format_clock(seconds))], REQUEST_TIMEOUT).await;
                    self.position = seconds;
                } else {
                    self.read_position().await;
                }
                self.publish("playing");
            }
            TransportState::Paused => {
                self.seen_playing = true;
                self.read_position().await;
                self.publish("paused");
            }
            TransportState::Transitioning => self.publish(if self.seen_playing { "buffering" } else { "loading" }),
            TransportState::Stopped if !self.seen_playing => {
                if self.started_at.elapsed() >= START_GRACE {
                    self.fail(MSG_NOT_STARTED);
                }
            }
            TransportState::Stopped => {
                self.active = false;
                if stopped_near_end(self.position, self.duration) {
                    self.position = self.duration.unwrap_or(self.position);
                    self.publish("ended");
                } else {
                    self.shared.set_idle();
                }
            }
        }
    }

    async fn on_command(&mut self, command: Command) {
        match command {
            Command::Play(request) => self.start_play(*request).await,
            Command::Pause => {
                if self.transport("Pause", &[], REQUEST_TIMEOUT).await.is_ok() && self.active {
                    self.publish("paused");
                }
            }
            Command::Resume => {
                let _ = self.transport("Play", &[("Speed", "1")], REQUEST_TIMEOUT).await;
            }
            Command::Seek(seconds) => {
                self.pending_seek = None;
                let target = format_clock(seconds);
                if self.transport("Seek", &[("Unit", "REL_TIME"), ("Target", &target)], REQUEST_TIMEOUT).await.is_ok() {
                    self.position = seconds.max(0.0);
                    let position = self.position;
                    self.shared.update(|report| report.position_seconds = position);
                }
            }
            Command::Stop => {
                self.active = false;
                self.pending_seek = None;
                self.shared.set_idle();
                let _ = self.transport("Stop", &[], REQUEST_TIMEOUT).await;
            }
            Command::Volume { level, muted } => {
                let level = level.clamp(0.0, 1.0);
                let percent = ((level * 100.0).round() as u32).to_string();
                if self.rendering("SetVolume", &[("DesiredVolume", &percent)]).await.is_ok() {
                    let _ = self.rendering("SetMute", &[("DesiredMute", if muted { "1" } else { "0" })]).await;
                    self.shared.set_volume(Some(level), Some(muted));
                }
            }
            Command::Probe(done) => {
                self.check_reachable().await;
                let _ = done.send(());
            }
        }
    }
}

pub async fn run(target: Target, shared: Arc<SessionShared>, mut commands: mpsc::Receiver<Command>, refresher: Option<Refresher>) {
    let Some(endpoints) = target.dlna.clone() else {
        shared.mark_ready();
        return;
    };
    let Ok(client) = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
    else {
        shared.mark_ready();
        return;
    };
    let mut driver = Driver {
        target,
        endpoints,
        shared,
        client,
        refresher,
        active: false,
        seen_playing: false,
        started_at: Instant::now(),
        pending_seek: None,
        poll_failures: 0,
        position: 0.0,
        duration: None,
        is_live: false,
        last_probe: None,
    };
    driver.probe_existing().await;

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(command) => driver.on_command(command).await,
                None => break,
            },
            _ = ticker.tick() => {
                if driver.active {
                    driver.poll().await;
                }
            }
        }
    }
}

#[cfg(test)]
pub(super) mod test_support {
    //! A scripted DLNA TV: records what it is asked and answers like a real renderer.

    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;

    use crate::cast_bridge::session::{DlnaEndpoints, Target, TargetKind};
    use crate::cast_bridge::xml::tag_text;

    #[derive(Default)]
    pub struct FakeTv {
        pub transport_state: String,
        pub position: String,
        pub duration: String,
        pub volume: u32,
        pub refuse_uri: bool,
        /// Accepts the link, then reports that it could not open it.
        pub fail_to_open: bool,
        pub current_uri: Option<String>,
        pub metadata: Option<String>,
        pub actions: Vec<String>,
        pub seek_target: Option<String>,
    }

    pub type SharedTv = Arc<Mutex<FakeTv>>;

    fn reply(action: &str, body: &str) -> (StatusCode, String) {
        (
            StatusCode::OK,
            format!("<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:{action}Response xmlns:u=\"urn:x\">{body}</u:{action}Response></s:Body></s:Envelope>"),
        )
    }

    fn fault(code: u32) -> (StatusCode, String) {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("<s:Envelope><s:Body><s:Fault><detail><UPnPError><errorCode>{code}</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>"),
        )
    }

    async fn control(State(tv): State<SharedTv>, headers: HeaderMap, body: String) -> (StatusCode, String) {
        let action = headers
            .get("SOAPACTION")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim_matches('"').rsplit('#').next())
            .unwrap_or_default()
            .to_string();
        let mut tv = tv.lock().unwrap();
        tv.actions.push(action.clone());
        match action.as_str() {
            "Stop" => {
                tv.transport_state = "STOPPED".to_string();
                reply(&action, "")
            }
            "SetAVTransportURI" => {
                if tv.refuse_uri {
                    return fault(714);
                }
                tv.current_uri = tag_text(&body, "CurrentURI");
                tv.metadata = tag_text(&body, "CurrentURIMetaData");
                reply(&action, "")
            }
            "Play" => {
                tv.transport_state = if tv.fail_to_open { "STOPPED" } else { "PLAYING" }.to_string();
                reply(&action, "")
            }
            "Pause" => {
                tv.transport_state = "PAUSED_PLAYBACK".to_string();
                reply(&action, "")
            }
            "Seek" => {
                tv.seek_target = tag_text(&body, "Target");
                reply(&action, "")
            }
            "GetTransportInfo" => {
                let state = tv.transport_state.clone();
                let status = if tv.fail_to_open && tv.current_uri.is_some() { "ERROR_OCCURRED" } else { "OK" };
                reply(&action, &format!("<CurrentTransportState>{state}</CurrentTransportState><CurrentTransportStatus>{status}</CurrentTransportStatus>"))
            }
            "GetPositionInfo" => {
                let (position, duration) = (tv.position.clone(), tv.duration.clone());
                reply(&action, &format!("<Track>1</Track><TrackDuration>{duration}</TrackDuration><RelTime>{position}</RelTime>"))
            }
            "GetVolume" => {
                let volume = tv.volume;
                reply(&action, &format!("<CurrentVolume>{volume}</CurrentVolume>"))
            }
            "GetMute" => reply(&action, "<CurrentMute>0</CurrentMute>"),
            "SetVolume" => {
                tv.volume = tag_text(&body, "DesiredVolume").and_then(|volume| volume.parse().ok()).unwrap_or(0);
                reply(&action, "")
            }
            "SetMute" => reply(&action, ""),
            _ => fault(401),
        }
    }

    /// Starts the fake TV on a free local port and returns a target pointing at it.
    pub async fn start_fake_tv(tv: FakeTv) -> (Target, SharedTv) {
        let tv = Arc::new(Mutex::new(tv));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let router = axum::Router::new().route("/control", post(control)).with_state(tv.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let target = Target {
            id: "ext-dlna-test".to_string(),
            kind: TargetKind::Dlna,
            name: "Bedroom TV".to_string(),
            host: "127.0.0.1".to_string(),
            port,
            key: "d".repeat(32),
            uid: None,
            manufacturer: Some("LG Electronics".to_string()),
            dlna: Some(DlnaEndpoints {
                av_transport_url: format!("http://127.0.0.1:{port}/control"),
                av_transport_type: "urn:schemas-upnp-org:service:AVTransport:1".to_string(),
                rendering_control_url: Some(format!("http://127.0.0.1:{port}/control")),
                rendering_control_type: Some("urn:schemas-upnp-org:service:RenderingControl:1".to_string()),
                accepted_mimes: Vec::new(),
            }),
        };
        (target, tv)
    }

    pub fn idle_tv() -> FakeTv {
        FakeTv {
            transport_state: "NO_MEDIA_PRESENT".to_string(),
            position: "0:00:00".to_string(),
            duration: "0:00:00".to_string(),
            volume: 30,
            ..FakeTv::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{idle_tv, start_fake_tv, FakeTv, SharedTv};
    use super::*;
    use crate::cast_bridge::session::Report;
    use serde_json::json;

    fn play_request(json: serde_json::Value) -> PlayRequest {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn clock_values_parse_and_format() {
        assert_eq!(parse_clock("1:02:03"), Some(3723.0));
        assert_eq!(parse_clock("0:00:07.500"), Some(7.5));
        assert_eq!(parse_clock("NOT_IMPLEMENTED"), None);
        assert_eq!(parse_clock("1:02"), None);
        assert_eq!(format_clock(3723.9), "1:02:03");
        assert_eq!(format_clock(-5.0), "0:00:00");
        assert_eq!(format_clock(f64::NAN), "0:00:00");
    }

    #[test]
    fn mime_follows_the_link_and_the_tv_brand() {
        let mkv = play_request(json!({"v": 1, "src": "http://h/movie/u/p/1.mkv", "isLive": false}));
        assert_eq!(dlna_mime(&mkv, &[], Some("Samsung Electronics")), "video/x-mkv");
        assert_eq!(dlna_mime(&mkv, &[], Some("LG Electronics")), "video/x-matroska");
        let live = play_request(json!({"v": 1, "src": "http://h/live/u/p/1.ts", "mime": "application/x-mpegURL", "isLive": true}));
        assert_eq!(dlna_mime(&live, &[], None), "video/mpeg");
        let bare_live = play_request(json!({"v": 1, "src": "http://h/u/p/1", "isLive": true}));
        assert_eq!(dlna_mime(&bare_live, &[], None), "video/mpeg");
        let bare_vod = play_request(json!({"v": 1, "src": "http://h/u/p/1", "isLive": false}));
        assert_eq!(dlna_mime(&bare_vod, &[], None), "video/mp4");
    }

    #[test]
    fn the_tvs_own_format_list_decides_the_label() {
        let mkv = play_request(json!({"v": 1, "src": "http://h/movie/u/p/1.mkv", "isLive": false}));
        let live = play_request(json!({"v": 1, "src": "http://h/live/u/p/1.ts", "isLive": true}));
        // A TV of unknown brand that, like Samsung, lists Matroska under its other name.
        let lists_x_mkv = vec!["video/mp4".to_string(), "video/x-mkv".to_string(), "video/vnd.dlna.mpeg-tts".to_string()];
        assert_eq!(dlna_mime(&mkv, &lists_x_mkv, None), "video/x-mkv");
        assert_eq!(dlna_mime(&live, &lists_x_mkv, None), "video/vnd.dlna.mpeg-tts");
        // Nothing on the list fits: fall back to the usual label rather than refuse to try.
        let lists_only_mp4 = vec!["video/mp4".to_string()];
        assert_eq!(dlna_mime(&mkv, &lists_only_mp4, None), "video/x-matroska");
    }

    #[test]
    fn a_sink_list_reduces_to_its_formats() {
        // Shape taken from a 2023 Samsung QLED's real answer.
        let sink = "http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_TS_HD_NA_ISO;DLNA.ORG_FLAGS=ED100000000000000000000000000000,\
http-get:*:video/mpeg:DLNA.ORG_PN=MPEG_PS_NTSC;DLNA.ORG_FLAGS=ED100000000000000000000000000000,\
http-get:*:video/x-mkv:*,http-get:*:Video/MP4:DLNA.ORG_PN=AVC_MP4_BL_CIF15_AAC_520,http-get:*:audio/mpeg:DLNA.ORG_PN=MP3,http-get:*:*:*";
        let mimes = parse_sink_mimes(sink);
        assert_eq!(mimes, ["audio/mpeg", "video/mp4", "video/mpeg", "video/x-mkv"]);
        assert!(!is_audio_only(&mimes));
        assert_eq!(video_mimes(mimes), ["video/mp4", "video/mpeg", "video/x-mkv"]);
    }

    #[test]
    fn a_renderer_that_lists_no_video_is_a_speaker() {
        let soundbar = parse_sink_mimes("http-get:*:audio/mpeg:*,http-get:*:audio/flac:*,http-get:*:audio/x-wav:*");
        assert!(is_audio_only(&soundbar));
        // A TV that would not say is given the benefit of the doubt.
        assert!(!is_audio_only(&[]));
    }

    #[test]
    fn metadata_escapes_the_title_and_link() {
        let request = play_request(json!({"v": 1, "src": "http://h/movie/u/p/1.mp4?a=1&b=2", "isLive": false, "title": "Tom & Jerry <HD>"}));
        let metadata = didl_metadata(&request, "video/mp4");
        assert!(metadata.contains("<dc:title>Tom &amp; Jerry &lt;HD&gt;</dc:title>"));
        assert!(metadata.contains(">http://h/movie/u/p/1.mp4?a=1&amp;b=2</res>"));
        assert!(metadata.contains("http-get:*:video/mp4:DLNA.ORG_OP=01;"));
        let live = play_request(json!({"v": 1, "src": "http://h/live/1.ts", "isLive": true}));
        assert!(didl_metadata(&live, "video/mpeg").contains("DLNA.ORG_OP=00;"));
    }

    #[test]
    fn envelope_escapes_arguments() {
        let envelope = soap_envelope("urn:schemas-upnp-org:service:AVTransport:1", "SetAVTransportURI", &[("CurrentURI", "http://h/1?a=1&b=2")]);
        assert!(envelope.contains("<u:SetAVTransportURI xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">"));
        assert!(envelope.contains("<CurrentURI>http://h/1?a=1&amp;b=2</CurrentURI>"));
    }

    #[test]
    fn a_stop_only_counts_as_finished_near_the_end() {
        assert!(stopped_near_end(5395.0, Some(5400.0)));
        assert!(!stopped_near_end(1200.0, Some(5400.0)));
        assert!(!stopped_near_end(1200.0, None));
        // A long film gets a proportionally wider margin for TVs that stop a little early.
        assert!(stopped_near_end(10_000.0, Some(10_150.0)));
    }

    struct Harness {
        shared: Arc<SessionShared>,
        commands: mpsc::Sender<Command>,
        tv: SharedTv,
    }

    async fn start(tv: FakeTv) -> Harness {
        let (target, tv) = start_fake_tv(tv).await;
        let shared = SessionShared::new();
        let (commands, receiver) = mpsc::channel(8);
        tokio::spawn(run(target, shared.clone(), receiver, None));
        Harness { shared, commands, tv }
    }

    async fn wait_for(shared: &SessionShared, accept: impl Fn(&Report) -> bool) -> Report {
        for _ in 0..500 {
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
            "v": 1, "src": "http://provider/movie/u/p/9.mkv?t=1&u=2", "isLive": false, "title": "A Film", "resumeSeconds": 600.0
        }))))
    }

    #[tokio::test]
    async fn play_hands_the_tv_the_link_and_reports_progress() {
        let harness = start(FakeTv { position: "0:00:42".to_string(), duration: "1:30:00".to_string(), ..idle_tv() }).await;
        harness.commands.send(film()).await.unwrap();

        let report = wait_for(&harness.shared, |report| report.state == "playing").await;
        assert_eq!(report.title.as_deref(), Some("A Film"));
        assert_eq!(report.volume, Some(0.3));

        let tv = harness.tv.lock().unwrap();
        assert_eq!(tv.current_uri.as_deref(), Some("http://provider/movie/u/p/9.mkv?t=1&u=2"));
        let metadata = tv.metadata.clone().unwrap();
        assert!(metadata.contains("video/x-matroska"), "an LG TV gets the standard Matroska label");
        assert!(metadata.contains("<dc:title>A Film</dc:title>"));
        let order: Vec<&str> = tv.actions.iter().map(String::as_str).filter(|action| matches!(*action, "Stop" | "SetAVTransportURI" | "Play")).collect();
        assert_eq!(&order[..3], ["Stop", "SetAVTransportURI", "Play"]);
        assert_eq!(tv.seek_target.as_deref(), Some("0:10:00"), "the film resumes where it was left");
    }

    #[tokio::test]
    async fn pause_seek_volume_and_stop_reach_the_tv() {
        let harness = start(FakeTv { position: "0:20:00".to_string(), duration: "1:30:00".to_string(), ..idle_tv() }).await;
        let fresh = Command::Play(Box::new(play_request(json!({"v": 1, "src": "http://provider/movie/u/p/9.mp4", "isLive": false, "title": "A Film"}))));
        harness.commands.send(fresh).await.unwrap();
        let playing = wait_for(&harness.shared, |report| report.state == "playing" && report.position_seconds > 0.0).await;
        assert_eq!(playing.position_seconds, 1200.0);
        assert_eq!(playing.duration_seconds, Some(5400.0));

        harness.commands.send(Command::Pause).await.unwrap();
        wait_for(&harness.shared, |report| report.state == "paused").await;

        harness.commands.send(Command::Seek(125.0)).await.unwrap();
        harness.commands.send(Command::Volume { level: 0.55, muted: false }).await.unwrap();
        wait_for(&harness.shared, |report| report.volume == Some(0.55)).await;
        {
            let tv = harness.tv.lock().unwrap();
            assert_eq!(tv.seek_target.as_deref(), Some("0:02:05"));
            assert_eq!(tv.volume, 55);
        }

        harness.commands.send(Command::Stop).await.unwrap();
        let idle = wait_for(&harness.shared, |report| report.state == "idle").await;
        assert_eq!(idle.volume, Some(0.55));
        for _ in 0..100 {
            if harness.tv.lock().unwrap().transport_state == "STOPPED" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("stop never reached the tv");
    }

    #[tokio::test]
    async fn a_film_that_runs_out_reports_ended() {
        let harness = start(FakeTv { position: "1:29:55".to_string(), duration: "1:30:00".to_string(), ..idle_tv() }).await;
        let fresh = Command::Play(Box::new(play_request(json!({"v": 1, "src": "http://provider/movie/u/p/9.mp4", "isLive": false}))));
        harness.commands.send(fresh).await.unwrap();
        wait_for(&harness.shared, |report| report.state == "playing" && report.position_seconds > 5000.0).await;

        harness.tv.lock().unwrap().transport_state = "STOPPED".to_string();
        let report = wait_for(&harness.shared, |report| report.state == "ended").await;
        assert_eq!(report.position_seconds, 5400.0);
    }

    #[tokio::test]
    async fn a_refused_link_reports_a_readable_error() {
        let harness = start(FakeTv { refuse_uri: true, ..idle_tv() }).await;
        harness.commands.send(film()).await.unwrap();
        let report = wait_for(&harness.shared, |report| report.state == "error").await;
        assert_eq!(report.error.as_deref(), Some(MSG_REFUSED));
    }

    #[tokio::test]
    async fn a_tv_that_cannot_open_the_link_says_so_without_the_long_wait() {
        let harness = start(FakeTv { fail_to_open: true, ..idle_tv() }).await;
        let asked_at = std::time::Instant::now();
        harness.commands.send(film()).await.unwrap();
        let report = wait_for(&harness.shared, |report| report.state == "error").await;
        assert_eq!(report.error.as_deref(), Some(MSG_NOT_STARTED));
        assert!(asked_at.elapsed() < START_GRACE / 2, "the TV's own error must not wait for the grace period");
    }

    #[tokio::test]
    async fn a_tv_already_playing_is_picked_up_on_start() {
        let harness = start(FakeTv {
            transport_state: "PLAYING".to_string(),
            position: "0:15:00".to_string(),
            duration: "1:30:00".to_string(),
            volume: 12,
            ..FakeTv::default()
        })
        .await;
        harness.shared.wait_ready(Duration::from_secs(3)).await;
        let report = harness.shared.snapshot();
        assert_eq!(report.state, "playing");
        assert_eq!(report.position_seconds, 900.0);
        assert_eq!(report.volume, Some(0.12));
    }

    #[tokio::test]
    async fn a_tv_that_is_switched_off_reports_unreachable() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (mut target, _tv) = start_fake_tv(idle_tv()).await;
        if let Some(endpoints) = target.dlna.as_mut() {
            endpoints.av_transport_url = format!("http://127.0.0.1:{port}/control");
        }
        let shared = SessionShared::new();
        let (commands, receiver) = mpsc::channel(8);
        tokio::spawn(run(target, shared.clone(), receiver, None));
        commands.send(film()).await.unwrap();
        let report = wait_for(&shared, |report| report.state == "error").await;
        assert_eq!(report.error.as_deref(), Some(MSG_UNREACHABLE));
    }
}

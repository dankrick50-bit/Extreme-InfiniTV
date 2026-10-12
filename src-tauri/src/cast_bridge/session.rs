// Shared plumbing between the bridge's HTTP surface and the per-TV driver tasks.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Chromecast,
    Dlna,
}

/// One saved TV. `key` is the secret the sender presents; it also selects the TV, since every
/// bridged TV shares the bridge's single loopback port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub id: String,
    pub kind: TargetKind,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub key: String,
    /// Stable identity (Cast id or UPnP UDN) used to find the TV again after its address changes.
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub manufacturer: Option<String>,
    #[serde(default)]
    pub dlna: Option<DlnaEndpoints>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DlnaEndpoints {
    pub av_transport_url: String,
    pub av_transport_type: String,
    #[serde(default)]
    pub rendering_control_url: Option<String>,
    #[serde(default)]
    pub rendering_control_type: Option<String>,
    /// Video formats the TV said it accepts (empty when it did not say); picks the label a video is sent under.
    #[serde(default)]
    pub accepted_mimes: Vec<String>,
}

/// The subset of the app's cast descriptor a third-party TV can act on.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayRequest {
    pub v: u32,
    pub src: String,
    pub is_live: bool,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub logo: Option<String>,
    #[serde(default)]
    pub drm: Option<serde_json::Value>,
    #[serde(default)]
    pub resume_seconds: Option<f64>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
}

impl PlayRequest {
    pub fn title_text(&self) -> String {
        self.title.clone().unwrap_or_default()
    }

    /// Where to start a film from; live streams and near-zero offsets start at the beginning.
    pub fn resume_position(&self) -> Option<f64> {
        if self.is_live {
            return None;
        }
        self.resume_seconds.filter(|seconds| seconds.is_finite() && *seconds > 5.0)
    }

    pub fn known_duration(&self) -> Option<f64> {
        if self.is_live {
            return None;
        }
        self.duration_seconds.filter(|seconds| seconds.is_finite() && *seconds > 0.0)
    }

    /// DRM needs a custom player on the TV, which neither Chromecast's stock player nor DLNA has.
    pub fn is_protected(&self) -> bool {
        self.drm.as_ref().is_some_and(|drm| {
            drm.as_object().is_some_and(|fields| fields.values().any(|value| value.as_str().is_some_and(|text| !text.is_empty())))
        })
    }
}

#[derive(Debug)]
pub enum Command {
    Play(Box<PlayRequest>),
    Pause,
    Resume,
    Seek(f64),
    Stop,
    Volume { level: f64, muted: bool },
    /// Checks the TV still answers; the sender is told when the check is done.
    Probe(oneshot::Sender<()>),
}

/// Same wire shape the app's own receiver reports, so the sender's pill and remote work unchanged.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub state: String,
    pub position_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            state: "idle".to_string(),
            position_seconds: 0.0,
            duration_seconds: None,
            title: None,
            error: None,
            volume: None,
            muted: None,
        }
    }
}

pub struct SessionShared {
    report: Mutex<Report>,
    broadcast: broadcast::Sender<String>,
    ready: watch::Sender<bool>,
    reachable: AtomicBool,
}

impl SessionShared {
    pub fn new() -> Arc<Self> {
        let (broadcast, _receiver) = broadcast::channel(16);
        let (ready, _ready_receiver) = watch::channel(false);
        Arc::new(Self { report: Mutex::new(Report::default()), broadcast, ready, reachable: AtomicBool::new(true) })
    }

    pub fn snapshot(&self) -> Report {
        self.report.lock().unwrap_or_else(|poison| poison.into_inner()).clone()
    }

    pub fn snapshot_json(&self) -> String {
        serde_json::to_string(&self.snapshot()).unwrap_or_else(|_| r#"{"state":"idle","positionSeconds":0}"#.to_string())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.broadcast.subscribe()
    }

    /// Applies a change and pushes it to listeners only when something actually changed.
    pub fn update(&self, change: impl FnOnce(&mut Report)) {
        let serialized = {
            let mut report = self.report.lock().unwrap_or_else(|poison| poison.into_inner());
            let before = report.clone();
            change(&mut report);
            if *report == before {
                return;
            }
            serde_json::to_string(&*report).ok()
        };
        if let Some(serialized) = serialized {
            let _ = self.broadcast.send(serialized);
        }
    }

    pub fn begin_loading(&self, title: Option<String>, duration_seconds: Option<f64>) {
        self.update(|report| {
            report.state = "loading".to_string();
            report.position_seconds = 0.0;
            report.duration_seconds = duration_seconds;
            report.title = title;
            report.error = None;
        });
    }

    pub fn fail(&self, message: &str) {
        self.update(|report| {
            report.state = "error".to_string();
            report.position_seconds = 0.0;
            report.error = Some(message.to_string());
        });
    }

    /// Volume survives: it belongs to the TV, not to whatever was playing.
    pub fn set_idle(&self) {
        self.update(|report| {
            *report = Report { volume: report.volume, muted: report.muted, ..Report::default() };
        });
    }

    pub fn set_volume(&self, level: Option<f64>, muted: Option<bool>) {
        self.update(|report| {
            if let Some(level) = level.filter(|level| level.is_finite()) {
                report.volume = Some(level.clamp(0.0, 1.0));
            }
            if let Some(muted) = muted {
                report.muted = Some(muted);
            }
        });
    }

    /// Whether the TV answered the last time the driver tried it.
    pub fn is_reachable(&self) -> bool {
        self.reachable.load(Ordering::Relaxed)
    }

    pub fn set_reachable(&self, reachable: bool) {
        self.reachable.store(reachable, Ordering::Relaxed);
    }

    /// Set once the driver knows whether the TV was already playing when the app found it.
    pub fn mark_ready(&self) {
        self.ready.send_replace(true);
    }

    pub async fn wait_ready(&self, limit: Duration) {
        let mut ready = self.ready.subscribe();
        let _ = tokio::time::timeout(limit, ready.wait_for(|ready| *ready)).await;
    }
}

#[derive(Clone)]
pub struct SessionHandle {
    pub shared: Arc<SessionShared>,
    pub commands: mpsc::Sender<Command>,
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Looks the TV up on the network again; `Some` only when its address or endpoints changed.
pub type Refresher = Arc<dyn Fn() -> BoxFuture<Option<Target>> + Send + Sync>;

/// Lowercased file extension of a URL's path, ignoring any query string.
pub fn url_extension(src: &str) -> Option<String> {
    let without_fragment = src.split('#').next().unwrap_or(src);
    let path = without_fragment.split('?').next().unwrap_or(without_fragment);
    let last_segment = path.rsplit('/').next().unwrap_or(path);
    let (_, extension) = last_segment.rsplit_once('.')?;
    if extension.is_empty() || extension.len() > 5 || !extension.chars().all(|character| character.is_ascii_alphanumeric()) {
        return None;
    }
    Some(extension.to_ascii_lowercase())
}

/// Log-safe summary of a stream: provider URLs carry the account's login in their path.
pub fn describe_source(request: &PlayRequest) -> String {
    let extension = url_extension(&request.src).unwrap_or_else(|| "none".to_string());
    format!("{} ext={extension}", if request.is_live { "live" } else { "vod" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(json: serde_json::Value) -> PlayRequest {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn url_extension_ignores_query_and_fragment() {
        assert_eq!(url_extension("http://host/movie/u/p/12.MKV?token=a.b#t"), Some("mkv".to_string()));
        assert_eq!(url_extension("http://host:8080/live/u/p/55.ts"), Some("ts".to_string()));
    }

    #[test]
    fn url_extension_is_none_for_extensionless_paths() {
        assert_eq!(url_extension("http://host:8080/u/p/55"), None);
        assert_eq!(url_extension("http://host.example.com/"), None);
        assert_eq!(url_extension("http://host/a.b/c"), None);
    }

    #[test]
    fn describe_source_never_contains_the_url() {
        let play = request(serde_json::json!({"v": 1, "src": "http://host/movie/user/secret/12.mkv", "isLive": false}));
        let described = describe_source(&play);
        assert_eq!(described, "vod ext=mkv");
        assert!(!described.contains("secret"));
    }

    #[test]
    fn resume_position_skips_live_and_near_zero_offsets() {
        let vod = request(serde_json::json!({"v": 1, "src": "http://h/1.mp4", "isLive": false, "resumeSeconds": 600.0}));
        assert_eq!(vod.resume_position(), Some(600.0));
        let barely = request(serde_json::json!({"v": 1, "src": "http://h/1.mp4", "isLive": false, "resumeSeconds": 3.0}));
        assert_eq!(barely.resume_position(), None);
        let live = request(serde_json::json!({"v": 1, "src": "http://h/1.ts", "isLive": true, "resumeSeconds": 600.0}));
        assert_eq!(live.resume_position(), None);
    }

    #[test]
    fn is_protected_needs_a_real_drm_value() {
        let clear = request(serde_json::json!({"v": 1, "src": "http://h/1.mpd", "isLive": true, "drm": {"drmScheme": null}}));
        assert!(!clear.is_protected());
        let protected = request(serde_json::json!({"v": 1, "src": "http://h/1.mpd", "isLive": true, "drm": {"drmScheme": "widevine"}}));
        assert!(protected.is_protected());
    }

    #[test]
    fn update_only_broadcasts_real_changes() {
        let shared = SessionShared::new();
        let mut listener = shared.subscribe();
        shared.update(|report| report.state = "idle".to_string());
        assert!(listener.try_recv().is_err());
        shared.update(|report| report.state = "playing".to_string());
        let pushed = listener.try_recv().unwrap();
        assert!(pushed.contains(r#""state":"playing""#));
    }

    #[test]
    fn set_idle_keeps_the_volume() {
        let shared = SessionShared::new();
        shared.set_volume(Some(0.4), Some(false));
        shared.begin_loading(Some("Film".to_string()), Some(90.0));
        shared.set_idle();
        let report = shared.snapshot();
        assert_eq!(report.state, "idle");
        assert_eq!(report.title, None);
        assert_eq!(report.volume, Some(0.4));
    }

    #[test]
    fn report_serializes_in_the_receiver_wire_shape() {
        let shared = SessionShared::new();
        shared.begin_loading(Some("Film".to_string()), Some(90.0));
        let value: serde_json::Value = serde_json::from_str(&shared.snapshot_json()).unwrap();
        assert_eq!(value["state"], "loading");
        assert_eq!(value["positionSeconds"], 0.0);
        assert_eq!(value["durationSeconds"], 90.0);
        assert_eq!(value["title"], "Film");
        assert!(value.get("error").is_none());
    }
}

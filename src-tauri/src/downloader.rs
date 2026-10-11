// Native download engine (desktop + Android). Fetches a file in sequential ranged pieces straight
// into its destination, resuming and retrying on its own, so a download survives page navigation,
// dropped connections and a backgrounded app. Pieces are sequential because IPTV accounts commonly
// allow a single connection.

use std::collections::{HashMap, VecDeque};
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::Notify;

const PROGRESS_EVENT: &str = "xt:dl-progress";
const DONE_EVENT: &str = "xt:dl-done";
const ERROR_EVENT: &str = "xt:dl-error";
const STATUS_FILE_NAME: &str = "downloads-status.json";
const WRITE_BUFFER_BYTES: usize = 256 * 1024;
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_secs(1);
const CANCEL_WAIT: Duration = Duration::from_secs(10);

const MESSAGE_HTML: &str = "The server sent a web page instead of the video. The file may be missing, or a network security filter may be blocking it.";
const MESSAGE_STALLED: &str = "No data arrived for a while.";
const MESSAGE_CLOSED_EARLY: &str = "The connection closed before the file finished.";
const MESSAGE_CHANGED: &str = "The file changed on the server. Remove this download and start it again.";
const MESSAGE_OVERSIZED: &str = "The file on disk is larger than the file on the server. Remove this download and start it again.";

// ---------------------------------------------------------------------------
// Types shared with the frontend
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSpec {
    id: String,
    url: String,
    path: Option<String>,
    /// Android: an already-open read/write descriptor for the destination, owned by the engine from here on.
    fd: Option<i32>,
    title: Option<String>,
    user_agent: Option<String>,
    authorization: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Queued,
    Downloading,
    Retrying,
    Done,
    Error,
}

impl Phase {
    fn is_unfinished(self) -> bool {
        matches!(self, Phase::Queued | Phase::Downloading | Phase::Retrying)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobStatus {
    id: String,
    phase: Phase,
    bytes_done: u64,
    bytes_total: u64,
    attempt: u32,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    limit: usize,
    status_file: Option<String>,
    jobs: Vec<JobStatus>,
}

pub trait DownloadEvents: Send + Sync + 'static {
    fn send(&self, event: &str, payload: Value);
}

impl DownloadEvents for AppHandle {
    fn send(&self, event: &str, payload: Value) {
        let _ = self.emit(event, payload);
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct EngineConfig {
    piece_size: u64,
    stall_timeout: Duration,
    connect_timeout: Duration,
    /// Consecutive failures with no new bytes before a job gives up.
    max_failures: u32,
    backoff_base: Duration,
    backoff_cap: Duration,
    progress_interval: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            piece_size: 16 * 1024 * 1024,
            stall_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(20),
            max_failures: 20,
            backoff_base: Duration::from_secs(2),
            backoff_cap: Duration::from_secs(60),
            progress_interval: Duration::from_millis(250),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Cancel,
    /// Paused by the concurrency limit; the job goes back to the front of the queue.
    Hold,
}

#[derive(Debug, Clone)]
struct Progress {
    phase: Phase,
    bytes_done: u64,
    bytes_total: u64,
    attempt: u32,
    error: Option<String>,
}

struct Job {
    spec: DownloadSpec,
    progress: Mutex<Progress>,
    stop: Mutex<Option<Stop>>,
    stop_signal: Notify,
    /// True once the task has exited and released the destination file.
    idle: AtomicBool,
    idle_signal: Notify,
    handed_file: Mutex<Option<std::fs::File>>,
}

impl Job {
    fn new(spec: DownloadSpec, handed_file: Option<std::fs::File>) -> Self {
        Self {
            spec,
            progress: Mutex::new(Progress {
                phase: Phase::Queued,
                bytes_done: 0,
                bytes_total: 0,
                attempt: 0,
                error: None,
            }),
            stop: Mutex::new(None),
            stop_signal: Notify::new(),
            idle: AtomicBool::new(true),
            idle_signal: Notify::new(),
            handed_file: Mutex::new(handed_file),
        }
    }

    fn update(&self, change: impl FnOnce(&mut Progress)) {
        change(&mut relock(&self.progress));
    }

    fn status(&self) -> JobStatus {
        let progress = relock(&self.progress);
        JobStatus {
            id: self.spec.id.clone(),
            phase: progress.phase,
            bytes_done: progress.bytes_done,
            bytes_total: progress.bytes_total,
            attempt: progress.attempt,
            error: progress.error.clone(),
        }
    }

    fn phase(&self) -> Phase {
        relock(&self.progress).phase
    }

    fn stop_reason(&self) -> Option<Stop> {
        *relock(&self.stop)
    }

    /// A cancel is never downgraded to a hold.
    fn request_stop(&self, reason: Stop) {
        {
            let mut stop = relock(&self.stop);
            if *stop == Some(Stop::Cancel) {
                return;
            }
            *stop = Some(reason);
        }
        self.stop_signal.notify_one();
    }

    /// Resolves only once a stop has really been requested (a stale wake-up permit just loops).
    async fn stopped(&self) {
        loop {
            if self.stop_reason().is_some() {
                return;
            }
            self.stop_signal.notified().await;
        }
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle_signal.notified();
            if self.idle.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

struct Inner {
    jobs: HashMap<String, Arc<Job>>,
    queue: VecDeque<String>,
    /// Start order, oldest first.
    running: Vec<String>,
    limit: usize,
    housekeeping: bool,
}

pub struct Engine {
    config: EngineConfig,
    client: reqwest::Client,
    inner: Mutex<Inner>,
    events: Mutex<Option<Arc<dyn DownloadEvents>>>,
    status_file: Mutex<Option<PathBuf>>,
}

pub struct DownloaderState {
    engine: Arc<Engine>,
}

impl Default for DownloaderState {
    fn default() -> Self {
        Self { engine: Engine::new(EngineConfig::default()) }
    }
}

fn relock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Debug)]
enum FailureKind {
    Fatal,
    Transient,
    Stopped,
}

#[derive(Debug)]
struct Failure {
    kind: FailureKind,
    message: String,
    /// Set when the server itself refused the request, as opposed to the network failing.
    http_status: Option<u16>,
}

impl Failure {
    fn fatal(message: impl Into<String>) -> Self {
        Self { kind: FailureKind::Fatal, message: message.into(), http_status: None }
    }
    fn transient(message: impl Into<String>) -> Self {
        Self { kind: FailureKind::Transient, message: message.into(), http_status: None }
    }
    fn stopped() -> Self {
        Self { kind: FailureKind::Stopped, message: String::new(), http_status: None }
    }
    fn status(kind: FailureKind, status: u16, message: impl Into<String>) -> Self {
        Self { kind, message: message.into(), http_status: Some(status) }
    }
}

enum Piece {
    /// New bytes were written; more may remain.
    Progress(reqwest::Url),
    Empty,
    Complete,
}

struct Sink {
    writer: BufWriter<tokio::fs::File>,
}

impl Sink {
    async fn open(job: &Job) -> std::io::Result<(Self, u64)> {
        let handed = match relock(&job.handed_file).as_ref() {
            Some(file) => Some(file.try_clone()?),
            None => None,
        };
        let mut file = match handed {
            Some(file) => tokio::fs::File::from_std(file),
            None => {
                let path = job.spec.path.as_deref().ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "no destination given")
                })?;
                tokio::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)
                    .await?
            }
        };
        let length = file.seek(SeekFrom::End(0)).await?;
        Ok((Self { writer: BufWriter::with_capacity(WRITE_BUFFER_BYTES, file) }, length))
    }

    async fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes).await
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush().await
    }

    async fn restart(&mut self) -> std::io::Result<()> {
        self.writer.flush().await?;
        let file = self.writer.get_mut();
        file.set_len(0).await?;
        file.seek(SeekFrom::Start(0)).await?;
        Ok(())
    }

    async fn finish(&mut self) -> std::io::Result<u64> {
        self.writer.flush().await?;
        let file = self.writer.get_mut();
        let _ = file.sync_data().await;
        file.seek(SeekFrom::End(0)).await
    }
}

impl Engine {
    fn new(config: EngineConfig) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Arc::new(Self {
            config,
            client,
            inner: Mutex::new(Inner {
                jobs: HashMap::new(),
                queue: VecDeque::new(),
                running: Vec::new(),
                limit: 1,
                housekeeping: false,
            }),
            events: Mutex::new(None),
            status_file: Mutex::new(None),
        })
    }

    fn attach(&self, app: &AppHandle) {
        let mut events = relock(&self.events);
        if events.is_none() {
            *events = Some(Arc::new(app.clone()));
            *relock(&self.status_file) =
                app.path().app_data_dir().ok().map(|dir| dir.join(STATUS_FILE_NAME));
        }
    }

    fn emit(&self, event: &str, payload: Value) {
        let events = relock(&self.events).clone();
        if let Some(events) = events {
            events.send(event, payload);
        }
    }

    fn emit_progress(&self, job: &Job) {
        self.emit(PROGRESS_EVENT, json!(job.status()));
    }

    fn status(&self) -> EngineStatus {
        let inner = relock(&self.inner);
        EngineStatus {
            limit: inner.limit,
            status_file: relock(&self.status_file).as_ref().map(|path| path.to_string_lossy().into_owned()),
            jobs: inner.jobs.values().map(|job| job.status()).collect(),
        }
    }

    /// Idempotent for a job that is already waiting or running; a finished job is replaced.
    fn enqueue(self: &Arc<Self>, spec: DownloadSpec, handed_file: Option<std::fs::File>) -> JobStatus {
        let job = {
            let mut inner = relock(&self.inner);
            if let Some(existing) = inner.jobs.get(&spec.id) {
                if existing.phase().is_unfinished() {
                    return existing.status();
                }
            }
            let job = Arc::new(Job::new(spec, handed_file));
            inner.jobs.insert(job.spec.id.clone(), job.clone());
            inner.queue.push_back(job.spec.id.clone());
            job
        };
        self.pump();
        job.status()
    }

    /// Stops a job and forgets it, returning once its file handle is released. The partial file stays.
    async fn cancel(self: &Arc<Self>, id: &str) {
        let running_job = {
            let mut inner = relock(&self.inner);
            let Some(job) = inner.jobs.get(id).cloned() else { return };
            if inner.running.iter().any(|running| running == id) {
                job.request_stop(Stop::Cancel);
                Some(job)
            } else {
                inner.queue.retain(|queued| queued != id);
                inner.jobs.remove(id);
                None
            }
        };
        if let Some(job) = running_job {
            let _ = tokio::time::timeout(CANCEL_WAIT, job.wait_idle()).await;
        }
        self.pump();
    }

    fn acknowledge(&self, id: &str) {
        let mut inner = relock(&self.inner);
        let finished = inner.jobs.get(id).is_some_and(|job| !job.phase().is_unfinished());
        if finished {
            inner.jobs.remove(id);
        }
    }

    fn set_limit(self: &Arc<Self>, limit: usize) {
        relock(&self.inner).limit = limit;
        self.pump();
    }

    /// Starts waiting jobs up to the limit, and holds the newest running jobs when over it.
    fn pump(self: &Arc<Self>) {
        let mut to_start = Vec::new();
        let mut start_housekeeping = false;
        {
            let mut inner = relock(&self.inner);
            while inner.running.len() < inner.limit {
                let Some(id) = inner.queue.pop_front() else { break };
                let Some(job) = inner.jobs.get(&id).cloned() else { continue };
                inner.running.push(id);
                to_start.push(job);
            }
            if inner.running.len() > inner.limit {
                let excess = inner.running.len() - inner.limit;
                for id in inner.running.iter().rev().take(excess) {
                    if let Some(job) = inner.jobs.get(id) {
                        job.request_stop(Stop::Hold);
                    }
                }
            }
            let unfinished = inner.jobs.values().any(|job| job.phase().is_unfinished());
            if unfinished && !inner.housekeeping {
                inner.housekeeping = true;
                start_housekeeping = true;
            }
        }
        for job in to_start {
            *relock(&job.stop) = None;
            job.idle.store(false, Ordering::SeqCst);
            tauri::async_runtime::spawn(self.clone().run(job));
        }
        if start_housekeeping {
            tauri::async_runtime::spawn(self.clone().housekeeping());
        }
    }

    async fn run(self: Arc<Self>, job: Arc<Job>) {
        job.update(|progress| {
            progress.phase = Phase::Downloading;
            progress.error = None;
            progress.attempt = 0;
        });

        let outcome = self.download(&job).await;

        let id = job.spec.id.clone();
        let mut release_handle = true;
        {
            let mut inner = relock(&self.inner);
            inner.running.retain(|running| running != &id);
            match &outcome {
                Ok(()) => job.update(|progress| {
                    progress.phase = Phase::Done;
                    progress.error = None;
                }),
                Err(failure) if matches!(failure.kind, FailureKind::Stopped) => {
                    if job.stop_reason() == Some(Stop::Hold) {
                        job.update(|progress| progress.phase = Phase::Queued);
                        inner.queue.push_front(id.clone());
                        release_handle = false;
                    } else {
                        inner.jobs.remove(&id);
                    }
                }
                Err(failure) => job.update(|progress| {
                    progress.phase = Phase::Error;
                    progress.error = Some(failure.message.clone());
                }),
            }
        }
        if release_handle {
            relock(&job.handed_file).take();
        }
        job.idle.store(true, Ordering::SeqCst);
        job.idle_signal.notify_waiters();

        match job.phase() {
            Phase::Done => self.emit(DONE_EVENT, json!(job.status())),
            Phase::Error => self.emit(ERROR_EVENT, json!(job.status())),
            Phase::Queued => self.emit_progress(&job),
            _ => {}
        }
        self.pump();
    }

    async fn download(&self, job: &Job) -> Result<(), Failure> {
        let original = reqwest::Url::parse(&job.spec.url)
            .map_err(|_| Failure::fatal("The download address isn't valid."))?;
        let (mut sink, mut offset) = Sink::open(job)
            .await
            .map_err(|error| Failure::fatal(format!("Couldn't open the download file: {error}")))?;
        // First report comes after the partial file is measured, so a resumed job never shows zero.
        job.update(|progress| progress.bytes_done = offset);
        self.emit_progress(job);

        let mut total: Option<u64> = None;
        // The address the provider redirects to, reused for later pieces to skip the redirect chain.
        let mut resolved: Option<reqwest::Url> = None;
        let mut reuse_resolved = true;
        let mut failures = 0u32;

        loop {
            if job.stop_reason().is_some() {
                return Err(Failure::stopped());
            }
            if let Some(total) = total {
                if offset == total {
                    break;
                }
                if offset > total {
                    return Err(Failure::fatal(MESSAGE_OVERSIZED));
                }
            }

            let using_resolved = resolved.is_some();
            let url = resolved.clone().unwrap_or_else(|| original.clone());
            let offset_before = offset;
            let result = self.fetch_piece(job, &mut sink, &url, &original, &mut offset, &mut total).await;
            // Keeps the on-disk size equal to `offset`, whatever happened to the request.
            if let Err(error) = sink.flush().await {
                return Err(Failure::fatal(format!("Couldn't write to disk: {error}")));
            }

            let failure = match result {
                Ok(Piece::Complete) => break,
                Ok(Piece::Progress(final_url)) => {
                    failures = 0;
                    resolved = if reuse_resolved { Some(final_url) } else { None };
                    if job.phase() != Phase::Downloading {
                        job.update(|progress| {
                            progress.phase = Phase::Downloading;
                            progress.error = None;
                            progress.attempt = 0;
                        });
                        self.emit_progress(job);
                    }
                    continue;
                }
                Ok(Piece::Empty) => Failure::transient("The server sent no data."),
                Err(failure) => failure,
            };

            match failure.kind {
                FailureKind::Stopped => return Err(failure),
                // A refused redirect target usually means its token expired: ask the provider again.
                _ if using_resolved && failure.http_status.is_some() => {
                    resolved = None;
                    reuse_resolved = false;
                    continue;
                }
                FailureKind::Fatal => return Err(failure),
                FailureKind::Transient => {}
            }

            resolved = None;
            failures = if offset > offset_before { 1 } else { failures + 1 };
            if failures > self.config.max_failures {
                return Err(failure);
            }
            job.update(|progress| {
                progress.phase = Phase::Retrying;
                progress.attempt = failures;
                progress.error = Some(failure.message.clone());
            });
            self.emit_progress(job);

            let delay = backoff_delay(failures, self.config.backoff_base, self.config.backoff_cap);
            tokio::select! {
                biased;
                _ = job.stopped() => return Err(Failure::stopped()),
                _ = tokio::time::sleep(delay) => {}
            }
        }

        let on_disk = sink
            .finish()
            .await
            .map_err(|error| Failure::fatal(format!("Couldn't write to disk: {error}")))?;
        if let Some(total) = total {
            if on_disk != total {
                return Err(Failure::fatal(format!(
                    "The saved file is {on_disk} bytes but should be {total}. Remove this download and start it again."
                )));
            }
        }
        job.update(|progress| {
            progress.bytes_done = on_disk;
            progress.bytes_total = on_disk;
        });
        Ok(())
    }

    async fn fetch_piece(
        &self,
        job: &Job,
        sink: &mut Sink,
        url: &reqwest::Url,
        original: &reqwest::Url,
        offset: &mut u64,
        total: &mut Option<u64>,
    ) -> Result<Piece, Failure> {
        let mut end = *offset + self.config.piece_size - 1;
        if let Some(total) = *total {
            end = end.min(total.saturating_sub(1));
        }
        let mut request = self
            .client
            .get(url.clone())
            .header(reqwest::header::RANGE, format!("bytes={}-{}", *offset, end));
        if let Some(user_agent) = job.spec.user_agent.as_deref() {
            request = request.header(reqwest::header::USER_AGENT, user_agent);
        }
        if let Some(authorization) = job.spec.authorization.as_deref() {
            if url.origin() == original.origin() {
                request = request.header(reqwest::header::AUTHORIZATION, authorization);
            }
        }

        let response = tokio::select! {
            biased;
            _ = job.stopped() => return Err(Failure::stopped()),
            sent = tokio::time::timeout(self.config.stall_timeout, request.send()) => match sent {
                Err(_) => return Err(Failure::transient("The server didn't answer in time.")),
                Ok(Err(error)) => return Err(Failure::transient(describe_network_error(&error))),
                Ok(Ok(response)) => response,
            },
        };

        let status = response.status().as_u16();
        let final_url = response.url().clone();
        let is_html = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim_start().to_ascii_lowercase().starts_with("text/html"));
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        match status {
            206 => {
                if is_html {
                    return Err(Failure::status(FailureKind::Fatal, status, MESSAGE_HTML));
                }
                let Some(range) = content_range.as_deref().and_then(parse_content_range) else {
                    return Err(Failure::transient("The server's reply couldn't be understood."));
                };
                if range.start != *offset || range.end < range.start {
                    return Err(Failure::transient("The server sent a different part of the file than requested."));
                }
                if let Some(range_total) = range.total {
                    if total.is_some_and(|known| known != range_total) {
                        return Err(Failure::fatal(MESSAGE_CHANGED));
                    }
                    *total = Some(range_total);
                    job.update(|progress| progress.bytes_total = range_total);
                }
                let expected = range.end - range.start + 1;
                let sniff = *offset == 0;
                let received = self.copy_body(job, sink, response, Some(expected), offset, sniff).await?;
                Ok(if received == 0 { Piece::Empty } else { Piece::Progress(final_url) })
            }
            200 => {
                if is_html {
                    return Err(Failure::status(FailureKind::Fatal, status, MESSAGE_HTML));
                }
                // The server ignored the range, so the only way forward is the whole file from the start.
                if *offset > 0 {
                    sink.restart()
                        .await
                        .map_err(|error| Failure::fatal(format!("Couldn't write to disk: {error}")))?;
                    *offset = 0;
                    job.update(|progress| progress.bytes_done = 0);
                }
                let length = response.content_length();
                if let Some(length) = length {
                    job.update(|progress| progress.bytes_total = length);
                }
                let received = self.copy_body(job, sink, response, None, offset, true).await?;
                match length {
                    Some(length) if received < length => Err(Failure::transient(MESSAGE_CLOSED_EARLY)),
                    _ => {
                        *total = Some(*offset);
                        Ok(Piece::Complete)
                    }
                }
            }
            416 => {
                let server_total = content_range.as_deref().and_then(parse_unsatisfied_total);
                match server_total {
                    Some(server_total) if server_total == *offset => {
                        *total = Some(server_total);
                        Ok(Piece::Complete)
                    }
                    Some(server_total) if *offset > server_total => {
                        Err(Failure::status(FailureKind::Fatal, status, MESSAGE_OVERSIZED))
                    }
                    None if *offset > 0 => Ok(Piece::Complete),
                    _ => Err(Failure::status(
                        FailureKind::Transient,
                        status,
                        "The server refused the requested part of the file (error 416).",
                    )),
                }
            }
            401 => Err(Failure::status(
                FailureKind::Fatal,
                status,
                "The provider rejected the login for this file (error 401).",
            )),
            404 | 410 => Err(Failure::status(
                FailureKind::Fatal,
                status,
                format!("The provider doesn't have this file (error {status})."),
            )),
            403 => Err(Failure::status(
                FailureKind::Transient,
                status,
                "The provider refused the download (error 403). The account may be at its connection limit.",
            )),
            408 | 425 | 429 => Err(Failure::status(
                FailureKind::Transient,
                status,
                format!("The server asked to try again later (error {status})."),
            )),
            400..=499 => Err(Failure::status(
                FailureKind::Fatal,
                status,
                format!("The server refused the download (error {status})."),
            )),
            _ => Err(Failure::status(
                FailureKind::Transient,
                status,
                format!("The server answered with an error ({status})."),
            )),
        }
    }

    /// Streams a response body to disk, advancing `offset` as bytes land. Returns the bytes written.
    async fn copy_body(
        &self,
        job: &Job,
        sink: &mut Sink,
        mut response: reqwest::Response,
        limit: Option<u64>,
        offset: &mut u64,
        sniff_first_chunk: bool,
    ) -> Result<u64, Failure> {
        let mut received = 0u64;
        let mut sniff = sniff_first_chunk;
        let mut last_emit = Instant::now();
        loop {
            if limit.is_some_and(|limit| received >= limit) {
                break;
            }
            let next = tokio::select! {
                biased;
                _ = job.stopped() => return Err(Failure::stopped()),
                chunk = tokio::time::timeout(self.config.stall_timeout, response.chunk()) => chunk,
            };
            let chunk = match next {
                Err(_) => return Err(Failure::transient(MESSAGE_STALLED)),
                Ok(Err(error)) => return Err(Failure::transient(describe_network_error(&error))),
                Ok(Ok(None)) => break,
                Ok(Ok(Some(chunk))) => chunk,
            };
            if chunk.is_empty() {
                continue;
            }
            if sniff {
                sniff = false;
                if looks_like_html(&chunk) {
                    return Err(Failure::fatal(MESSAGE_HTML));
                }
            }
            let take = match limit {
                Some(limit) => (limit - received).min(chunk.len() as u64) as usize,
                None => chunk.len(),
            };
            sink.write(&chunk[..take])
                .await
                .map_err(|error| Failure::fatal(format!("Couldn't write to disk: {error}")))?;
            received += take as u64;
            *offset += take as u64;
            let bytes_done = *offset;
            job.update(|progress| progress.bytes_done = bytes_done);
            if last_emit.elapsed() >= self.config.progress_interval {
                last_emit = Instant::now();
                self.emit_progress(job);
            }
        }
        Ok(received)
    }

    /// Runs while any job is unfinished: keeps the machine awake and publishes a status file that
    /// Android's foreground service reads for its notification.
    async fn housekeeping(self: Arc<Self>) {
        loop {
            let summary = {
                let mut inner = relock(&self.inner);
                let summary = summarize(&inner);
                if summary.unfinished == 0 {
                    inner.housekeeping = false;
                }
                summary
            };
            keep_awake::set(summary.unfinished > 0);
            self.write_status_file(&summary);
            if summary.unfinished == 0 {
                break;
            }
            tokio::time::sleep(HOUSEKEEPING_INTERVAL).await;
        }
    }

    fn write_status_file(&self, summary: &Summary) {
        if !cfg!(target_os = "android") {
            return;
        }
        let Some(path) = relock(&self.status_file).clone() else { return };
        let updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0);
        let body = json!({
            "updatedAt": updated_at,
            "unfinished": summary.unfinished,
            "running": summary.running,
            "held": summary.held,
            "title": summary.title,
            "bytesDone": summary.bytes_done,
            "bytesTotal": summary.bytes_total,
        });
        // Written beside the target and renamed, so the reader never sees half a file.
        let staging = path.with_extension("json.tmp");
        if std::fs::write(&staging, body.to_string()).is_ok() {
            let _ = std::fs::rename(&staging, &path);
        }
    }
}

struct Summary {
    unfinished: usize,
    running: usize,
    held: bool,
    title: String,
    bytes_done: u64,
    bytes_total: u64,
}

fn summarize(inner: &Inner) -> Summary {
    let unfinished = inner.jobs.values().filter(|job| job.phase().is_unfinished()).count();
    let current = inner.running.first().and_then(|id| inner.jobs.get(id));
    let (title, bytes_done, bytes_total) = match current {
        Some(job) => {
            let status = job.status();
            (job.spec.title.clone().unwrap_or_default(), status.bytes_done, status.bytes_total)
        }
        None => (String::new(), 0, 0),
    };
    Summary {
        unfinished,
        running: inner.running.len(),
        held: inner.limit == 0,
        title,
        bytes_done,
        bytes_total,
    }
}

fn backoff_delay(failures: u32, base: Duration, cap: Duration) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    base.saturating_mul(1u32 << doublings).min(cap)
}

struct ContentRange {
    start: u64,
    end: u64,
    total: Option<u64>,
}

/// Parses `bytes 0-99/1234` (the total may be `*`).
fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (span, total) = rest.split_once('/')?;
    let (start, end) = span.trim().split_once('-')?;
    Some(ContentRange {
        start: start.trim().parse().ok()?,
        end: end.trim().parse().ok()?,
        total: total.trim().parse().ok(),
    })
}

/// Parses the total out of a 416 reply's `bytes */1234`.
fn parse_unsatisfied_total(value: &str) -> Option<u64> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (span, total) = rest.split_once('/')?;
    if span.trim() != "*" {
        return None;
    }
    total.trim().parse().ok()
}

fn looks_like_html(chunk: &[u8]) -> bool {
    let head = &chunk[..chunk.len().min(256)];
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches(|character: char| character.is_whitespace() || character == '\u{feff}');
    let lower = text.to_ascii_lowercase();
    lower.starts_with("<!doctype html") || lower.starts_with("<html")
}

fn describe_network_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "The connection timed out.".to_string()
    } else if error.is_connect() {
        "Couldn't connect to the server.".to_string()
    } else if error.is_body() || error.is_decode() {
        "The connection dropped while downloading.".to_string()
    } else {
        "The network request failed.".to_string()
    }
}

#[cfg(unix)]
fn file_from_descriptor(descriptor: i32) -> Result<std::fs::File, String> {
    use std::os::fd::FromRawFd;
    if descriptor < 3 {
        return Err("That file handle isn't valid.".to_string());
    }
    // Safety: the frontend hands over a descriptor it just detached for this download and never uses again.
    Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
}

#[cfg(not(unix))]
fn file_from_descriptor(_descriptor: i32) -> Result<std::fs::File, String> {
    Err("File handles aren't supported on this platform.".to_string())
}

// ---------------------------------------------------------------------------
// Keeping the machine awake while downloads run
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod keep_awake {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Mutex, OnceLock};

    #[link(name = "kernel32")]
    extern "system" {
        fn SetThreadExecutionState(flags: u32) -> u32;
    }

    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static CONTROL: OnceLock<Mutex<Sender<bool>>> = OnceLock::new();

    pub fn set(active: bool) {
        if ACTIVE.swap(active, Ordering::SeqCst) == active {
            return;
        }
        // Windows ties the request to the calling thread, so one parked thread owns it for good.
        let control = CONTROL.get_or_init(|| {
            let (sender, receiver) = channel::<bool>();
            let _ = std::thread::Builder::new().name("download-keep-awake".into()).spawn(move || {
                for active in receiver {
                    let flags = if active { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS };
                    let previous = unsafe { SetThreadExecutionState(flags) };
                    log::info!("[download] keep-awake {} (previous state {previous:#x})", if active { "on" } else { "off" });
                }
            });
            Mutex::new(sender)
        });
        if let Ok(sender) = control.lock() {
            let _ = sender.send(active);
        }
    }
}

#[cfg(target_os = "macos")]
mod keep_awake {
    use std::process::{Child, Command};
    use std::sync::Mutex;

    static HELPER: Mutex<Option<Child>> = Mutex::new(None);

    pub fn set(active: bool) {
        let Ok(mut helper) = HELPER.lock() else { return };
        if active && helper.is_none() {
            // -w ties the helper to this process, so it can never outlive the app.
            *helper = Command::new("caffeinate")
                .args(["-i", "-w", &std::process::id().to_string()])
                .spawn()
                .ok();
        } else if !active {
            if let Some(mut child) = helper.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod keep_awake {
    pub fn set(_active: bool) {}
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Download/video/home/app-data dirs, matching the fs capability scope in `desktop.json`, plus any
/// folder picked in a dialog this session - so this command can't write where the page couldn't.
fn destination_allowed(app: &AppHandle, destination: &str) -> bool {
    use tauri_plugin_fs::FsExt;
    let path = PathBuf::from(destination);
    // The file may not exist yet, so its folder is what gets resolved (and what defeats `..`).
    let Some(folder) = path.parent().and_then(|parent| std::fs::canonicalize(parent).ok()) else {
        return false;
    };
    if app.fs_scope().is_allowed(&path) {
        return true;
    }
    let resolver = app.path();
    let roots = [
        resolver.download_dir().ok(),
        resolver.video_dir().ok(),
        resolver.home_dir().ok(),
        resolver.app_data_dir().ok(),
        resolver.app_local_data_dir().ok(),
    ];
    is_under_any_root(&folder, roots.into_iter().flatten().map(|root| std::fs::canonicalize(&root).unwrap_or(root)))
}

fn is_under_any_root(folder: &std::path::Path, roots: impl IntoIterator<Item = PathBuf>) -> bool {
    roots.into_iter().any(|root| folder.starts_with(&root))
}

#[tauri::command]
pub async fn download_enqueue(
    app: AppHandle,
    state: State<'_, DownloaderState>,
    spec: DownloadSpec,
) -> Result<JobStatus, String> {
    let scheme_ok = reqwest::Url::parse(&spec.url)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false);
    if !scheme_ok {
        return Err("Only http and https downloads are supported.".to_string());
    }
    let handed_file = match spec.fd {
        Some(descriptor) => Some(file_from_descriptor(descriptor)?),
        None => {
            let path = spec.path.as_deref().ok_or_else(|| "No download location was given.".to_string())?;
            if !destination_allowed(&app, path) {
                return Err("That folder isn't allowed for downloads. Choose the download folder again in Settings.".to_string());
            }
            None
        }
    };
    state.engine.attach(&app);
    Ok(state.engine.enqueue(spec, handed_file))
}

#[tauri::command]
pub async fn download_cancel(state: State<'_, DownloaderState>, id: String) -> Result<(), String> {
    state.engine.cancel(&id).await;
    Ok(())
}

#[tauri::command]
pub fn download_status(app: AppHandle, state: State<'_, DownloaderState>) -> Result<EngineStatus, String> {
    state.engine.attach(&app);
    Ok(state.engine.status())
}

#[tauri::command]
pub fn download_ack(state: State<'_, DownloaderState>, id: String) -> Result<(), String> {
    state.engine.acknowledge(&id);
    Ok(())
}

#[tauri::command]
pub fn download_set_limit(state: State<'_, DownloaderState>, limit: usize) -> Result<(), String> {
    state.engine.set_limit(limit.min(8));
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::State as AxumState;
    use axum::http::{HeaderMap, Response, StatusCode, Uri};
    use futures_util::StreamExt;
    use std::sync::atomic::AtomicUsize;

    const PIECE: u64 = 64 * 1024;

    enum Step {
        Normal,
        Status(u16),
        /// Send this many bytes of the piece, then break the connection.
        DropAfter(usize),
        /// Send this many bytes of the piece, then go silent.
        StallAfter(usize),
        HtmlPage,
    }

    struct Fixture {
        body: Vec<u8>,
        hits: Mutex<Vec<(String, Option<String>)>>,
        plan: Mutex<VecDeque<Step>>,
        ignore_range: AtomicBool,
        delay_ms: AtomicUsize,
    }

    impl Fixture {
        fn new(size: usize) -> Arc<Self> {
            let body = (0..size).map(|index| ((index * 31 + index / 251) % 251) as u8).collect();
            Arc::new(Self {
                body,
                hits: Mutex::new(Vec::new()),
                plan: Mutex::new(VecDeque::new()),
                ignore_range: AtomicBool::new(false),
                delay_ms: AtomicUsize::new(0),
            })
        }
        fn plan(&self, steps: Vec<Step>) {
            *self.plan.lock().unwrap() = steps.into();
        }
        fn file_hits(&self) -> Vec<Option<String>> {
            self.hits.lock().unwrap().iter().filter(|(path, _)| path == "/file").map(|(_, range)| range.clone()).collect()
        }
    }

    fn broken_body(bytes: Vec<u8>, stall: bool) -> Body {
        let head = futures_util::stream::iter(vec![Ok::<_, std::io::Error>(bytes::Bytes::from(bytes))]);
        if stall {
            Body::from_stream(head.chain(futures_util::stream::pending()))
        } else {
            // The pause lets the bytes reach the client before the connection breaks.
            let tail = futures_util::stream::once(async {
                tokio::time::sleep(Duration::from_millis(80)).await;
                Err(std::io::Error::other("connection reset by test"))
            });
            Body::from_stream(head.chain(tail))
        }
    }

    async fn serve(AxumState(fixture): AxumState<Arc<Fixture>>, uri: Uri, headers: HeaderMap) -> Response<Body> {
        let range = headers.get("range").and_then(|value| value.to_str().ok()).map(str::to_owned);
        fixture.hits.lock().unwrap().push((uri.path().to_string(), range.clone()));

        if uri.path() == "/start" {
            return Response::builder().status(StatusCode::FOUND).header("location", "/file").body(Body::empty()).unwrap();
        }
        if uri.path() != "/file" {
            return Response::builder().status(StatusCode::NOT_FOUND).body(Body::from("missing")).unwrap();
        }

        let delay = fixture.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        }
        let step = fixture.plan.lock().unwrap().pop_front().unwrap_or(Step::Normal);
        let total = fixture.body.len() as u64;

        if let Step::Status(code) = step {
            return Response::builder().status(code).body(Body::from("nope")).unwrap();
        }
        if let Step::HtmlPage = step {
            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header("content-type", "text/html")
                .header("content-range", format!("bytes 0-99/{total}"))
                .body(Body::from("<!doctype html><html><body>Potential Threat Detected</body></html>"))
                .unwrap();
        }

        let parsed = range.as_deref().and_then(|value| value.strip_prefix("bytes=")).and_then(|span| {
            let (start, end) = span.split_once('-')?;
            Some((start.parse::<u64>().ok()?, end.parse::<u64>().ok()))
        });
        let ranged = parsed.filter(|_| !fixture.ignore_range.load(Ordering::SeqCst));

        let Some((start, end)) = ranged else {
            return Response::builder()
                .status(StatusCode::OK)
                .header("content-length", total)
                .body(Body::from(fixture.body.clone()))
                .unwrap();
        };
        if start >= total {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{total}"))
                .body(Body::empty())
                .unwrap();
        }
        let end = end.unwrap_or(total - 1).min(total - 1);
        let slice = fixture.body[start as usize..=end as usize].to_vec();
        let builder = Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header("content-type", "video/mp4")
            .header("content-range", format!("bytes {start}-{end}/{total}"));
        match step {
            Step::DropAfter(count) => builder.body(broken_body(slice[..count.min(slice.len())].to_vec(), false)).unwrap(),
            Step::StallAfter(count) => builder.body(broken_body(slice[..count.min(slice.len())].to_vec(), true)).unwrap(),
            _ => builder.header("content-length", slice.len()).body(Body::from(slice)).unwrap(),
        }
    }

    async fn start_server(fixture: Arc<Fixture>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind must succeed");
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(serve).with_state(fixture);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    fn test_engine() -> Arc<Engine> {
        Engine::new(EngineConfig {
            piece_size: PIECE,
            stall_timeout: Duration::from_millis(400),
            connect_timeout: Duration::from_secs(2),
            max_failures: 3,
            backoff_base: Duration::from_millis(10),
            backoff_cap: Duration::from_millis(40),
            progress_interval: Duration::from_millis(5),
        })
    }

    fn temp_path(name: &str) -> PathBuf {
        let unique = format!(
            "xt-downloader-{}-{}-{}",
            std::process::id(),
            name,
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    fn spec(id: &str, url: String, path: &PathBuf) -> DownloadSpec {
        DownloadSpec {
            id: id.to_string(),
            url,
            path: Some(path.to_string_lossy().into_owned()),
            fd: None,
            title: Some(id.to_string()),
            user_agent: Some("test-agent".to_string()),
            authorization: None,
        }
    }

    async fn wait_for(engine: &Arc<Engine>, id: &str, wanted: &[Phase]) -> JobStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let found = engine.status().jobs.into_iter().find(|job| job.id == id);
            if let Some(job) = found {
                if wanted.contains(&job.phase) {
                    return job;
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for {id} to reach {wanted:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn downloads_a_file_in_sequential_pieces() {
        let fixture = Fixture::new(PIECE as usize * 3 + 1000);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("pieces");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Done, "{:?}", status.error);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        assert_eq!(status.bytes_total, fixture.body.len() as u64);
        let hits = fixture.file_hits();
        assert_eq!(hits.len(), 4);
        assert_eq!(hits[0].as_deref(), Some("bytes=0-65535"));
        assert_eq!(hits[1].as_deref(), Some("bytes=65536-131071"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resumes_from_a_partial_file() {
        let fixture = Fixture::new(PIECE as usize * 3);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("resume");
        let already = PIECE as usize + 500;
        std::fs::write(&path, &fixture.body[..already]).unwrap();
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Done, "{:?}", status.error);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        assert_eq!(fixture.file_hits()[0].as_deref(), Some(format!("bytes={already}-{}", already as u64 + PIECE - 1).as_str()));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn recovers_from_dropped_connections_and_server_errors() {
        let fixture = Fixture::new(PIECE as usize * 2 + 77);
        fixture.plan(vec![Step::DropAfter(1000), Step::Status(503), Step::Normal, Step::DropAfter(0), Step::Normal]);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("drops");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Done, "{:?}", status.error);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        // The retry after the drop asks for exactly the byte after the last one that arrived.
        assert_eq!(fixture.file_hits()[1].as_deref(), Some("bytes=1000-66535"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn recovers_from_a_stalled_connection() {
        let fixture = Fixture::new(PIECE as usize + 10);
        fixture.plan(vec![Step::StallAfter(2000)]);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("stall");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Done, "{:?}", status.error);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn gives_up_after_repeated_failures_and_keeps_the_partial_file() {
        let fixture = Fixture::new(PIECE as usize * 2);
        fixture.plan(vec![Step::DropAfter(3000), Step::Status(500), Step::Status(500), Step::Status(500), Step::Status(500)]);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("giveup");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Error);
        assert!(status.error.unwrap().contains("500"));
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body[..3000]);

        // A later retry of the same download continues from the partial file.
        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;
        assert_eq!(status.phase, Phase::Done, "{:?}", status.error);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn restarts_from_zero_when_the_server_ignores_ranges() {
        let fixture = Fixture::new(PIECE as usize * 2 + 5);
        fixture.ignore_range.store(true, Ordering::SeqCst);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("norange");
        std::fs::write(&path, vec![9u8; 4000]).unwrap();
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Done, "{:?}", status.error);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        assert_eq!(fixture.file_hits().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn refuses_a_web_page_served_in_place_of_the_video() {
        let fixture = Fixture::new(PIECE as usize);
        fixture.plan(vec![Step::HtmlPage]);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("html");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Error);
        assert!(status.error.unwrap().contains("web page"));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(fixture.file_hits().len(), 1, "a blocked file must not be retried");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reports_a_missing_file_without_retrying() {
        let fixture = Fixture::new(PIECE as usize);
        fixture.plan(vec![Step::Status(404)]);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("missing");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Error);
        assert!(status.error.unwrap().contains("404"));
        assert_eq!(fixture.file_hits().len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn treats_an_already_complete_file_as_done() {
        let fixture = Fixture::new(PIECE as usize + 9);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("complete");
        std::fs::write(&path, &fixture.body).unwrap();
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Done, "{:?}", status.error);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn follows_the_provider_redirect_and_returns_to_it_when_the_link_expires() {
        let fixture = Fixture::new(PIECE as usize * 3);
        // Piece 1 via the redirect, piece 2 straight to the redirect target is refused, then all good.
        fixture.plan(vec![Step::Normal, Step::Status(403)]);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("redirect");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/start"), &path), None);
        let status = wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await;

        assert_eq!(status.phase, Phase::Done, "{:?}", status.error);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        let starts = fixture.hits.lock().unwrap().iter().filter(|(path, _)| path == "/start").count();
        // Once for piece 1, then once per remaining piece after the reused link was refused.
        assert_eq!(starts, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn cancel_stops_a_running_job_and_releases_the_file() {
        let fixture = Fixture::new(PIECE as usize * 40);
        fixture.delay_ms.store(30, Ordering::SeqCst);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("cancel");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        wait_for(&engine, "a", &[Phase::Downloading]).await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        engine.cancel("a").await;

        assert!(engine.status().jobs.is_empty());
        let partial = std::fs::metadata(&path).unwrap().len();
        assert!(partial > 0 && partial < fixture.body.len() as u64);
        // The handle is released: the partial file can be deleted straight away (matters on Windows).
        std::fs::remove_file(&path).expect("partial file must not be locked after cancel");
    }

    #[tokio::test]
    async fn the_limit_queues_holds_and_releases_jobs() {
        let fixture = Fixture::new(PIECE as usize * 12);
        fixture.delay_ms.store(15, Ordering::SeqCst);
        let base = start_server(fixture.clone()).await;
        let first = temp_path("limit-a");
        let second = temp_path("limit-b");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &first), None);
        engine.enqueue(spec("b", format!("{base}/file"), &second), None);
        wait_for(&engine, "a", &[Phase::Downloading]).await;
        assert_eq!(wait_for(&engine, "b", &[Phase::Queued, Phase::Downloading]).await.phase, Phase::Queued);

        // A limit of zero (someone is watching on a one-connection account) parks everything.
        engine.set_limit(0);
        wait_for(&engine, "a", &[Phase::Queued]).await;
        let parked = std::fs::metadata(&first).unwrap().len();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(std::fs::metadata(&first).unwrap().len(), parked, "a held job must not keep downloading");

        engine.set_limit(1);
        assert_eq!(wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await.phase, Phase::Done);
        assert_eq!(wait_for(&engine, "b", &[Phase::Done, Phase::Error]).await.phase, Phase::Done);
        assert_eq!(std::fs::read(&first).unwrap(), fixture.body);
        assert_eq!(std::fs::read(&second).unwrap(), fixture.body);
        let _ = std::fs::remove_file(&first);
        let _ = std::fs::remove_file(&second);
    }

    #[tokio::test]
    async fn enqueue_is_idempotent_and_ack_forgets_finished_jobs() {
        let fixture = Fixture::new(PIECE as usize * 6);
        fixture.delay_ms.store(20, Ordering::SeqCst);
        let base = start_server(fixture.clone()).await;
        let path = temp_path("idempotent");
        let engine = test_engine();

        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        wait_for(&engine, "a", &[Phase::Downloading]).await;
        engine.enqueue(spec("a", format!("{base}/file"), &path), None);
        assert_eq!(engine.status().jobs.len(), 1);

        assert_eq!(wait_for(&engine, "a", &[Phase::Done, Phase::Error]).await.phase, Phase::Done);
        assert_eq!(std::fs::read(&path).unwrap(), fixture.body);
        engine.acknowledge("a");
        assert!(engine.status().jobs.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn confines_destinations_to_the_allowed_folders() {
        let base = temp_path("roots");
        let allowed = base.join("Downloads");
        let nested = allowed.join("Movies");
        let outside = base.join("Elsewhere");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let root = std::fs::canonicalize(&allowed).unwrap();
        let resolve = |path: PathBuf| std::fs::canonicalize(path).unwrap();

        assert!(is_under_any_root(&resolve(allowed.clone()), vec![root.clone()]));
        assert!(is_under_any_root(&resolve(nested.clone()), vec![root.clone()]));
        assert!(!is_under_any_root(&resolve(outside.clone()), vec![root.clone()]));
        // A path that climbs back out with `..` resolves to where it really points.
        assert!(!is_under_any_root(&resolve(nested.join("..").join("..").join("Elsewhere")), vec![root.clone()]));
        assert!(!is_under_any_root(&resolve(allowed), Vec::new()));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parses_content_range_headers() {
        let range = parse_content_range("bytes 100-199/1234").unwrap();
        assert_eq!((range.start, range.end, range.total), (100, 199, Some(1234)));
        assert_eq!(parse_content_range("bytes 0-9/*").unwrap().total, None);
        assert!(parse_content_range("items 0-9/10").is_none());
        assert_eq!(parse_unsatisfied_total("bytes */500"), Some(500));
        assert_eq!(parse_unsatisfied_total("bytes 0-9/500"), None);
    }

    #[test]
    fn recognises_html_bodies() {
        assert!(looks_like_html(b"  \n<!DOCTYPE html><html>"));
        assert!(looks_like_html(b"<html lang=\"en\">"));
        assert!(!looks_like_html(&[0x00, 0x00, 0x00, 0x20, b'f', b't', b'y', b'p']));
        assert!(!looks_like_html(&[0x1a, 0x45, 0xdf, 0xa3]));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let base = Duration::from_secs(2);
        let cap = Duration::from_secs(60);
        assert_eq!(backoff_delay(1, base, cap), Duration::from_secs(2));
        assert_eq!(backoff_delay(2, base, cap), Duration::from_secs(4));
        assert_eq!(backoff_delay(5, base, cap), Duration::from_secs(32));
        assert_eq!(backoff_delay(6, base, cap), cap);
        assert_eq!(backoff_delay(40, base, cap), cap);
    }
}

//! Connect an app to Local Router (PROTOCOL.md 4, 5, 15).
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use localrouter_client::{discover, Presence};
//! match discover().await {
//!     Presence::Running(client) => { let app = client.connect(&manifest(), None).await?; }
//!     Presence::Installed => { /* offer "Start Local Router": localrouter_client::start().await */ }
//!     Presence::NotInstalled | Presence::Unreachable => { /* run standalone */ }
//! }
//! # Ok(()) }
//! # fn manifest() -> localrouter_core::ConnectRequest { unimplemented!() }
//! ```
//!
//! [`LocalAi`] gives an app one way to transcribe, whether Local Router is
//! there or not: through Local Router when connected, otherwise in-process
//! with the same engine and the same Hugging Face cache (feature `embedded`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use localrouter_core::{AppRecord, ConnectRequest, Endpoint, ModelRef, RoleDeclaration};
use localrouter_core::{ApiError, Capability, Health};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Where to get Local Router (PROTOCOL.md 4.2).
pub const DOWNLOAD_URL: &str = "https://github.com/Askantis/public-local-router/releases/latest";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Local Router answered: {0}")]
    Api(Box<ApiError>),
    #[error("couldn't reach Local Router: {0}")]
    Transport(String),
    #[error("the user didn't allow the connection")]
    Denied,
    #[error("nobody answered the connection request in time")]
    Expired,
    /// The app stopped the work with [`Cancel::cancel`].
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// The protocol error code (`role_unbound`, `not_approved`, …), if any.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Api(e) => Some(&e.code),
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Reaching Local Router's socket; it's local, so this only trips when it hangs.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Requests Local Router answers at once (status, roles, downloads' state).
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);
/// A whole chat answer, or loading a model to hold: a model may load first.
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// One live window (at most 30 s of audio).
pub const WINDOW_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// Whole-file work answered in one piece (transcription without streaming,
/// diarization).
pub const FILE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
/// A streamed transcription has no overall limit (long files take long),
/// but this much silence means Local Router is stuck. It's generous because
/// Local Router sends nothing while it decodes, finds speakers or waits for
/// another app's file.
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Stops a transcription from elsewhere (a Cancel button): the call returns
/// [`Error::Cancelled`] right away. Through Local Router the request is
/// dropped; in the app the engine stops at its next window.
#[derive(Clone)]
pub struct Cancel(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for Cancel {
    fn default() -> Self {
        Cancel(Arc::new(tokio::sync::watch::Sender::new(false)))
    }
}

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Cancel").field(&self.is_cancelled()).finish()
    }
}

impl Cancel {
    pub fn new() -> Self {
        Cancel::default()
    }

    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once [`cancel`](Self::cancel) was called.
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    /// `work`, unless cancelled first.
    pub async fn run<T>(&self, work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        if self.is_cancelled() {
            return Err(Error::Cancelled);
        }
        tokio::select! {
            r = work => r,
            _ = self.cancelled() => Err(Error::Cancelled),
        }
    }
}

/// `work` within `limit`, or a transport error saying what took too long.
async fn within<T>(limit: Duration, what: &str, work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(limit, work)
        .await
        .unwrap_or_else(|_| Err(Error::Transport(format!("Local Router didn't answer {what} within {} s", limit.as_secs()))))
}

/// What [`discover`] found.
#[allow(clippy::large_enum_variant)] // returned once, at discovery
pub enum Presence {
    Running(Client),
    /// Installed but not running: offer to [`start`] it.
    Installed,
    NotInstalled,
    /// A discovery file is there but nothing answers.
    Unreachable,
}

/// Find Local Router (PROTOCOL.md 4.2). Never takes longer than a health check.
pub async fn discover() -> Presence {
    discover_in(&localrouter_core::support_dir()).await
}

/// [`discover`] with an explicit support folder (tests, development).
pub async fn discover_in(support_dir: &Path) -> Presence {
    if let Ok(ep) = Endpoint::read(&support_dir.join("endpoint.json")) {
        if let Ok(client) = Client::new(&ep)
            && let Ok(h) = client.health().await
                && h.instance_id == ep.instance_id {
                    return Presence::Running(client);
                }
        return if installed() { Presence::Unreachable } else { Presence::NotInstalled };
    }
    if installed() { Presence::Installed } else { Presence::NotInstalled }
}

/// Whether Local Router is installed. On macOS, asks Launch Services which
/// apps have its bundle id (no side effects: nothing is opened or revealed).
pub fn installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        launch_services::has_app(localrouter_core::BUNDLE_ID)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
mod launch_services {
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn LSCopyApplicationURLsForBundleIdentifier(id: CFStringRef, error: *mut *const std::ffi::c_void) -> CFArrayRef;
    }

    pub fn has_app(bundle_id: &str) -> bool {
        let id = CFString::new(bundle_id);
        let urls = unsafe { LSCopyApplicationURLsForBundleIdentifier(id.as_concrete_TypeRef(), std::ptr::null_mut()) };
        if urls.is_null() {
            return false;
        }
        let urls: CFArray = unsafe { CFArray::wrap_under_create_rule(urls) };
        !urls.is_empty()
    }
}

/// Start Local Router (in the background, no window) and wait up to 10 s for it.
pub async fn start() -> Result<Client> {
    #[cfg(target_os = "macos")]
    {
        // Launching the app (in the background) makes sure its service runs.
        // `open` waits for Launch Services: off the async threads.
        let ok = tokio::task::spawn_blocking(|| {
            std::process::Command::new("/usr/bin/open")
                .args(["-g", "-b", localrouter_core::BUNDLE_ID])
                .status()
                .is_ok_and(|s| s.success())
        })
        .await
        .unwrap_or(false);
        if !ok {
            return Err(Error::Other("Local Router isn't installed".into()));
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Presence::Running(c) = discover().await {
            return Ok(c);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(Error::Other("Local Router didn't start".into()))
}

/// A connection to Local Router over its Unix socket.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    endpoint: Endpoint,
    user_agent: String,
}

/// Options for [`Client::transcribe`] and [`LocalAi::transcribe`].
#[derive(Debug, Clone, Default)]
pub struct TranscribeOptions {
    /// A model id or `localrouter/<role>`; `localrouter/transcribe` when empty.
    pub model: Option<String>,
    /// ISO 639-1, or `gsw` for Swiss German; detected when `None`.
    pub language: Option<String>,
    pub prompt: Option<String>,
    /// Separate speakers (`lr_speakers_json`).
    pub speakers: bool,
    /// Transcribe only these stretches, with their speakers (`lr_segments`).
    pub segments: Option<Vec<Segment>>,
    /// Seconds added to every time (`lr_offset`), for live windows.
    pub offset: f64,
    /// `live` for live windows, `batch` for work nobody waits for (`lr_priority`).
    pub priority: Option<String>,
}

/// A download Local Router runs for the app (PROTOCOL.md 8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    pub id: String,
    pub model: String,
    pub source: String,
    /// `queued`, `running`, `verifying`, `done`, `failed` or `cancelled`.
    pub state: String,
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: Option<u64>,
    #[serde(default)]
    pub started_by_you: bool,
    #[serde(default)]
    pub error: Option<ApiError>,
}

impl Download {
    pub fn finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed" | "cancelled")
    }

    /// 0.0 to 1.0, when the size is known.
    pub fn fraction(&self) -> Option<f64> {
        self.bytes_total.filter(|t| *t > 0).map(|t| (self.bytes_done as f64 / t as f64).min(1.0))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Word {
    pub word: String,
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub probability: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TranscriptSegment {
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub speaker: Option<String>,
    pub text: String,
    #[serde(default)]
    pub words: Vec<Word>,
}

/// A transcript, as `verbose_json` / `lr_speakers_json` (PROTOCOL.md 11.4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Transcript {
    #[serde(default)]
    pub language: String,
    pub duration: f64,
    pub text: String,
    pub segments: Vec<TranscriptSegment>,
    /// Which model did it: the `LR-Resolved-Model` header, or the embedded model.
    #[serde(default)]
    pub model: Option<String>,
}

impl Client {
    pub fn new(endpoint: &Endpoint) -> Result<Self> {
        let socket = endpoint.socket.as_ref().ok_or_else(|| Error::Other("Local Router has no socket".into()))?;
        // No overall timeout here: each call sets its own (the constants above).
        let http = reqwest::Client::builder()
            .unix_socket(PathBuf::from(socket))
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| Error::Transport(e.to_string()))?;
        Ok(Client {
            http,
            endpoint: endpoint.clone(),
            user_agent: format!("localrouter-client/{}", env!("CARGO_PKG_VERSION")),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Sent as `LR-Client` (informational): `"<app_id>/<version>"`.
    pub fn with_app(mut self, app_id: &str, version: &str) -> Self {
        self.user_agent = format!("{app_id}/{version} localrouter-client/{}", env!("CARGO_PKG_VERSION"));
        self
    }

    fn url(&self, path: &str) -> String {
        format!("http://lr{path}")
    }

    /// A request Local Router answers at once ([`CONTROL_TIMEOUT`]; callers
    /// that wait longer set their own).
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, self.url(path)).timeout(CONTROL_TIMEOUT)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::GET, path)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::POST, path)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let resp = req.header("LR-Client", &self.user_agent).send().await.map_err(|e| Error::Transport(e.to_string()))?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let mut err: ApiError = serde_json::from_value(body["error"].clone())
            .unwrap_or_else(|_| ApiError::new(status, "server_error", "internal", body.to_string()));
        err.status = status;
        Err(Error::Api(Box::new(err)))
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> Result<T> {
        self.send(req).await?.json().await.map_err(|e| Error::Transport(e.to_string()))
    }

    pub async fn health(&self) -> Result<Health> {
        self.json(self.get("/lr/v1/health").timeout(Duration::from_secs(3))).await
    }

    /// Connect once (PROTOCOL.md 5.2). Askantis-signed apps are approved right
    /// away; others wait until the user answers in Local Router, calling
    /// `waiting` once so the app can say "Allow it in Local Router".
    pub async fn connect(&self, manifest: &ConnectRequest, waiting: Option<&(dyn Fn() + Sync)>) -> Result<AppRecord> {
        let resp = self.send(self.post("/lr/v1/connect").json(manifest)).await?;
        let pending = resp.status() == reqwest::StatusCode::ACCEPTED;
        let body: Value = resp.json().await.map_err(|e| Error::Transport(e.to_string()))?;
        if !pending {
            return serde_json::from_value(body["app"].clone()).map_err(|e| Error::Other(e.to_string()));
        }
        if let Some(w) = waiting {
            w();
        }
        let poll = body["poll"].as_str().unwrap_or_default().to_string();
        let deadline = Instant::now() + Duration::from_secs(6 * 60);
        while Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let s: Value = self.json(self.get(&poll)).await?;
            match s["status"].as_str() {
                Some("approved") => return serde_json::from_value(s["app"].clone()).map_err(|e| Error::Other(e.to_string())),
                Some("denied") => return Err(Error::Denied),
                Some("expired") => return Err(Error::Expired),
                _ => {}
            }
        }
        Err(Error::Expired)
    }

    pub async fn me(&self) -> Result<AppRecord> {
        self.json(self.get("/lr/v1/me")).await
    }

    pub async fn put_roles(&self, roles: &[RoleDeclaration]) -> Result<AppRecord> {
        self.json(self.request(reqwest::Method::PUT, "/lr/v1/me/roles").json(&json!({ "roles": roles }))).await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.send(self.request(reqwest::Method::DELETE, "/lr/v1/me")).await.map(|_| ())
    }

    pub async fn models(&self) -> Result<Vec<Value>> {
        let v: Value = self.json(self.get("/lr/v1/models")).await?;
        Ok(v["models"].as_array().cloned().unwrap_or_default())
    }

    /// Tell Local Router about models the app runs itself (PROTOCOL.md 9).
    pub async fn report_resident(&self, models: &[Value], lease_seconds: u64) -> Result<()> {
        let body = json!({ "models": models, "lease_seconds": lease_seconds });
        self.send(self.request(reqwest::Method::PUT, "/lr/v1/me/resident").json(&body)).await.map(|_| ())
    }

    /// A chat turn through Ollama's API (non-streaming). `body` is Ollama's
    /// `/api/chat` request; its `model` may be `localrouter/<role>`.
    pub async fn chat(&self, mut body: Value) -> Result<Value> {
        body["stream"] = json!(false);
        self.json(self.post("/api/chat").timeout(CHAT_TIMEOUT).json(&body)).await
    }

    /// Speech to text; `audio` is the file's bytes and `filename` its name (for the format).
    pub async fn transcribe(&self, audio: Vec<u8>, filename: &str, opts: &TranscribeOptions) -> Result<Transcript> {
        let form = self.transcribe_form(bytes_part(audio, filename), opts);
        let resp = self.send(self.post("/v1/audio/transcriptions").timeout(FILE_TIMEOUT).multipart(form)).await?;
        let model = resp.headers().get("LR-Resolved-Model").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let mut t: Transcript = resp.json().await.map_err(|e| Error::Transport(e.to_string()))?;
        t.model = model;
        Ok(t)
    }

    /// Speech to text with each finished segment handed to `on_segment` as it
    /// arrives (`stream=true`); returns the whole transcript at the end.
    pub async fn transcribe_streaming(
        &self,
        audio: Vec<u8>,
        filename: &str,
        opts: &TranscribeOptions,
        on_segment: impl FnMut(TranscriptSegment),
    ) -> Result<Transcript> {
        self.stream_transcription(bytes_part(audio, filename), opts, on_segment).await
    }

    /// [`transcribe_streaming`](Self::transcribe_streaming) of a file, sent
    /// from disk as it's read (a long video isn't held in memory).
    pub async fn transcribe_file_streaming(&self, path: &Path, opts: &TranscribeOptions, on_segment: impl FnMut(TranscriptSegment)) -> Result<Transcript> {
        let part = reqwest::multipart::Part::file(path).await.map_err(|e| Error::Other(format!("reading {}: {e}", path.display())))?;
        self.stream_transcription(part, opts, on_segment).await
    }

    /// No overall limit: a long file streams for long. Silence longer than
    /// [`STREAM_IDLE_TIMEOUT`] (before the answer starts or between events)
    /// means Local Router is stuck.
    async fn stream_transcription(&self, file: reqwest::multipart::Part, opts: &TranscribeOptions, mut on_segment: impl FnMut(TranscriptSegment)) -> Result<Transcript> {
        use futures::StreamExt;
        let form = self.transcribe_form(file, opts).text("stream", "true");
        let req = self.http.post(self.url("/v1/audio/transcriptions")).multipart(form);
        let resp = within(STREAM_IDLE_TIMEOUT, "the transcription", self.send(req)).await?;
        let model = resp.headers().get("LR-Resolved-Model").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let mut body = resp.bytes_stream();
        let mut buf = String::new();
        loop {
            let chunk = match tokio::time::timeout(STREAM_IDLE_TIMEOUT, body.next()).await {
                Ok(Some(chunk)) => chunk.map_err(|e| Error::Transport(e.to_string()))?,
                Ok(None) => break,
                Err(_) => return Err(Error::Transport(format!("Local Router sent nothing for {} min during the transcription", STREAM_IDLE_TIMEOUT.as_secs() / 60))),
            };
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buf.find("\n\n") {
                let block: String = buf.drain(..end + 2).collect();
                let data: String = block.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim).collect();
                let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                match v["type"].as_str() {
                    Some("transcript.text.segment") => {
                        if let Ok(s) = serde_json::from_value(v["segment"].clone()) {
                            on_segment(s);
                        }
                    }
                    Some("transcript.text.done") => {
                        let mut t: Transcript = serde_json::from_value(v).map_err(|e| Error::Other(e.to_string()))?;
                        t.model = model;
                        return Ok(t);
                    }
                    Some("error") => {
                        let e: ApiError = serde_json::from_value(v["error"].clone())
                            .unwrap_or_else(|_| ApiError::internal("transcription failed"));
                        return Err(Error::Api(Box::new(e)));
                    }
                    _ => {}
                }
            }
        }
        Err(Error::Transport("the transcription stream ended early".into()))
    }

    fn transcribe_form(&self, file: reqwest::multipart::Part, opts: &TranscribeOptions) -> reqwest::multipart::Form {
        let mut form = reqwest::multipart::Form::new()
            .part("file", file)
            .text("model", opts.model.clone().unwrap_or_else(|| "localrouter/transcribe".into()))
            .text("response_format", if opts.speakers || opts.segments.is_some() { "lr_speakers_json" } else { "verbose_json" })
            .text("timestamp_granularities[]", "word")
            .text("timestamp_granularities[]", "segment");
        if let Some(l) = &opts.language {
            form = form.text("language", l.clone());
        }
        if let Some(p) = &opts.prompt {
            form = form.text("prompt", p.clone());
        }
        if let Some(s) = &opts.segments {
            form = form.text("lr_segments", serde_json::to_string(s).unwrap());
        }
        if opts.offset != 0.0 {
            form = form.text("lr_offset", opts.offset.to_string());
        }
        if let Some(p) = &opts.priority {
            form = form.text("lr_priority", p.clone());
        }
        form
    }

    /// Ask Local Router to download a model (PROTOCOL.md 8). `request` is the
    /// body of `POST /lr/v1/downloads`, for example
    /// `{"model": {"store": "ollama", "id": "qwen3.5:9b"}, "source": "ollama"}`.
    pub async fn download(&self, request: &Value) -> Result<Download> {
        self.json(self.post("/lr/v1/downloads").json(request)).await
    }

    pub async fn download_status(&self, id: &str) -> Result<Download> {
        self.json(self.get(&format!("/lr/v1/downloads/{id}"))).await
    }

    /// Stop waiting for a download; it stops when no other app wants it.
    pub async fn cancel_download(&self, id: &str) -> Result<Download> {
        self.json(self.request(reqwest::Method::DELETE, &format!("/lr/v1/downloads/{id}"))).await
    }

    /// Follow a download to the end, calling `progress` about twice a second.
    /// A failed download comes back as its error.
    pub async fn wait_download(&self, mut d: Download, mut progress: impl FnMut(&Download)) -> Result<Download> {
        loop {
            progress(&d);
            if d.finished() {
                return match (d.state.as_str(), d.error.clone()) {
                    ("failed", Some(e)) => Err(Error::Api(Box::new(e))),
                    ("failed", None) => Err(Error::Other(format!("downloading {} failed", d.model))),
                    ("cancelled", _) => Err(Error::Other(format!("downloading {} was cancelled", d.model))),
                    _ => Ok(d),
                };
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            d = self.download_status(&d.id).await?;
        }
    }

    /// Keep a model loaded, for example before the user presses Record
    /// (PROTOCOL.md 7.2). `keep_alive` like `"15m"`; at most 4 hours.
    pub async fn hold(&self, model: &str, context: Option<u32>, keep_alive: Option<&str>) -> Result<Value> {
        let mut body = json!({ "model": model });
        if let Some(c) = context {
            body["context"] = json!(c);
        }
        if let Some(k) = keep_alive {
            body["keep_alive"] = json!(k);
        }
        self.json(self.post("/lr/v1/models/load").timeout(CHAT_TIMEOUT).json(&body)).await
    }

    pub async fn release(&self, model: &str) -> Result<()> {
        self.send(self.post("/lr/v1/models/unload").json(&json!({ "model": model }))).await.map(|_| ())
    }

    /// Who speaks when (PROTOCOL.md 11.5).
    pub async fn diarize(&self, audio: Vec<u8>, filename: &str) -> Result<Vec<Segment>> {
        let form = reqwest::multipart::Form::new().part("file", bytes_part(audio, filename));
        let v: Value = self.json(self.post("/lr/v1/audio/diarizations").timeout(FILE_TIMEOUT).multipart(form)).await?;
        serde_json::from_value(v["segments"].clone()).map_err(|e| Error::Other(e.to_string()))
    }
}

/// One segment of a live window (PROTOCOL.md 11.4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowSegment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    #[serde(default)]
    pub words: Vec<Word>,
    /// How sure Whisper is that nothing was said; high values are usually
    /// made-up text over silence.
    #[serde(default)]
    pub no_speech_prob: Option<f32>,
}

/// What a live window said: segments in the recording's timeline (the
/// window's offset is added) and the language Whisper heard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WindowResult {
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub segments: Vec<WindowSegment>,
}

/// Keeps a model loaded in Local Router while it lives: renews the hold,
/// and lets go when dropped (PROTOCOL.md 7.2).
pub struct HoldGuard {
    client: Client,
    model: String,
    renew: tokio::task::JoinHandle<()>,
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        self.renew.abort();
        let (client, model) = (self.client.clone(), self.model.clone());
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = client.release(&model).await;
            });
        }
    }
}

impl Client {
    /// A live window: mono 16 kHz samples, sent as WAV with
    /// `lr_priority=live` (unless `opts.priority` says otherwise) and
    /// `lr_offset`, answered as `verbose_json` with word times.
    pub async fn transcribe_window(&self, samples: &[f32], opts: &TranscribeOptions) -> Result<WindowResult> {
        let mut form = reqwest::multipart::Form::new()
            .part("file", reqwest::multipart::Part::bytes(wav_pcm16(samples, 16_000)).file_name("window.wav"))
            .text("model", opts.model.clone().unwrap_or_else(|| "localrouter/transcribe".into()))
            .text("response_format", "verbose_json")
            .text("timestamp_granularities[]", "word")
            .text("timestamp_granularities[]", "segment")
            .text("lr_priority", opts.priority.clone().unwrap_or_else(|| "live".into()));
        if let Some(l) = &opts.language {
            form = form.text("language", l.clone());
        }
        if let Some(p) = &opts.prompt {
            form = form.text("prompt", p.clone());
        }
        if opts.offset != 0.0 {
            form = form.text("lr_offset", opts.offset.to_string());
        }
        let v: Value = self.json(self.post("/v1/audio/transcriptions").timeout(WINDOW_TIMEOUT).multipart(form)).await?;
        let language = v["language"].as_str().filter(|l| !l.is_empty()).map(str::to_owned);
        let segments = serde_json::from_value(v["segments"].clone()).map_err(|e| Error::Other(e.to_string()))?;
        Ok(WindowResult { language, segments })
    }

    /// Hold `model` now, renew the hold every `renew_every`, release it when
    /// the guard is dropped.
    pub async fn keep_held(&self, model: &str, keep_alive: &str, renew_every: Duration) -> Result<HoldGuard> {
        self.hold(model, None, Some(keep_alive)).await?;
        let (client, m, k) = (self.clone(), model.to_string(), keep_alive.to_string());
        let renew = tokio::spawn(async move {
            loop {
                tokio::time::sleep(renew_every).await;
                let _ = client.hold(&m, None, Some(&k)).await;
            }
        });
        Ok(HoldGuard { client: self.clone(), model: model.to_string(), renew })
    }
}

fn bytes_part(audio: Vec<u8>, filename: &str) -> reqwest::multipart::Part {
    reqwest::multipart::Part::bytes(audio).file_name(filename.to_string())
}

/// Mono samples as 16-bit PCM WAV (kept here so the client stays light;
/// `localrouter_audio::wav_i16` is the same for apps that have it).
fn wav_pcm16(samples: &[f32], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    for part in [&b"RIFF"[..], &(36 + data_len).to_le_bytes(), b"WAVEfmt ", &16u32.to_le_bytes(), &1u16.to_le_bytes(), &1u16.to_le_bytes(),
        &rate.to_le_bytes(), &(rate * 2).to_le_bytes(), &2u16.to_le_bytes(), &16u16.to_le_bytes(), b"data", &data_len.to_le_bytes()]
    {
        wav.extend_from_slice(part);
    }
    for &s in samples {
        wav.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    wav
}

/// Ready for live windows: a hold in Local Router, or the model loaded in the app.
#[allow(clippy::large_enum_variant)] // one per recording
pub enum LiveReady {
    Held(HoldGuard),
    Loaded,
}

/// What a transcription is doing, for progress in the app.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscribeEvent {
    /// `decode`, `speakers` or `transcribe` (embedded only; Local Router
    /// does these steps on its side).
    Phase(&'static str),
    /// 0.0 to 1.0 of the current phase, when it's known.
    Fraction(f64),
    /// A finished segment; segments arrive in order and don't change later.
    Segment(TranscriptSegment),
}

/// One way to run speech in an app, with or without Local Router.
#[allow(clippy::large_enum_variant)] // one per app
#[derive(Clone)]
pub enum LocalAi {
    Connected(Client),
    #[cfg(feature = "embedded")]
    Embedded(embedded::Engine),
}

impl LocalAi {
    pub async fn transcribe(&self, audio: Vec<u8>, filename: &str, opts: &TranscribeOptions) -> Result<Transcript> {
        match self {
            LocalAi::Connected(c) => c.transcribe(audio, filename, opts).await,
            #[cfg(feature = "embedded")]
            LocalAi::Embedded(e) => {
                let e = e.clone();
                let (filename, opts) = (filename.to_string(), opts.clone());
                tokio::task::spawn_blocking(move || e.transcribe(audio, &filename, &opts))
                    .await
                    .map_err(|e| Error::Other(e.to_string()))?
            }
        }
    }

    /// [`LocalAi::transcribe`] with progress: phases, fractions and each
    /// segment as soon as it's final.
    pub async fn transcribe_with_progress(
        &self,
        audio: Vec<u8>,
        filename: &str,
        opts: &TranscribeOptions,
        on_event: impl FnMut(TranscribeEvent) + Send + 'static,
    ) -> Result<Transcript> {
        let mut on_event = on_event;
        match self {
            LocalAi::Connected(c) => {
                on_event(TranscribeEvent::Phase("transcribe"));
                c.transcribe_streaming(audio, filename, opts, move |s| on_event(TranscribeEvent::Segment(s))).await
            }
            #[cfg(feature = "embedded")]
            LocalAi::Embedded(e) => {
                let e = e.clone();
                let (filename, opts) = (filename.to_string(), opts.clone());
                tokio::task::spawn_blocking(move || e.transcribe_with_progress(audio, &filename, &opts, &mut on_event))
                    .await
                    .map_err(|e| Error::Other(e.to_string()))?
            }
        }
    }

    /// Transcribe a file with progress, read from disk as it goes (a
    /// multi-gigabyte video isn't loaded into memory first). `cancel` stops
    /// it: through Local Router the request is dropped; in the app the
    /// engine stops at its next window. Either way it returns
    /// [`Error::Cancelled`] at once, and `on_event` isn't called after that.
    pub async fn transcribe_file(
        &self,
        path: &Path,
        opts: &TranscribeOptions,
        cancel: &Cancel,
        on_event: impl FnMut(TranscribeEvent) + Send + 'static,
    ) -> Result<Transcript> {
        let mut on_event = on_event;
        match self {
            LocalAi::Connected(c) => {
                on_event(TranscribeEvent::Phase("transcribe"));
                cancel.run(c.transcribe_file_streaming(path, opts, move |s| on_event(TranscribeEvent::Segment(s)))).await
            }
            #[cfg(feature = "embedded")]
            LocalAi::Embedded(e) => {
                let (e, path, opts, stop) = (e.clone(), path.to_path_buf(), opts.clone(), cancel.clone());
                let job = tokio::task::spawn_blocking(move || e.transcribe_file_with_progress(&path, &opts, &stop, &mut on_event));
                cancel.run(async { job.await.map_err(|e| Error::Other(e.to_string()))? }).await
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, LocalAi::Connected(_))
    }

    /// Get the speech model ready before recording starts: held in Local
    /// Router (so live windows don't wait behind other work), or loaded here.
    pub async fn prepare_live(&self) -> Result<LiveReady> {
        match self {
            LocalAi::Connected(c) => Ok(LiveReady::Held(c.keep_held("localrouter/transcribe", "30m", Duration::from_secs(10 * 60)).await?)),
            #[cfg(feature = "embedded")]
            LocalAi::Embedded(e) => {
                let e = e.clone();
                tokio::task::spawn_blocking(move || e.preload()).await.map_err(|e| Error::Other(e.to_string()))??;
                Ok(LiveReady::Loaded)
            }
        }
    }

    /// One live window (≤ 30 s of mono 16 kHz audio); `opts.offset` is
    /// where it starts in the recording.
    pub async fn transcribe_window(&self, samples: std::sync::Arc<[f32]>, opts: &TranscribeOptions) -> Result<WindowResult> {
        match self {
            LocalAi::Connected(c) => c.transcribe_window(&samples, opts).await,
            #[cfg(feature = "embedded")]
            LocalAi::Embedded(e) => {
                let (e, opts) = (e.clone(), opts.clone());
                tokio::task::spawn_blocking(move || e.transcribe_window(&samples, &opts)).await.map_err(|e| Error::Other(e.to_string()))?
            }
        }
    }
}

/// The capabilities a manifest's roles need; handy for onboarding copy.
pub fn needed_capabilities(manifest: &ConnectRequest) -> Vec<Capability> {
    let mut caps: Vec<Capability> = manifest.roles.iter().map(|r| r.capability).collect();
    caps.dedup();
    caps
}

#[cfg(feature = "embedded")]
pub mod embedded {
    //! The speech engine inside the app, using the standard Hugging Face cache.
    use std::sync::{Arc, Mutex};

    use localrouter_speech::{Asr, AsrOptions, Diarizer, Strategy, Turn, catalog};

    use super::*;

    #[derive(Clone)]
    pub struct Engine {
        inner: Arc<Inner>,
    }

    struct Inner {
        cache: localrouter_hf::Cache,
        whisper: catalog::WhisperModel,
        /// Locked only to load or drop the model; Whisper's own state lock
        /// is taken per window, so a live window runs between a file's windows.
        asr: Mutex<Option<Arc<Asr>>>,
        diarizer: Mutex<Option<Diarizer>>,
        /// One file at a time; live windows don't wait for it (PROTOCOL.md 11.4).
        job: Mutex<()>,
    }

    fn err(e: impl std::fmt::Display) -> Error {
        Error::Other(e.to_string())
    }

    impl Engine {
        /// Uses `whisper` (a catalog model) from the standard cache; download
        /// missing models with [`catalog::ensure_whisper`] and
        /// [`catalog::ensure_speaker_models`] first.
        pub fn new(whisper: catalog::WhisperModel) -> anyhow::Result<Self> {
            Ok(Engine::with_cache(whisper, localrouter_hf::Cache::standard()?))
        }

        /// Like [`new`](Self::new), with the models in another cache.
        pub fn with_cache(whisper: catalog::WhisperModel, cache: localrouter_hf::Cache) -> Self {
            Engine { inner: Arc::new(Inner { cache, whisper, asr: Mutex::new(None), diarizer: Mutex::new(None), job: Mutex::new(()) }) }
        }

        pub fn transcribe(&self, audio: Vec<u8>, filename: &str, opts: &TranscribeOptions) -> Result<Transcript> {
            self.transcribe_with_progress(audio, filename, opts, &mut |_| {})
        }

        /// Whether the models this engine needs are in the cache.
        pub fn ready(&self, speakers: bool) -> bool {
            let asr = matches!(catalog::whisper_present(&self.inner.cache, self.inner.whisper), Ok(Some(_)));
            asr && (!speakers || matches!(catalog::speaker_models_present(&self.inner.cache), Ok(Some(_))))
        }

        /// The speech model, loaded on first use.
        fn asr(&self) -> Result<Arc<Asr>> {
            let mut slot = self.inner.asr.lock().unwrap();
            if let Some(asr) = slot.as_ref() {
                return Ok(asr.clone());
            }
            let w = self.inner.whisper;
            let path = catalog::whisper_present(&self.inner.cache, w).map_err(err)?.ok_or_else(|| err(format!("{} isn't downloaded", w.id)))?;
            let asr = Arc::new(Asr::load(&path, catalog::aheads_for(&w)).map_err(|e| err(format!("{e:#}")))?);
            *slot = Some(asr.clone());
            Ok(asr)
        }

        /// Load the speech model now, so the first window doesn't wait for it.
        pub fn preload(&self) -> Result<()> {
            self.asr().map(|_| ())
        }

        pub fn is_loaded(&self) -> bool {
            self.inner.asr.lock().unwrap().is_some()
        }

        /// Free the models' memory (once any running work is done with them).
        pub fn unload(&self) {
            *self.inner.asr.lock().unwrap() = None;
            *self.inner.diarizer.lock().unwrap() = None;
        }

        /// One live window: mono 16 kHz samples, at most 30 s. Speakers are
        /// ignored (live mode has none); `opts.offset` is added to every time.
        pub fn transcribe_window(&self, samples: &[f32], opts: &TranscribeOptions) -> Result<WindowResult> {
            let asr = self.asr()?;
            let asr_opts = AsrOptions { language: opts.language.clone(), prompt: opts.prompt.clone(), threads: 0 };
            let out = asr.transcribe_full(samples, opts.offset, &asr_opts).map_err(|e| err(format!("{e:#}")))?;
            Ok(WindowResult {
                language: out.language,
                segments: out
                    .segments
                    .into_iter()
                    .map(|s| WindowSegment {
                        start: s.start,
                        end: s.end,
                        text: s.text,
                        words: s.words.into_iter().map(|w| Word { word: w.word, start: w.start, end: w.end, probability: w.probability as f64 }).collect(),
                        no_speech_prob: Some(s.no_speech_prob),
                    })
                    .collect(),
            })
        }

        pub fn transcribe_with_progress(
            &self,
            audio: Vec<u8>,
            filename: &str,
            opts: &TranscribeOptions,
            on_event: &mut (dyn FnMut(TranscribeEvent) + Send),
        ) -> Result<Transcript> {
            // Files take turns; a live request goes between their windows.
            let _job = (opts.priority.as_deref() != Some("live")).then(|| self.inner.job.lock().unwrap_or_else(|e| e.into_inner()));
            on_event(TranscribeEvent::Phase("decode"));
            let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
            let audio = localrouter_audio::decode_bytes(audio, ext.as_deref()).map_err(|e| err(format!("{e:#}")))?;
            self.run(&audio, opts, &Cancel::default(), on_event)
        }

        /// [`transcribe_with_progress`](Self::transcribe_with_progress) of a
        /// file, decoded as it's read, stopping soon after `cancel`.
        pub fn transcribe_file_with_progress(&self, path: &Path, opts: &TranscribeOptions, cancel: &Cancel, on_event: &mut (dyn FnMut(TranscribeEvent) + Send)) -> Result<Transcript> {
            let _job = (opts.priority.as_deref() != Some("live")).then(|| self.inner.job.lock().unwrap_or_else(|e| e.into_inner()));
            let mut on_event = |e: TranscribeEvent| {
                if !cancel.is_cancelled() {
                    on_event(e)
                }
            };
            stop_if(cancel)?;
            on_event(TranscribeEvent::Phase("decode"));
            let audio = localrouter_audio::decode_file(path).map_err(|e| err(format!("{e:#}")))?;
            self.run(&audio, opts, cancel, &mut on_event)
        }

        /// Speakers (unless given), then the words. Between phases and windows
        /// `cancel` is looked at.
        fn run(&self, audio: &localrouter_audio::Audio, opts: &TranscribeOptions, cancel: &Cancel, on_event: &mut (dyn FnMut(TranscribeEvent) + Send)) -> Result<Transcript> {
            stop_if(cancel)?;
            let asr = self.asr()?;
            let turns: Option<Vec<Turn>> = match (&opts.segments, opts.speakers) {
                (Some(s), _) => Some(s.iter().map(|s| Turn { start: s.start, end: s.end, speaker: s.speaker.clone().unwrap_or_else(|| "S1".into()) }).collect()),
                (None, true) => {
                    on_event(TranscribeEvent::Phase("speakers"));
                    let mut slot = self.inner.diarizer.lock().unwrap();
                    if slot.is_none() {
                        let dir = catalog::speaker_models_present(&self.inner.cache)
                            .map_err(err)?
                            .ok_or_else(|| err("speaker models aren't downloaded"))?;
                        *slot = Some(Diarizer::load(&dir).map_err(|e| err(format!("{e:#}")))?);
                    }
                    stop_if(cancel)?;
                    let raw = slot.as_mut().unwrap().run(&audio.samples).map_err(|e| err(format!("{e:#}")))?;
                    Some(localrouter_speech::merge_same_speaker(&localrouter_speech::label_speakers(raw)))
                }
                (None, false) => None,
            };
            stop_if(cancel)?;
            on_event(TranscribeEvent::Phase("transcribe"));
            let asr_opts = AsrOptions { language: opts.language.clone(), prompt: opts.prompt.clone(), threads: 0 };
            let with_speakers = opts.speakers || opts.segments.is_some();
            let offset = opts.offset;
            let segment = |x: &localrouter_speech::TranscriptTurn| TranscriptSegment {
                start: x.start + offset,
                end: x.end + offset,
                speaker: with_speakers.then(|| short_label(&x.speaker)),
                text: x.text.clone(),
                words: x
                    .words
                    .iter()
                    .map(|w| Word { word: w.word.clone(), start: w.start + offset, end: w.end + offset, probability: w.probability as f64 })
                    .collect(),
            };
            let events = std::cell::RefCell::new(on_event);
            let t = until_cancelled(cancel, || {
                localrouter_speech::transcribe_turns_streaming(
                    audio,
                    &asr,
                    turns,
                    &asr_opts,
                    Strategy::Windows,
                    &mut |done, total| {
                        unwind_if(cancel);
                        (events.borrow_mut())(TranscribeEvent::Fraction(if total == 0 { 1.0 } else { done as f64 / total as f64 }))
                    },
                    &mut |turn| (events.borrow_mut())(TranscribeEvent::Segment(segment(turn))),
                )
            })?
            .map_err(|e| err(format!("{e:#}")))?;
            Ok(Transcript {
                language: t.language.clone().unwrap_or_default(),
                duration: t.duration_s,
                text: t.turns.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" "),
                segments: t.turns.iter().map(segment).collect(),
                model: Some(catalog::whisper_model_id(&self.inner.whisper)),
            })
        }
    }

    fn stop_if(cancel: &Cancel) -> Result<()> {
        if cancel.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }

    /// The speech engine's windows can't be told to stop, but it reports
    /// progress before each one: there, a cancelled job unwinds (without the
    /// panic hook, so nothing is printed) back to [`until_cancelled`]. The
    /// progress callback runs in Rust between windows, outside any lock the
    /// next job needs (the job lock is taken poison-tolerant). With
    /// `panic = "abort"` it doesn't unwind and the job runs to its end.
    struct Stopped;

    pub(crate) fn unwind_if(cancel: &Cancel) {
        #[cfg(panic = "unwind")]
        if cancel.is_cancelled() {
            std::panic::resume_unwind(Box::new(Stopped));
        }
        #[cfg(not(panic = "unwind"))]
        let _ = cancel;
    }

    pub(crate) fn until_cancelled<T>(cancel: &Cancel, work: impl FnOnce() -> T) -> Result<T> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
            Ok(v) => Ok(v),
            Err(p) if p.is::<Stopped>() => Err(Error::Cancelled),
            Err(p) => std::panic::resume_unwind(p),
        }
        .and_then(|v| stop_if(cancel).map(|_| v))
    }

    /// "Speaker 3" → "S3", as Local Router labels them.
    fn short_label(s: &str) -> String {
        s.strip_prefix("Speaker ").map(|n| format!("S{n}")).unwrap_or_else(|| s.to_string())
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nothing_there_means_standalone() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(discover_in(dir.path()).await, Presence::NotInstalled | Presence::Installed));
        // A stale discovery file (crash) isn't "running".
        let ep = Endpoint {
            protocols: vec![1],
            daemon_version: "2.0.0".into(),
            instance_id: "6f1d1c2e-8f0b-4b8e-9d55-0a3f0b6c3a10".into(),
            pid: 1,
            started_at: localrouter_core::now_rfc3339(),
            http: "http://127.0.0.1:11436".into(),
            socket: Some(dir.path().join("lr.sock").display().to_string()),
            hf_cache: localrouter_core::HfCacheInfo { path: None, state: "ok".into() },
            ollama: localrouter_core::OllamaInfo { url: String::new(), state: "stopped".into(), version: None },
            capabilities: vec![],
        };
        ep.write_atomic(&dir.path().join("endpoint.json")).unwrap();
        assert!(!matches!(discover_in(dir.path()).await, Presence::Running(_)));
    }

    #[tokio::test]
    async fn cancel_stops_waiting_at_once() {
        let cancel = Cancel::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            stop.cancel();
        });
        let hung = cancel.run(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        assert!(matches!(tokio::time::timeout(Duration::from_secs(5), hung).await, Ok(Err(Error::Cancelled))));
        assert!(cancel.is_cancelled());
        // Cancelled before it starts: the work isn't begun.
        assert!(matches!(cancel.run(async { Ok(1) }).await, Err(Error::Cancelled)));
        assert_eq!(Cancel::new().run(async { Ok(1) }).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_hung_call_times_out() {
        let r = within(Duration::from_millis(20), "the test", async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        })
        .await;
        assert!(matches!(r, Err(Error::Transport(m)) if m.contains("the test")));
    }

    #[cfg(feature = "embedded")]
    #[test]
    fn a_cancelled_engine_job_unwinds_quietly() {
        use embedded::{unwind_if, until_cancelled};
        let cancel = Cancel::new();
        let mut windows = 0;
        let r = until_cancelled(&cancel, || {
            for i in 0..10 {
                unwind_if(&cancel);
                windows += 1;
                if i == 2 {
                    cancel.cancel();
                }
            }
        });
        assert!(matches!(r, Err(Error::Cancelled)));
        assert_eq!(windows, 3, "stops at the next window");
        assert!(until_cancelled(&Cancel::new(), || 7).is_ok_and(|v| v == 7));
        // Other panics aren't swallowed.
        let other = std::panic::catch_unwind(|| until_cancelled(&Cancel::new(), || panic!("boom")));
        assert!(other.is_err());
    }

    #[test]
    fn windows_are_sent_as_16_bit_wav() {
        let wav = wav_pcm16(&[0.0, 1.0, -1.0, 2.0], 16_000);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 8);
        let pcm: Vec<i16> = wav[44..].chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(pcm, vec![0, 32767, -32767, 32767]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launch_services_lookup_has_no_false_positives() {
        assert!(launch_services::has_app("com.apple.finder"));
        assert!(!launch_services::has_app("ch.askantis.does-not-exist"));
    }

    #[test]
    fn transcripts_parse_from_the_protocol_formats() {
        let v = json!({ "task": "transcribe", "language": "de", "duration": 2.0, "text": "Hoi",
            "speakers": [{ "id": "S1" }], "segments": [{ "id": 0, "start": 0.1, "end": 0.5, "speaker": "S1", "text": "Hoi",
            "words": [{ "word": "Hoi", "start": 0.1, "end": 0.5, "probability": 0.9 }] }], "overlaps": [] });
        let t: Transcript = serde_json::from_value(v).unwrap();
        assert_eq!(t.segments[0].speaker.as_deref(), Some("S1"));
        assert_eq!(t.segments[0].words[0].word, "Hoi");
        let plain = json!({ "task": "transcribe", "language": "de", "duration": 2.0, "text": "Hoi", "segments": [{ "id": 0, "start": 0.1, "end": 0.5, "text": "Hoi" }] });
        let t: Transcript = serde_json::from_value(plain).unwrap();
        assert_eq!(t.segments[0].speaker, None);
    }
}

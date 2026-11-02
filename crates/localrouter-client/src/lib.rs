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
        let ok = std::process::Command::new("/usr/bin/open")
            .args(["-g", "-b", localrouter_core::BUNDLE_ID])
            .status()
            .map(|s| s.success())
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
        let http = reqwest::Client::builder()
            .unix_socket(PathBuf::from(socket))
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
        self.json(self.http.get(self.url("/lr/v1/health")).timeout(Duration::from_secs(3))).await
    }

    /// Connect once (PROTOCOL.md 5.2). Askantis-signed apps are approved right
    /// away; others wait until the user answers in Local Router, calling
    /// `waiting` once so the app can say "Allow it in Local Router".
    pub async fn connect(&self, manifest: &ConnectRequest, waiting: Option<&(dyn Fn() + Sync)>) -> Result<AppRecord> {
        let resp = self.send(self.http.post(self.url("/lr/v1/connect")).json(manifest)).await?;
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
            let s: Value = self.json(self.http.get(self.url(&poll))).await?;
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
        self.json(self.http.get(self.url("/lr/v1/me"))).await
    }

    pub async fn put_roles(&self, roles: &[RoleDeclaration]) -> Result<AppRecord> {
        self.json(self.http.put(self.url("/lr/v1/me/roles")).json(&json!({ "roles": roles }))).await
    }

    pub async fn disconnect(&self) -> Result<()> {
        self.send(self.http.delete(self.url("/lr/v1/me"))).await.map(|_| ())
    }

    pub async fn models(&self) -> Result<Vec<Value>> {
        let v: Value = self.json(self.http.get(self.url("/lr/v1/models"))).await?;
        Ok(v["models"].as_array().cloned().unwrap_or_default())
    }

    /// Tell Local Router about models the app runs itself (PROTOCOL.md 9).
    pub async fn report_resident(&self, models: &[Value], lease_seconds: u64) -> Result<()> {
        let body = json!({ "models": models, "lease_seconds": lease_seconds });
        self.send(self.http.put(self.url("/lr/v1/me/resident")).json(&body)).await.map(|_| ())
    }

    /// A chat turn through Ollama's API (non-streaming). `body` is Ollama's
    /// `/api/chat` request; its `model` may be `localrouter/<role>`.
    pub async fn chat(&self, mut body: Value) -> Result<Value> {
        body["stream"] = json!(false);
        self.json(self.http.post(self.url("/api/chat")).json(&body)).await
    }

    /// Speech to text; `audio` is the file's bytes and `filename` its name (for the format).
    pub async fn transcribe(&self, audio: Vec<u8>, filename: &str, opts: &TranscribeOptions) -> Result<Transcript> {
        let form = self.transcribe_form(audio, filename, opts);
        let resp = self.send(self.http.post(self.url("/v1/audio/transcriptions")).multipart(form)).await?;
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
        mut on_segment: impl FnMut(TranscriptSegment),
    ) -> Result<Transcript> {
        use futures::StreamExt;
        let form = self.transcribe_form(audio, filename, opts).text("stream", "true");
        let resp = self.send(self.http.post(self.url("/v1/audio/transcriptions")).multipart(form)).await?;
        let model = resp.headers().get("LR-Resolved-Model").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let mut body = resp.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = body.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk.map_err(|e| Error::Transport(e.to_string()))?));
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

    fn transcribe_form(&self, audio: Vec<u8>, filename: &str, opts: &TranscribeOptions) -> reqwest::multipart::Form {
        let mut form = reqwest::multipart::Form::new()
            .part("file", reqwest::multipart::Part::bytes(audio).file_name(filename.to_string()))
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
        self.json(self.http.post(self.url("/lr/v1/downloads")).json(request)).await
    }

    pub async fn download_status(&self, id: &str) -> Result<Download> {
        self.json(self.http.get(self.url(&format!("/lr/v1/downloads/{id}")))).await
    }

    /// Stop waiting for a download; it stops when no other app wants it.
    pub async fn cancel_download(&self, id: &str) -> Result<Download> {
        self.json(self.http.delete(self.url(&format!("/lr/v1/downloads/{id}")))).await
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
        self.json(self.http.post(self.url("/lr/v1/models/load")).json(&body)).await
    }

    pub async fn release(&self, model: &str) -> Result<()> {
        self.send(self.http.post(self.url("/lr/v1/models/unload")).json(&json!({ "model": model }))).await.map(|_| ())
    }

    /// Who speaks when (PROTOCOL.md 11.5).
    pub async fn diarize(&self, audio: Vec<u8>, filename: &str) -> Result<Vec<Segment>> {
        let form = reqwest::multipart::Form::new()
            .part("file", reqwest::multipart::Part::bytes(audio).file_name(filename.to_string()));
        let v: Value = self.json(self.http.post(self.url("/lr/v1/audio/diarizations")).multipart(form)).await?;
        serde_json::from_value(v["segments"].clone()).map_err(|e| Error::Other(e.to_string()))
    }
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

    pub fn is_connected(&self) -> bool {
        matches!(self, LocalAi::Connected(_))
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
        inner: Arc<Mutex<Inner>>,
    }

    struct Inner {
        cache: localrouter_hf::Cache,
        whisper: catalog::WhisperModel,
        asr: Option<Asr>,
        diarizer: Option<Diarizer>,
    }

    impl Engine {
        /// Uses `whisper` (a catalog model) from the standard cache; download
        /// missing models with [`catalog::ensure_whisper`] and
        /// [`catalog::ensure_speaker_models`] first.
        pub fn new(whisper: catalog::WhisperModel) -> anyhow::Result<Self> {
            let cache = localrouter_hf::Cache::standard()?;
            Ok(Engine { inner: Arc::new(Mutex::new(Inner { cache, whisper, asr: None, diarizer: None })) })
        }

        pub fn transcribe(&self, audio: Vec<u8>, filename: &str, opts: &TranscribeOptions) -> Result<Transcript> {
            self.transcribe_with_progress(audio, filename, opts, &mut |_| {})
        }

        /// Whether the models this engine needs are in the cache.
        pub fn ready(&self, speakers: bool) -> bool {
            let g = self.inner.lock().unwrap();
            let asr = matches!(catalog::whisper_present(&g.cache, g.whisper), Ok(Some(_)));
            asr && (!speakers || matches!(catalog::speaker_models_present(&g.cache), Ok(Some(_))))
        }

        pub fn transcribe_with_progress(
            &self,
            audio: Vec<u8>,
            filename: &str,
            opts: &TranscribeOptions,
            on_event: &mut (dyn FnMut(TranscribeEvent) + Send),
        ) -> Result<Transcript> {
            on_event(TranscribeEvent::Phase("decode"));
            let ext = filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
            let audio = localrouter_audio::decode_bytes(audio, ext.as_deref()).map_err(|e| Error::Other(format!("{e:#}")))?;
            let mut g = self.inner.lock().unwrap();
            let inner = &mut *g;
            if inner.asr.is_none() {
                let path = catalog::whisper_present(&inner.cache, inner.whisper)
                    .map_err(|e| Error::Other(e.to_string()))?
                    .ok_or_else(|| Error::Other(format!("{} isn't downloaded", inner.whisper.id)))?;
                inner.asr = Some(Asr::load(&path, catalog::aheads_for(&inner.whisper)).map_err(|e| Error::Other(format!("{e:#}")))?);
            }
            let turns: Option<Vec<Turn>> = match (&opts.segments, opts.speakers) {
                (Some(s), _) => Some(s.iter().map(|s| Turn { start: s.start, end: s.end, speaker: s.speaker.clone().unwrap_or_else(|| "S1".into()) }).collect()),
                (None, true) => {
                    on_event(TranscribeEvent::Phase("speakers"));
                    if inner.diarizer.is_none() {
                        let dir = catalog::speaker_models_present(&inner.cache)
                            .map_err(|e| Error::Other(e.to_string()))?
                            .ok_or_else(|| Error::Other("speaker models aren't downloaded".into()))?;
                        inner.diarizer = Some(Diarizer::load(&dir).map_err(|e| Error::Other(format!("{e:#}")))?);
                    }
                    let raw = inner.diarizer.as_mut().unwrap().run(&audio.samples).map_err(|e| Error::Other(format!("{e:#}")))?;
                    Some(localrouter_speech::merge_same_speaker(&localrouter_speech::label_speakers(raw)))
                }
                (None, false) => None,
            };
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
            let t = localrouter_speech::transcribe_turns_streaming(
                &audio,
                inner.asr.as_ref().unwrap(),
                turns,
                &asr_opts,
                Strategy::Windows,
                &mut |done, total| (events.borrow_mut())(TranscribeEvent::Fraction(if total == 0 { 1.0 } else { done as f64 / total as f64 })),
                &mut |turn| (events.borrow_mut())(TranscribeEvent::Segment(segment(turn))),
            )
            .map_err(|e| Error::Other(format!("{e:#}")))?;
            Ok(Transcript {
                language: t.language.clone().unwrap_or_default(),
                duration: t.duration_s,
                text: t.turns.iter().map(|x| x.text.as_str()).collect::<Vec<_>>().join(" "),
                segments: t.turns.iter().map(segment).collect(),
                model: Some(catalog::whisper_model_id(&inner.whisper)),
            })
        }
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

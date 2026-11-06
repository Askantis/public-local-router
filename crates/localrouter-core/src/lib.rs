//! Local Router Protocol v1 (`spec/PROTOCOL.md`): the types both sides share,
//! the error shape, where things live on disk, and the discovery file.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 1;
/// First TCP port tried; the next nine are fallbacks (PROTOCOL.md 3.1).
pub const TCP_PORT: u16 = 11436;
pub const TCP_PORT_LAST: u16 = 11445;
pub const BUNDLE_ID: &str = "ch.askantis.localrouter";
/// Askantis' Apple Developer team.
pub const ASKANTIS_TEAM: &str = "PB9T4S67XB";

// ─── Paths ──────────────────────────────────────────────────────────────────

/// `~/Library/Application Support/Local Router` (macOS). `LR_SUPPORT_DIR`
/// overrides it, for tests and development.
pub fn support_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LR_SUPPORT_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Local Router")
    } else if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(local).join("Askantis").join("Local Router")
    } else {
        home.join(".local/share/localrouter")
    }
}

pub fn socket_path() -> PathBuf {
    support_dir().join("lr.sock")
}

pub fn endpoint_path() -> PathBuf {
    support_dir().join("endpoint.json")
}

// ─── Discovery file (PROTOCOL.md 4.1) ──────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub protocols: Vec<u32>,
    pub daemon_version: String,
    pub instance_id: String,
    pub pid: u32,
    pub started_at: String,
    pub http: String,
    pub socket: Option<String>,
    pub hf_cache: HfCacheInfo,
    /// The Askantis models folder, the `hf` store's second folder (PROTOCOL.md 2).
    /// Missing in files written by older versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub askantis_models: Option<HfCacheInfo>,
    /// Where new `hf` models go: `askantis`, `huggingface` or `custom`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_target: Option<String>,
    /// The folder `download_target` stands for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_path: Option<String>,
    pub ollama: OllamaInfo,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HfCacheInfo {
    pub path: Option<String>,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OllamaInfo {
    pub url: String,
    pub state: String,
    #[serde(default)]
    pub version: Option<String>,
}

impl Endpoint {
    /// Write to a temporary file in the same folder, then rename, so readers
    /// never see half a file.
    pub fn write_atomic(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".endpoint.{}.tmp", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(serde_json::to_vec_pretty(self).map_err(io::Error::other)?.as_slice())?;
            f.sync_all()?;
        }
        fs::rename(tmp, path)
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
    }
}

pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

// ─── Errors (PROTOCOL.md 12) ────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    #[serde(skip)]
    pub status: u16,
    #[serde(rename = "type")]
    pub kind: String,
    pub code: String,
    pub message: String,
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ApiError {
    pub fn new(status: u16, kind: &str, code: &str, message: impl Into<String>) -> Self {
        ApiError { status, kind: kind.into(), code: code.into(), message: message.into(), param: None, details: None }
    }
    pub fn with_param(mut self, param: &str) -> Self {
        self.param = Some(param.into());
        self
    }
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", "invalid_request", message)
    }
    pub fn unauthorized() -> Self {
        Self::new(401, "authentication_error", "unauthorized", "This needs an lra_ key from Local Router.")
    }
    pub fn not_approved() -> Self {
        Self::new(403, "permission_error", "not_approved", "This app isn't connected to Local Router.")
    }
    pub fn identity_mismatch(message: impl Into<String>) -> Self {
        Self::new(403, "permission_error", "identity_mismatch", message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, "permission_error", "forbidden", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(404, "not_found_error", "not_found", message)
    }
    pub fn role_unbound(app_id: &str, role: &str) -> Self {
        Self::new(409, "invalid_request_error", "role_unbound", format!("No model is chosen for “{role}”."))
            .with_details(serde_json::json!({
                "role": role,
                "link": format!("localrouter://apps?app={app_id}&role={role}"),
            }))
    }
    pub fn model_not_downloaded(model: &str) -> Self {
        Self::new(409, "invalid_request_error", "model_not_downloaded", format!("{model} isn't downloaded."))
            .with_details(serde_json::json!({ "model": model }))
    }
    pub fn unsupported_media(message: impl Into<String>) -> Self {
        Self::new(415, "invalid_request_error", "unsupported_media_type", message)
    }
    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(413, "invalid_request_error", "payload_too_large", message)
    }
    pub fn upstream_unavailable(message: impl Into<String>) -> Self {
        Self::new(502, "upstream_error", "upstream_unavailable", message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "server_error", "internal", message)
    }

    pub fn body(&self) -> serde_json::Value {
        serde_json::json!({ "error": self })
    }
}

// ─── Shared payloads ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub protocols: Vec<u32>,
    pub daemon_version: String,
    pub instance_id: String,
    pub capabilities: Vec<String>,
    pub ollama: OllamaInfo,
    pub hf_cache: HfCacheInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub askantis_models: Option<HfCacheInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Chat,
    Embedding,
    Transcription,
    Diarization,
}

impl Capability {
    /// The well-known role a capability falls back to (PROTOCOL.md 6.2).
    pub fn well_known_role(self) -> &'static str {
        match self {
            Capability::Chat => "chat",
            Capability::Embedding => "embed",
            Capability::Transcription => "transcribe",
            Capability::Diarization => "diarize",
        }
    }
}

pub const WELL_KNOWN_ROLES: [(&str, Capability); 4] = [
    ("chat", Capability::Chat),
    ("embed", Capability::Embedding),
    ("transcribe", Capability::Transcription),
    ("diarize", Capability::Diarization),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub store: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl ModelRef {
    /// The string model id (PROTOCOL.md 2): an Ollama name, or `hf:org/repo[/file]`.
    pub fn model_id(&self) -> String {
        match (self.store.as_str(), &self.file) {
            ("hf", Some(f)) => format!("hf:{}/{}", self.id, f),
            ("hf", None) => format!("hf:{}", self.id),
            _ => self.id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleDeclaration {
    pub role: String,
    pub capability: Capability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_context: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub languages: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<ModelRef>,
    #[serde(default = "yes")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn yes() -> bool {
    true
}

/// `^[a-z0-9_.-]{1,32}$`
pub fn valid_role_name(role: &str) -> bool {
    (1..=32).contains(&role.len())
        && role.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// Reverse-DNS: at least two parts of `[A-Za-z0-9-]`.
pub fn valid_app_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('.').collect();
    id.len() <= 255
        && parts.len() >= 2
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectRequest {
    pub app_id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub icon_png: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub roles: Vec<RoleDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleBinding {
    pub role: String,
    pub capability: Capability,
    pub model: Option<String>,
    /// `app`, `global`, `suggestion` or `none`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub app_id: String,
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    pub trust: String,
    #[serde(default)]
    pub connected_at: Option<String>,
    #[serde(default)]
    pub last_used: Option<String>,
    pub roles: Vec<RoleBinding>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_file_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/endpoint.json");
        let ep = Endpoint {
            protocols: vec![1],
            daemon_version: "2.0.0".into(),
            instance_id: uuid::Uuid::new_v4().to_string(),
            pid: 42,
            started_at: now_rfc3339(),
            http: "http://127.0.0.1:11436".into(),
            socket: Some("/tmp/lr.sock".into()),
            hf_cache: HfCacheInfo { path: Some("/x/hub".into()), state: "ok".into() },
            askantis_models: Some(HfCacheInfo { path: Some("/x/Askantis/Models".into()), state: "ok".into() }),
            download_target: Some("askantis".into()),
            download_path: Some("/x/Askantis/Models".into()),
            ollama: OllamaInfo { url: "http://127.0.0.1:11434".into(), state: "running".into(), version: None },
            capabilities: vec!["connect".into()],
        };
        ep.write_atomic(&path).unwrap();
        assert_eq!(Endpoint::read(&path).unwrap(), ep);
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file left behind");
    }

    #[test]
    fn older_discovery_files_still_read() {
        // Written before the Askantis models folder existed.
        let old = r#"{"protocols":[1],"daemon_version":"0.1.0","instance_id":"x","pid":1,"started_at":"t",
            "http":"http://127.0.0.1:11436","socket":null,"hf_cache":{"path":"/x/hub","state":"ok"},
            "ollama":{"url":"u","state":"stopped"},"capabilities":[]}"#;
        let ep: Endpoint = serde_json::from_str(old).unwrap();
        assert_eq!(ep.askantis_models, None);
        assert_eq!(ep.download_target, None);
        let health: Health = serde_json::from_str(
            r#"{"protocols":[1],"daemon_version":"0.1.0","instance_id":"x","capabilities":[],
                "ollama":{"url":"u","state":"stopped"},"hf_cache":{"path":null,"state":"ok"}}"#,
        )
        .unwrap();
        assert!(health.askantis_models.is_none() && health.download_path.is_none());
    }

    #[test]
    fn validates_names() {
        assert!(valid_role_name("chapters") && valid_role_name("a.b-c_1"));
        assert!(!valid_role_name("Chapters") && !valid_role_name("") && !valid_role_name(&"x".repeat(33)));
        assert!(valid_app_id("app.askantis.scribely") && valid_app_id("com.example.my-app"));
        assert!(!valid_app_id("scribely") && !valid_app_id("app..x") && !valid_app_id("a.b/c"));
    }

    #[test]
    fn errors_have_the_protocol_shape() {
        let e = ApiError::role_unbound("app.x.y", "chapters");
        let v = e.body();
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["code"], "role_unbound");
        assert_eq!(v["error"]["details"]["link"], "localrouter://apps?app=app.x.y&role=chapters");
        assert!(v["error"].get("status").is_none());
    }

    #[test]
    fn model_refs_become_model_ids() {
        let r = |store: &str, id: &str, file: Option<&str>| ModelRef {
            store: store.into(),
            id: id.into(),
            file: file.map(Into::into),
            revision: None,
        };
        assert_eq!(r("ollama", "qwen3.5:9b", None).model_id(), "qwen3.5:9b");
        assert_eq!(r("hf", "avencera/speakrs-models", None).model_id(), "hf:avencera/speakrs-models");
        assert_eq!(r("hf", "ggerganov/whisper.cpp", Some("ggml-large-v3-turbo-q8_0.bin")).model_id(),
                   "hf:ggerganov/whisper.cpp/ggml-large-v3-turbo-q8_0.bin");
    }
}

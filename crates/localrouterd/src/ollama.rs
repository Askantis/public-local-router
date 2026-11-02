//! Ollama: what it has installed and loaded, and forwarding the allowed part
//! of its API (PROTOCOL.md 11.3).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use localrouter_core::Capability;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct OllamaModel {
    pub name: String,
    pub size: u64,
    pub format: Option<String>,
    pub quantization: Option<String>,
    pub parameters: Option<String>,
    pub capabilities: Vec<String>,
    pub context_length: Option<u32>,
}

impl OllamaModel {
    pub fn protocol_capabilities(&self) -> Vec<Capability> {
        let mut caps = Vec::new();
        if self.capabilities.iter().any(|c| c == "completion") {
            caps.push(Capability::Chat);
        }
        if self.capabilities.iter().any(|c| c == "embedding") {
            caps.push(Capability::Embedding);
        }
        caps
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Loaded {
    pub name: String,
    #[serde(default)]
    pub size_vram: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

pub struct Ollama {
    pub base: String,
    pub http: reqwest::Client,
    /// `/api/show` answers, by model digest: context length rarely changes.
    context_cache: Mutex<HashMap<String, Option<u32>>>,
}

#[derive(Deserialize)]
struct Tags {
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    name: String,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    details: TagDetails,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Deserialize, Default)]
struct TagDetails {
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    quantization_level: Option<String>,
    #[serde(default)]
    parameter_size: Option<String>,
}

impl Ollama {
    pub fn new(base: String) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("HTTP client");
        Ollama { base: base.trim_end_matches('/').to_string(), http, context_cache: Mutex::new(HashMap::new()) }
    }

    pub async fn version(&self) -> Option<String> {
        #[derive(Deserialize)]
        struct V {
            version: String,
        }
        let r = self.http.get(format!("{}/api/version", self.base)).timeout(Duration::from_secs(3)).send().await.ok()?;
        r.json::<V>().await.ok().map(|v| v.version)
    }

    pub async fn models(&self) -> anyhow::Result<Vec<OllamaModel>> {
        let tags: Tags = self
            .http
            .get(format!("{}/api/tags", self.base))
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let mut out = Vec::with_capacity(tags.models.len());
        for m in tags.models {
            let context_length = self.context_length(&m.name, &m.digest).await;
            out.push(OllamaModel {
                name: m.name,
                size: m.size,
                format: m.details.format,
                quantization: m.details.quantization_level,
                parameters: m.details.parameter_size.filter(|s| !s.is_empty()),
                capabilities: m.capabilities,
                context_length,
            });
        }
        Ok(out)
    }

    async fn context_length(&self, name: &str, digest: &str) -> Option<u32> {
        if let Some(c) = self.context_cache.lock().unwrap().get(digest) {
            return *c;
        }
        #[derive(Deserialize)]
        struct Show {
            #[serde(default)]
            model_info: HashMap<String, serde_json::Value>,
        }
        let show: Option<Show> = async {
            self.http
                .post(format!("{}/api/show", self.base))
                .json(&serde_json::json!({ "model": name }))
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()
        }
        .await;
        let ctx = show.and_then(|s| {
            s.model_info
                .iter()
                .find(|(k, _)| k.ends_with(".context_length"))
                .and_then(|(_, v)| v.as_u64())
                .map(|v| v as u32)
        });
        self.context_cache.lock().unwrap().insert(digest.to_string(), ctx);
        ctx
    }

    pub async fn loaded(&self) -> Vec<Loaded> {
        #[derive(Deserialize)]
        struct Ps {
            models: Vec<Loaded>,
        }
        let r = async {
            self.http
                .get(format!("{}/api/ps", self.base))
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .ok()?
                .json::<Ps>()
                .await
                .ok()
        };
        r.await.map(|p| p.models).unwrap_or_default()
    }
}

/// The Ollama endpoints apps may use; every other `/api/*` path is refused.
pub fn allowed(method: &str, path: &str) -> bool {
    matches!(
        (method, path),
        ("POST", "/api/chat")
            | ("POST", "/api/generate")
            | ("POST", "/api/embed")
            | ("POST", "/api/embeddings")
            | ("POST", "/api/show")
            | ("GET", "/api/tags")
            | ("GET", "/api/ps")
            | ("GET", "/api/version")
    )
}

/// Longest keep-alive an app may ask for (PROTOCOL.md 7.2).
pub const MAX_KEEP_ALIVE_S: i64 = 4 * 3600;

/// Ollama's keep-alive: seconds as a number, or "15m"-style text. Negative
/// (forever) and anything over the limit become the limit; an app can't pin a
/// model other apps share.
pub fn clamp_keep_alive(v: &serde_json::Value) -> serde_json::Value {
    let secs = match v {
        serde_json::Value::Number(n) => n.as_f64().map(|f| f as i64),
        serde_json::Value::String(s) => parse_duration(s),
        _ => None,
    };
    match secs {
        Some(s) if (0..=MAX_KEEP_ALIVE_S).contains(&s) => serde_json::json!(s),
        _ => serde_json::json!(MAX_KEEP_ALIVE_S),
    }
}

fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()? {
        'h' => (&s[..s.len() - 1], 3600.0),
        'm' => (&s[..s.len() - 1], 60.0),
        's' => (&s[..s.len() - 1], 1.0),
        _ => (s, 1.0),
    };
    num.parse::<f64>().ok().map(|n| (n * mult) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_running_and_reading_endpoints_pass() {
        for (m, p) in [("POST", "/api/chat"), ("POST", "/api/generate"), ("POST", "/api/embed"), ("GET", "/api/tags")] {
            assert!(allowed(m, p), "{m} {p}");
        }
        for (m, p) in [("POST", "/api/pull"), ("POST", "/api/create"), ("DELETE", "/api/delete"), ("POST", "/api/blobs/sha256:x"), ("POST", "/api/copy"), ("POST", "/api/push"), ("GET", "/api/chat")] {
            assert!(!allowed(m, p), "{m} {p}");
        }
    }

    #[test]
    fn keep_alive_is_limited() {
        assert_eq!(clamp_keep_alive(&serde_json::json!("15m")), serde_json::json!(900));
        assert_eq!(clamp_keep_alive(&serde_json::json!(60)), serde_json::json!(60));
        assert_eq!(clamp_keep_alive(&serde_json::json!(-1)), serde_json::json!(MAX_KEEP_ALIVE_S));
        assert_eq!(clamp_keep_alive(&serde_json::json!("24h")), serde_json::json!(MAX_KEEP_ALIVE_S));
        assert_eq!(clamp_keep_alive(&serde_json::json!("nonsense")), serde_json::json!(MAX_KEEP_ALIVE_S));
    }
}

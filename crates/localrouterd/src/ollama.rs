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

    /// Load `model`, or reset its unload timer: `keep_alive` seconds, or
    /// Ollama's own idle time when `None`. Pass the loaded context: another
    /// `num_ctx` makes Ollama load the model again.
    pub async fn hold(&self, model: &str, keep_alive: Option<u64>, num_ctx: Option<u32>, embedding: bool) -> anyhow::Result<()> {
        let mut body = serde_json::json!({ "model": model });
        if let Some(k) = keep_alive {
            body["keep_alive"] = serde_json::json!(k);
        }
        if let Some(c) = num_ctx {
            body["options"] = serde_json::json!({ "num_ctx": c });
        }
        // Embedding models can't generate, so they're loaded through /api/embed.
        let path = if embedding {
            body["input"] = serde_json::json!("");
            "/api/embed"
        } else {
            body["prompt"] = serde_json::json!("");
            body["stream"] = serde_json::json!(false);
            "/api/generate"
        };
        self.http
            .post(format!("{}{path}", self.base))
            .json(&body)
            .timeout(Duration::from_secs(300))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// `ollama pull`, with progress summed over the model's layers. Stops when
    /// `cancel` is set (closing the request stops Ollama's pull too).
    pub async fn pull(
        &self,
        name: &str,
        mut progress: impl FnMut(u64, Option<u64>),
        cancel: &std::sync::atomic::AtomicBool,
    ) -> anyhow::Result<()> {
        use futures::StreamExt;
        #[derive(Deserialize)]
        struct Line {
            #[serde(default)]
            status: String,
            #[serde(default)]
            digest: Option<String>,
            #[serde(default)]
            total: Option<u64>,
            #[serde(default)]
            completed: Option<u64>,
            #[serde(default)]
            error: Option<String>,
        }
        let resp = self
            .http
            .post(format!("{}/api/pull", self.base))
            .json(&serde_json::json!({ "model": name, "stream": true }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<Line>(&text).ok().and_then(|l| l.error).unwrap_or(text);
            anyhow::bail!("Ollama answered {status}: {msg}");
        }
        let mut layers: HashMap<String, (u64, u64)> = HashMap::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                anyhow::bail!("cancelled");
            }
            buf.extend_from_slice(&chunk?);
            while let Some(end) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=end).collect();
                let Ok(l) = serde_json::from_slice::<Line>(&line) else { continue };
                if let Some(e) = l.error {
                    anyhow::bail!("Ollama: {e}");
                }
                if let (Some(d), Some(total)) = (l.digest, l.total) {
                    layers.insert(d, (l.completed.unwrap_or(0), total));
                    let done = layers.values().map(|(c, _)| c).sum();
                    let total = layers.values().map(|(_, t)| t).sum();
                    progress(done, Some(total));
                }
                if l.status == "success" {
                    return Ok(());
                }
            }
        }
        anyhow::bail!("Ollama stopped answering before the pull finished")
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

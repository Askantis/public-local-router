//! The HTTP side of `localrouterd`: who is calling, what they may do, and
//! every endpoint of the Local Router Protocol v1 this version implements.

use std::collections::HashMap;
use std::convert::Infallible;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Multipart, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Extension, Json, Router};
use futures::Stream;
use localrouter_core::{
    ApiError, AppRecord, BUNDLE_ID, ASKANTIS_TEAM, Capability, ConnectRequest, Health, HfCacheInfo,
    OllamaInfo, PROTOCOL, RoleDeclaration, valid_app_id, valid_role_name,
};
use localrouter_identity::{Identity, Trust};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::UnixListener;

use crate::downloads::{self, DownloadRequest, Downloads, Plan};
use crate::events::{Audience, Events, visible_to};
use crate::holds::{self, Holds};
use crate::ollama::{self, Ollama};
use crate::speech::{self, Speech, TranscribeRequest};
use crate::state::{self, Candidate, KeyEntry, PendingRequests, PendingStatus, Resolution, Store};

pub const CAPABILITIES: &[&str] = &[
    "connect", "roles", "resident", "events", "downloads", "holds", "audio.transcriptions", "audio.diarizations", "chat",
    "embeddings", "ollama.passthrough",
];

/// Longest upload (PROTOCOL.md 11.4).
const MAX_UPLOAD: usize = 2 << 30;

pub struct AppState {
    pub instance_id: String,
    pub store: RwLock<Store>,
    pub pending: Mutex<PendingRequests>,
    pub events: Events,
    pub ollama: Ollama,
    pub speech: Arc<Speech>,
    pub hf: RwLock<HfCacheInfo>,
    /// Wakes the Hugging Face cache check (`POST /lr/v1/admin/hf_cache/check`).
    pub hf_check: tokio::sync::Notify,
    pub ollama_state: RwLock<OllamaInfo>,
    /// Models apps run themselves: approval key → (models, lease end).
    pub resident: Mutex<HashMap<String, Resident>>,
    pub downloads: Downloads,
    pub holds: Holds,
    pub shutdown: tokio::sync::Notify,
}

pub type Shared = Arc<AppState>;

impl AppState {
    pub fn speech_handle(&self) -> Arc<Speech> {
        self.speech.clone()
    }
}
pub type Resident = (Vec<Value>, Instant);

// ─── Who is calling ─────────────────────────────────────────────────────────

/// Identity of the program on the other end of a Unix-socket connection,
/// computed once when the connection opens.
#[derive(Clone)]
pub struct SocketPeer(pub Arc<Identity>);

impl Connected<axum::serve::IncomingStream<'_, UnixListener>> for SocketPeer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, UnixListener>) -> Self {
        SocketPeer(Arc::new(localrouter_identity::identify_fd(stream.io().as_raw_fd())))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Socket,
    Tcp,
}

pub enum Caller {
    Socket(Arc<Identity>),
    /// A TCP client and the index of its key, if it sent a valid one.
    Tcp(Option<String>),
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = Infallible;
    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        if let Some(ConnectInfo(SocketPeer(id))) = parts.extensions.get::<ConnectInfo<SocketPeer>>() {
            return Ok(Caller::Socket(id.clone()));
        }
        let key = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|s| s.trim().to_string());
        Ok(Caller::Tcp(key))
    }
}

struct E(ApiError);

impl IntoResponse for E {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.0.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(self.0.body())).into_response()
    }
}

impl From<ApiError> for E {
    fn from(e: ApiError) -> Self {
        E(e)
    }
}

type R<T> = Result<T, E>;

/// The approval key of a connected app (or `key:<id>` for a TCP key client).
fn connected(st: &AppState, caller: &Caller) -> Result<String, ApiError> {
    match caller {
        Caller::Socket(id) => {
            let key = id.approval_key();
            if st.store.read().unwrap().apps.contains_key(&key) {
                Ok(key)
            } else {
                Err(ApiError::not_approved())
            }
        }
        Caller::Tcp(secret) => {
            let secret = secret.as_deref().ok_or_else(ApiError::unauthorized)?;
            let store = st.store.read().unwrap();
            let i = store.key_by_secret(secret).ok_or_else(ApiError::unauthorized)?;
            Ok(format!("key:{}", store.keys[i].id))
        }
    }
}

fn is_admin(caller: &Caller) -> bool {
    match caller {
        Caller::Socket(id) => {
            cfg!(feature = "test-admin")
                || id.is_local_router(BUNDLE_ID, ASKANTIS_TEAM)
                || (cfg!(feature = "dev-admin") && id.identifier.as_deref() == Some(BUNDLE_ID))
        }
        Caller::Tcp(_) => false,
    }
}

fn admin(caller: &Caller) -> Result<(), ApiError> {
    if is_admin(caller) { Ok(()) } else { Err(ApiError::forbidden("Only Local Router itself can do this.")) }
}

/// Declared roles and the user's bindings for an app or key client.
fn roles_of(store: &Store, key: &str) -> (Vec<RoleDeclaration>, std::collections::BTreeMap<String, String>) {
    if let Some(id) = key.strip_prefix("key:")
        && let Some(k) = store.keys.iter().find(|k| k.id == id) {
            return (k.roles.clone(), k.bindings.clone());
        }
    store.apps.get(key).map(|a| (a.roles.clone(), a.bindings.clone())).unwrap_or_default()
}

fn touch(st: &AppState, key: &str) {
    let mut store = st.store.write().unwrap();
    let now = localrouter_core::now_rfc3339();
    if let Some(a) = store.apps.get_mut(key) {
        a.last_used = Some(now);
    } else if let Some(id) = key.strip_prefix("key:")
        && let Some(k) = store.keys.iter_mut().find(|k| k.id == id) {
            k.last_used = Some(now);
        }
}

// ─── Router ─────────────────────────────────────────────────────────────────

pub fn router(st: Shared, transport: Transport) -> Router {
    let mut r = Router::new()
        .route("/lr/v1/health", get(health))
        .route("/lr/v1/connect", post(connect))
        .route("/lr/v1/connect/{id}", get(connect_status))
        .route("/lr/v1/me", get(me).delete(disconnect))
        .route("/lr/v1/me/roles", put(put_roles))
        .route("/lr/v1/me/resident", put(put_resident))
        .route("/lr/v1/models", get(models))
        .route("/lr/v1/models/load", post(models_load))
        .route("/lr/v1/models/unload", post(models_unload))
        .route("/lr/v1/events", get(events))
        .route("/lr/v1/downloads", get(list_downloads).post(start_download))
        .route("/lr/v1/downloads/{id}", get(get_download).delete(cancel_download))
        .route("/lr/v1/audio/diarizations", post(diarizations).layer(DefaultBodyLimit::max(MAX_UPLOAD)))
        .route("/v1/audio/transcriptions", post(transcriptions).layer(DefaultBodyLimit::max(MAX_UPLOAD)))
        .route("/v1/models", get(openai_models))
        .route("/v1/chat/completions", post(openai_passthrough))
        .route("/v1/embeddings", post(openai_passthrough))
        .route("/api/{*rest}", any(ollama_passthrough))
        .route("/lr/v1/admin/apps", get(admin_apps))
        .route("/lr/v1/admin/apps/{app}", delete(admin_remove_app))
        .route("/lr/v1/admin/apps/{app}/roles/{role}", put(admin_bind_app_role))
        .route("/lr/v1/admin/roles/{role}", put(admin_bind_global))
        .route("/lr/v1/admin/connect/{id}", post(admin_answer))
        .route("/lr/v1/admin/keys", get(admin_keys).post(admin_create_key))
        .route("/lr/v1/admin/keys/{id}", delete(admin_revoke_key))
        .route("/lr/v1/admin/shutdown", post(admin_shutdown))
        .route("/lr/v1/admin/hf_cache/check", post(admin_check_hf_cache))
        .layer(DefaultBodyLimit::max(4 << 20))
        .layer(middleware::from_fn(request_id))
        .layer(Extension(transport))
        .with_state(st);
    if transport == Transport::Tcp {
        r = r.layer(middleware::from_fn(tcp_guard));
    }
    r
}

async fn request_id(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    if let Ok(v) = HeaderValue::from_str(&format!("req_{}", uuid::Uuid::new_v4().simple())) {
        resp.headers_mut().insert("LR-Request-Id", v);
    }
    resp
}

/// PROTOCOL.md 3.3: no web pages. Host must be loopback, no Origin, JSON only.
async fn tcp_guard(req: Request, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let host_ok = {
        let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
        matches!(name, "127.0.0.1" | "[::1]" | "localhost")
    };
    if !host_ok || req.headers().contains_key(header::ORIGIN) {
        return reject(req, ApiError::forbidden("Local Router doesn't answer web pages.")).await;
    }
    let path = req.uri().path();
    let is_upload = path == "/v1/audio/transcriptions" || path == "/lr/v1/audio/diarizations";
    if matches!(*req.method(), Method::POST | Method::PUT) && !is_upload {
        let ct = req.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
        if !ct.starts_with("application/json") {
            return reject(req, ApiError::unsupported_media("Send JSON with Content-Type: application/json.")).await;
        }
    }
    next.run(req).await
}

/// Answer with an error, after reading (up to 1 MB of) the request body: a
/// connection closed with unread data is reset, and the client would lose
/// the answer.
async fn reject(req: Request, err: ApiError) -> Response {
    let _ = axum::body::to_bytes(req.into_body(), 1 << 20).await;
    E(err).into_response()
}

// ─── Health, inventory ──────────────────────────────────────────────────────

async fn health(State(st): State<Shared>) -> Json<Health> {
    Json(Health {
        protocols: vec![PROTOCOL],
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        instance_id: st.instance_id.clone(),
        capabilities: CAPABILITIES.iter().map(|s| s.to_string()).collect(),
        ollama: st.ollama_state.read().unwrap().clone(),
        hf_cache: st.hf.read().unwrap().clone(),
    })
}

/// Speech models in the cache. Reading the disk can block (a removable drive
/// waiting for the user's permission), so it never runs on an async thread.
async fn installed_speech(st: &AppState) -> Vec<(String, Capability, String, u64)> {
    if st.hf.read().unwrap().state != "ok" {
        return vec![];
    }
    let speech = st.speech_handle();
    tokio::task::spawn_blocking(move || speech.installed()).await.unwrap_or_default()
}

struct Inventory {
    ollama: Vec<ollama::OllamaModel>,
    loaded: Vec<ollama::Loaded>,
    speech: Vec<(String, Capability, String, u64)>,
}

impl Inventory {
    async fn get(st: &AppState) -> Inventory {
        let ollama = st.ollama.models().await.unwrap_or_default();
        let loaded = st.ollama.loaded().await;
        Inventory { ollama, loaded, speech: installed_speech(st).await }
    }

    fn candidates(&self) -> Vec<Candidate> {
        let mut c: Vec<Candidate> = self
            .ollama
            .iter()
            .map(|m| Candidate { id: m.name.clone(), capabilities: m.protocol_capabilities(), context_length: m.context_length })
            .collect();
        c.extend(self.speech.iter().map(|(id, cap, _, _)| Candidate { id: id.clone(), capabilities: vec![*cap], context_length: None }));
        c
    }
}

fn record_for(st: &AppState, key: &str, inv: &Inventory) -> Option<AppRecord> {
    let store = st.store.read().unwrap();
    let candidates = inv.candidates();
    if let Some(entry) = store.apps.get(key) {
        return Some(state::app_record(entry, &store.global, &candidates));
    }
    let id = key.strip_prefix("key:")?;
    let k = store.keys.iter().find(|k| k.id == id)?;
    let entry = state::AppEntry {
        app_id: key.into(),
        name: k.name.clone(),
        version: None,
        publisher: None,
        trust: "key".into(),
        connected_at: k.created.clone(),
        last_used: k.last_used.clone(),
        roles: k.roles.clone(),
        bindings: k.bindings.clone(),
    };
    Some(state::app_record(&entry, &store.global, &candidates))
}

#[derive(Deserialize)]
struct ModelFilter {
    kind: Option<String>,
    store: Option<String>,
    state: Option<String>,
}

async fn models(State(st): State<Shared>, caller: Caller, Query(f): Query<ModelFilter>) -> R<Json<Value>> {
    let key = connected(&st, &caller).or_else(|e| if is_admin(&caller) { Ok("admin".into()) } else { Err(e) })?;
    let inv = Inventory::get(&st).await;
    let record = record_for(&st, &key, &inv);
    let bound = |id: &str| -> Vec<String> {
        record.as_ref().map(|r| r.roles.iter().filter(|b| b.model.as_deref() == Some(id)).map(|b| b.role.clone()).collect()).unwrap_or_default()
    };
    let speech_loaded = st.speech.loaded();
    let mut out: Vec<Value> = Vec::new();
    for m in &inv.ollama {
        let loaded = inv.loaded.iter().find(|l| l.name == m.name);
        let kind = if m.capabilities.iter().any(|c| c == "embedding") { "embedding" } else { "llm" };
        let mut caps: Vec<String> = m.protocol_capabilities().iter().map(|c| serde_json::to_value(c).unwrap().as_str().unwrap().to_string()).collect();
        caps.extend(m.capabilities.iter().filter(|c| matches!(c.as_str(), "vision" | "tools" | "thinking" | "audio")).cloned());
        let mut v = json!({
            "id": m.name, "store": "ollama", "kind": kind, "display_name": m.name,
            "format": m.format.clone().map(|f| if f == "safetensors" { "mlx".to_string() } else { f }).unwrap_or_else(|| "other".into()),
            "size_bytes": m.size, "capabilities": caps,
            "state": if loaded.is_some() { "loaded" } else { "available" },
            "bound_for_you": bound(&m.name),
        });
        if let Some(q) = &m.quantization { v["quantization"] = json!(q); }
        if let Some(p) = &m.parameters { v["parameters"] = json!(p); }
        if let Some(c) = m.context_length { v["context_length"] = json!(c); }
        if let Some(l) = loaded {
            let (holders, mine) = st.holds.count(&m.name, &key);
            v["loaded"] = json!({
                "memory_bytes": l.size_vram.max(l.size), "context": l.context_length, "expires_at": l.expires_at,
                "held_by_you": mine, "holders": holders,
            });
        }
        out.push(v);
    }
    for (id, cap, name, size) in &inv.speech {
        let kind = if *cap == Capability::Transcription { "asr" } else { "diarization" };
        out.push(json!({
            "id": id, "store": "hf", "kind": kind, "display_name": name,
            "format": if kind == "asr" { "ggml" } else { "other" }, "size_bytes": size,
            "capabilities": [serde_json::to_value(cap).unwrap()],
            "state": if speech_loaded.contains(id) { "loaded" } else { "available" },
            "bound_for_you": bound(id),
        }));
        if speech_loaded.contains(id) {
            let (holders, mine) = st.holds.count(id, &key);
            out.last_mut().unwrap()["loaded"] = json!({ "memory_bytes": size, "held_by_you": mine, "holders": holders });
        }
    }
    let now = Instant::now();
    for (owner, (list, until)) in st.resident.lock().unwrap().iter() {
        if *until < now {
            continue;
        }
        for m in list {
            let mine = owner == &key;
            let app_id = st.store.read().unwrap().apps.get(owner).map(|a| a.app_id.clone()).unwrap_or_default();
            out.push(json!({
                "id": if mine { format!("app:{}/{}", app_id, m["name"].as_str().unwrap_or("")) } else { "app:other".to_string() },
                "store": "app", "kind": m["kind"], "display_name": if mine { m["display_name"].clone() } else { json!("Model in another app") },
                "state": m["state"], "loaded": { "memory_bytes": m["memory_bytes"] },
            }));
        }
    }
    out.retain(|m| {
        f.kind.as_deref().is_none_or(|k| m["kind"] == k)
            && f.store.as_deref().is_none_or(|s| m["store"] == s)
            && f.state.as_deref().is_none_or(|s| m["state"] == s)
    });
    Ok(Json(json!({ "models": out })))
}

async fn openai_models(State(st): State<Shared>, caller: Caller) -> R<Json<Value>> {
    let key = connected(&st, &caller)?;
    let inv = Inventory::get(&st).await;
    let mut data: Vec<Value> = inv.ollama.iter().map(|m| json!({ "id": m.name, "object": "model", "owned_by": "ollama" })).collect();
    data.extend(inv.speech.iter().map(|(id, ..)| json!({ "id": id, "object": "model", "owned_by": "localrouter" })));
    if let Some(r) = record_for(&st, &key, &inv) {
        data.extend(r.roles.iter().filter(|b| b.model.is_some()).map(|b| json!({ "id": format!("localrouter/{}", b.role), "object": "model", "owned_by": "localrouter" })));
    }
    Ok(Json(json!({ "object": "list", "data": data })))
}

// ─── Connecting (PROTOCOL.md 5.2) ───────────────────────────────────────────

fn clean_text(s: &str, max: usize) -> String {
    // No control characters or bidirectional overrides in what the prompt shows.
    s.chars()
        .filter(|c| !c.is_control() && !matches!(*c as u32, 0x200E | 0x200F | 0x202A..=0x202E | 0x2066..=0x2069))
        .take(max)
        .collect()
}

async fn connect(State(st): State<Shared>, caller: Caller, Json(mut req): Json<ConnectRequest>) -> R<Response> {
    let Caller::Socket(id) = caller else {
        return Err(ApiError::forbidden("Connect over Local Router's socket; tools that use TCP need a key from Local Router.").into());
    };
    if id.interpreter {
        return Err(ApiError::forbidden(format!(
            "{} is an interpreter: approving it would approve every script it runs. Create a key in Local Router instead.",
            id.path.as_deref().unwrap_or("This program")
        ))
        .into());
    }
    if !valid_app_id(&req.app_id) {
        return Err(ApiError::invalid("app_id must be reverse-DNS, like app.example.notes").with_param("app_id").into());
    }
    if let Some(bad) = req.roles.iter().find(|r| !valid_role_name(&r.role)) {
        return Err(ApiError::invalid(format!("invalid role name {:?}", bad.role)).with_param("roles").into());
    }
    req.name = clean_text(&req.name, 60);
    req.reason = req.reason.as_deref().map(|r| clean_text(r, 140));
    req.icon_png = None; // the prompt shows the signed bundle's icon, never one the app sends
    let trust = match id.trust {
        Trust::Askantis => "askantis",
        Trust::Signed => "signed",
        Trust::Unsigned => "unsigned",
    };
    let key = id.approval_key();
    if id.trust != Trust::Unsigned && id.identifier.as_deref() != Some(req.app_id.as_str()) {
        return Err(ApiError::identity_mismatch(format!(
            "app_id {} doesn't match the code signature ({}).",
            req.app_id,
            id.identifier.as_deref().unwrap_or("none")
        ))
        .into());
    }
    {
        let mut store = st.store.write().unwrap();
        if id.trust == Trust::Unsigned && !store.apps.contains_key(&key) && store.app_id_owned_by_signed(&req.app_id) {
            return Err(ApiError::identity_mismatch(format!("{} belongs to a signed app.", req.app_id)).into());
        }
        let auto = id.trust == Trust::Askantis && store.settings.auto_connect_askantis;
        if let Some(entry) = store.apps.get_mut(&key) {
            entry.version = Some(req.version.clone());
            if !req.roles.is_empty() {
                entry.roles = req.roles.clone();
            }
        } else if auto {
            store.apps.insert(key.clone(), state::new_entry(&req, trust));
        } else {
            drop(store);
            let mut pending = st.pending.lock().unwrap();
            pending.expire();
            let request_id = match pending.waiting_for(&key) {
                Some(existing) => existing,
                None => {
                    let rid = PendingRequests::new_id();
                    pending.by_id.insert(
                        rid.clone(),
                        state::Pending {
                            identity_key: key.clone(),
                            trust: trust.into(),
                            request: req.clone(),
                            status: PendingStatus::Pending,
                            created: Instant::now(),
                        },
                    );
                    st.events.send(
                        "connect.updated",
                        json!({
                            "request_id": rid, "status": "pending", "app_id": req.app_id, "name": req.name,
                            "trust": trust, "verified": id.trust != Trust::Unsigned, "identifier": id.identifier,
                            "path": id.path, "reason": req.reason, "why_unverified": id.reason,
                        }),
                        Audience::Admin,
                    );
                    rid
                }
            };
            let body = json!({ "status": "pending", "request_id": request_id, "poll": format!("/lr/v1/connect/{request_id}") });
            return Ok((StatusCode::ACCEPTED, Json(body)).into_response());
        }
        let _ = store.save();
    }
    let inv = Inventory::get(&st).await;
    let record = record_for(&st, &key, &inv);
    Ok(Json(json!({ "status": "approved", "app": record })).into_response())
}

async fn connect_status(State(st): State<Shared>, caller: Caller, Path(rid): Path<String>) -> R<Json<Value>> {
    let Caller::Socket(id) = caller else { return Err(ApiError::not_found("unknown request").into()) };
    let status = {
        let mut pending = st.pending.lock().unwrap();
        pending.expire();
        match pending.by_id.get(&rid) {
            Some(p) if p.identity_key == id.approval_key() => p.status,
            _ => return Err(ApiError::not_found("unknown request").into()),
        }
    };
    let mut body = json!({ "status": status });
    if status == PendingStatus::Approved {
        let inv = Inventory::get(&st).await;
        body["app"] = json!(record_for(&st, &id.approval_key(), &inv));
    }
    Ok(Json(body))
}

async fn me(State(st): State<Shared>, caller: Caller) -> R<Json<Value>> {
    let key = connected(&st, &caller)?;
    let inv = Inventory::get(&st).await;
    Ok(Json(json!(record_for(&st, &key, &inv))))
}

async fn disconnect(State(st): State<Shared>, caller: Caller) -> R<StatusCode> {
    let key = connected(&st, &caller)?;
    if key.starts_with("key:") {
        return Err(ApiError::forbidden("Keys are removed in Local Router.").into());
    }
    {
        let mut store = st.store.write().unwrap();
        store.apps.remove(&key);
        let _ = store.save();
    }
    st.resident.lock().unwrap().remove(&key);
    for model in st.holds.remove_app(&key) {
        release(&st, &model).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ─── Holding models (PROTOCOL.md 7.2) ──────────────────────────────────────

#[derive(Deserialize)]
struct LoadBody {
    model: String,
    #[serde(default)]
    context: Option<u32>,
    #[serde(default)]
    keep_alive: Option<Value>,
}

async fn models_load(State(st): State<Shared>, caller: Caller, Json(b): Json<LoadBody>) -> R<Json<Value>> {
    let key = connected(&st, &caller)?;
    let keep = holds::parse_keep(b.keep_alive.as_ref()).map_err(|m| ApiError::invalid(m).with_param("keep_alive"))?;
    let (model, min_ctx) = resolve_model(&st, &key, &b.model).await?;
    touch(&st, &key);

    if Speech::knows(&model) {
        let speech = st.speech_handle();
        let id = model.clone();
        tokio::task::spawn_blocking(move || speech.preload(&id))
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?
            .map_err(|_| ApiError::model_not_downloaded(&model))?;
        st.holds.add(&model, &key, None, keep);
        st.events.send("model.state", json!({ "id": model, "state": "loaded" }), Audience::Everyone);
        return Ok(Json(json!({ "model": model, "state": "loaded" })));
    }

    let inv = Inventory::get(&st).await;
    let Some(m) = inv.ollama.iter().find(|m| m.name == model) else { return Err(ApiError::model_not_downloaded(&model).into()) };
    let embedding = m.capabilities.iter().any(|c| c == "embedding");
    let loaded_ctx = inv.loaded.iter().find(|l| l.name == model).and_then(|l| l.context_length);
    let wanted = b.context.or(min_ctx);
    // A bigger context means loading again, which other holders would feel.
    if let Some(want) = wanted
        && st.holds.others(&model, &key).is_some()
        && loaded_ctx.is_some_and(|l| l < want)
    {
        return Err(ApiError::new(409, "invalid_request_error", "context_in_use", format!("{model} is in use with a smaller context."))
            .with_details(json!({ "loaded_context": loaded_ctx, "requested_context": want }))
            .into());
    }
    let ctx = st.holds.add(&model, &key, wanted, keep).or(loaded_ctx);
    // Ollama keeps it a little longer than the hold; the hold decides.
    if let Err(e) = st.ollama.hold(&model, Some(keep.as_secs() + 60), ctx, embedding).await {
        st.holds.remove(&model, &key);
        return Err(ApiError::upstream_unavailable(format!("Ollama couldn't load {model}: {e}")).into());
    }
    st.events.send("model.state", json!({ "id": model, "state": "loaded" }), Audience::Everyone);
    let expires = time::OffsetDateTime::now_utc() + keep;
    Ok(Json(json!({
        "model": model, "state": "loaded", "context": ctx,
        "expires_at": expires.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    })))
}

#[derive(Deserialize)]
struct UnloadBody {
    model: String,
}

async fn models_unload(State(st): State<Shared>, caller: Caller, Json(b): Json<UnloadBody>) -> R<StatusCode> {
    let key = connected(&st, &caller)?;
    let (model, _) = resolve_model(&st, &key, &b.model).await?;
    if st.holds.remove(&model, &key) {
        release(&st, &model).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Nobody holds `model` any more. Speech models are freed; Ollama models
/// go back to Ollama's own idle time (Local Router's window re-pins models
/// the user pinned).
pub async fn release(st: &AppState, model: &str) {
    if Speech::knows(model) {
        st.speech.unload(model);
        st.events.send("model.state", json!({ "id": model, "state": "available" }), Audience::Everyone);
        return;
    }
    let inv = Inventory::get(st).await;
    let Some(loaded) = inv.loaded.iter().find(|l| l.name == model) else { return };
    let embedding = inv.ollama.iter().any(|m| m.name == model && m.capabilities.iter().any(|c| c == "embedding"));
    let _ = st.ollama.hold(model, None, loaded.context_length, embedding).await;
}

#[derive(Deserialize)]
struct RolesBody {
    roles: Vec<RoleDeclaration>,
}

async fn put_roles(State(st): State<Shared>, caller: Caller, Json(b): Json<RolesBody>) -> R<Json<Value>> {
    let key = connected(&st, &caller)?;
    if let Some(bad) = b.roles.iter().find(|r| !valid_role_name(&r.role)) {
        return Err(ApiError::invalid(format!("invalid role name {:?}", bad.role)).with_param("roles").into());
    }
    {
        let mut store = st.store.write().unwrap();
        if let Some(id) = key.strip_prefix("key:") {
            if let Some(k) = store.keys.iter_mut().find(|k| k.id == id) {
                k.roles = b.roles;
            }
        } else if let Some(a) = store.apps.get_mut(&key) {
            a.roles = b.roles;
        }
        let _ = store.save();
    }
    let inv = Inventory::get(&st).await;
    Ok(Json(json!(record_for(&st, &key, &inv))))
}

#[derive(Deserialize)]
struct ResidentBody {
    models: Vec<Value>,
    #[serde(default)]
    lease_seconds: Option<u64>,
}

async fn put_resident(State(st): State<Shared>, caller: Caller, Json(b): Json<ResidentBody>) -> R<StatusCode> {
    let key = connected(&st, &caller)?;
    let lease = Duration::from_secs(b.lease_seconds.unwrap_or(60).clamp(10, 3600));
    for m in &b.models {
        let name = m["name"].as_str().unwrap_or("");
        if !valid_role_name(name) || !m["memory_bytes"].is_u64() {
            return Err(ApiError::invalid("each model needs a name ([a-z0-9_.-]) and memory_bytes").with_param("models").into());
        }
    }
    st.resident.lock().unwrap().insert(key, (b.models, Instant::now() + lease));
    Ok(StatusCode::NO_CONTENT)
}

// ─── Events ─────────────────────────────────────────────────────────────────

async fn events(
    State(st): State<Shared>,
    caller: Caller,
    headers: HeaderMap,
) -> R<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    let admin = is_admin(&caller);
    let key = if admin { None } else { Some(connected(&st, &caller)?) };
    let last: Option<u64> = headers.get("Last-Event-ID").and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok());
    let mut rx = st.events.subscribe();
    let backlog = match last {
        Some(id) => st.events.since(id),
        None => Some(Vec::new()),
    };
    let instance = st.instance_id.clone();
    let stream = async_stream(move |tx| async move {
        let to_sse = |e: &crate::events::Event| SseEvent::default().id(e.id.to_string()).event(&e.kind).data(e.data.to_string());
        match backlog {
            None => {
                let _ = tx.send(SseEvent::default().event("reset").data(json!({ "instance_id": instance }).to_string())).await;
            }
            Some(list) => {
                for e in list.iter().filter(|e| visible_to(e, key.as_deref(), admin)) {
                    if tx.send(to_sse(e)).await.is_err() {
                        return;
                    }
                }
            }
        }
        loop {
            match rx.recv().await {
                Ok(e) if visible_to(&e, key.as_deref(), admin) => {
                    if tx.send(to_sse(&e)).await.is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let _ = tx.send(SseEvent::default().event("reset").data(json!({ "instance_id": instance }).to_string())).await;
                }
                Err(_) => return,
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive")))
}

/// A stream fed by a task, so the SSE body ends when the client goes away.
fn async_stream<F, Fut>(f: F) -> impl Stream<Item = Result<SseEvent, Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<SseEvent>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(f(tx));
    futures::StreamExt::map(tokio_stream::wrappers::ReceiverStream::new(rx), Ok)
}

// ─── Downloads (PROTOCOL.md 8) ─────────────────────────────────────────────

/// Whose download it is: a connected app, or Local Router's own window.
fn download_key(st: &AppState, caller: &Caller) -> Result<String, ApiError> {
    match connected(st, caller) {
        Ok(key) => Ok(key),
        Err(_) if is_admin(caller) => Ok(downloads::ADMIN.into()),
        Err(e) => Err(e),
    }
}

/// Check a request and decide what to download.
fn plan_for(caller: &Caller, req: DownloadRequest) -> Result<Plan, ApiError> {
    let DownloadRequest { model, source, token, mirror } = req;
    let revision = model.revision.clone().unwrap_or_else(|| "main".into());
    if !localrouter_hf::is_valid_revision(&revision) {
        return Err(ApiError::invalid("revision isn't a branch name or commit hash").with_param("model.revision"));
    }
    match source.as_str() {
        "ollama" => {
            if model.store != "ollama" || model.file.is_some() || mirror.is_some() {
                return Err(ApiError::invalid("an ollama download takes an Ollama model name").with_param("model"));
            }
            if !downloads::valid_ollama_name(&model.id) {
                return Err(ApiError::invalid("not an Ollama model name").with_param("model.id"));
            }
            Ok(Plan::Ollama { name: model.id })
        }
        "hf" => {
            if model.store != "hf" || mirror.is_some() || !localrouter_hf::is_valid_repo(&model.id) {
                return Err(ApiError::invalid("an hf download takes a repository id like org/name").with_param("model.id"));
            }
            match model.file {
                Some(file) if !localrouter_hf::is_valid_file(&file) => {
                    Err(ApiError::invalid("not a file path inside the repository").with_param("model.file"))
                }
                Some(file) => Ok(Plan::HfFile { repo: model.id, file, revision, token }),
                None => Ok(Plan::HfRepo { repo: model.id, revision, token }),
            }
        }
        "mirror" => {
            let mirror = mirror.ok_or_else(|| ApiError::invalid("a mirror download needs `mirror`").with_param("mirror"))?;
            if model.store != "hf" || !localrouter_hf::is_valid_repo(&model.id) {
                return Err(ApiError::invalid("a mirror download takes a repository id like org/name").with_param("model.id"));
            }
            // Rule 1: signed apps only, into their own organisation.
            let Caller::Socket(id) = caller else {
                return Err(ApiError::forbidden("Mirror downloads are for signed apps."));
            };
            let orgs: &[&str] = if cfg!(feature = "test-admin") {
                &["askantis"]
            } else if matches!(id.trust, Trust::Askantis | Trust::Signed) {
                downloads::mirror_orgs(id.team.as_deref())
            } else {
                &[]
            };
            let org = model.id.split('/').next().unwrap_or_default();
            if !orgs.contains(&org) {
                return Err(ApiError::forbidden(format!("This app can't place files in {org}/ repositories.")));
            }
            if mirror.files.is_empty() || mirror.files.len() > 1000 {
                return Err(ApiError::invalid("a mirror lists 1 to 1000 files").with_param("mirror.files"));
            }
            Ok(Plan::Mirror { repo: model.id, commit: mirror.revision, files: mirror.files })
        }
        _ => Err(ApiError::invalid("source is ollama, hf or mirror").with_param("source")),
    }
}

/// Whether there's nothing to do (PROTOCOL.md 8.2: no second download).
async fn already_present(st: &AppState, plan: &Plan) -> bool {
    match plan {
        Plan::Ollama { name } => {
            let want = if name.contains(':') { name.clone() } else { format!("{name}:latest") };
            st.ollama.models().await.is_ok_and(|ms| ms.iter().any(|m| m.name == want))
        }
        Plan::HfFile { repo, file, revision, .. } => {
            let Some(cache) = st.speech.cache() else { return false };
            let (repo, file, revision) = (repo.clone(), file.clone(), revision.clone());
            tokio::task::spawn_blocking(move || cache.snapshot_path(&repo, &file, &revision).ok().flatten().is_some())
                .await
                .unwrap_or(false)
        }
        // Present files are skipped without going online.
        Plan::HfRepo { .. } | Plan::Mirror { .. } => false,
    }
}

fn download_record(st: &AppState, job: &downloads::Job, key: &str) -> Value {
    let mut v = job.record(Some(key));
    if key == downloads::ADMIN {
        // Local Router's window shows who asked (PROTOCOL.md 8.2).
        let store = st.store.read().unwrap();
        let names: Vec<String> = job
            .starters
            .lock()
            .unwrap()
            .iter()
            .map(|k| match store.apps.get(k) {
                Some(a) => a.name.clone(),
                None if k == downloads::ADMIN => "Local Router".into(),
                None => k.clone(),
            })
            .collect();
        v["started_by"] = json!(names);
    }
    v
}

async fn start_download(State(st): State<Shared>, caller: Caller, Json(req): Json<DownloadRequest>) -> R<Response> {
    let key = download_key(&st, &caller)?;
    let plan = plan_for(&caller, req)?;
    if let Plan::Mirror { files, .. } = &plan {
        let cache = st.speech.cache().ok_or_else(|| ApiError::new(503, "server_error", "internal", "The Hugging Face cache isn't available."))?;
        #[cfg(feature = "test-admin")]
        let cache = cache.with_mirror_guard(localrouter_hf::MirrorGuard::AllowLoopbackForTests);
        cache.check_mirror_files(files).map_err(downloads::hf_error)?;
    }
    let present = already_present(&st, &plan).await;
    let (job, new) = st.downloads.start(&key, &plan);
    if new && present {
        job.progress.lock().unwrap().state = downloads::State::Running;
        job.finish_now(&st.events);
        return Ok((StatusCode::OK, Json(download_record(&st, &job, &key))).into_response());
    }
    if new {
        tokio::spawn(downloads::run(job.clone(), plan, st.clone()));
    }
    Ok((StatusCode::ACCEPTED, Json(download_record(&st, &job, &key))).into_response())
}

async fn list_downloads(State(st): State<Shared>, caller: Caller) -> R<Json<Value>> {
    let key = download_key(&st, &caller)?;
    let list: Vec<Value> = st.downloads.list(&key).iter().map(|j| download_record(&st, j, &key)).collect();
    Ok(Json(json!({ "downloads": list })))
}

async fn get_download(State(st): State<Shared>, caller: Caller, Path(id): Path<String>) -> R<Json<Value>> {
    let key = download_key(&st, &caller)?;
    let job = st.downloads.get(&key, &id).ok_or_else(|| ApiError::not_found("unknown download"))?;
    Ok(Json(download_record(&st, &job, &key)))
}

async fn cancel_download(State(st): State<Shared>, caller: Caller, Path(id): Path<String>) -> R<Json<Value>> {
    let key = download_key(&st, &caller)?;
    let job = st.downloads.get(&key, &id).ok_or_else(|| ApiError::not_found("unknown download"))?;
    st.downloads.cancel(&key, &job);
    Ok(Json(job.record(Some(&key))))
}

// ─── Language models: Ollama and OpenAI pass-through ───────────────────────

/// Resolve `localrouter/<role>` for this caller; returns the model and the
/// role's minimum context, if any.
async fn resolve_model(st: &AppState, key: &str, model: &str) -> Result<(String, Option<u32>), ApiError> {
    let Some(role) = model.strip_prefix("localrouter/") else { return Ok((model.to_string(), None)) };
    let (declared, bindings) = roles_of(&st.store.read().unwrap(), key);
    let decl = state::declaration(&declared, role).ok_or_else(|| ApiError::role_unbound(key, role))?;
    let inv = Inventory::get(st).await;
    let global = st.store.read().unwrap().global.clone();
    match state::resolve(&decl, &bindings, &global, &inv.candidates()) {
        Resolution::Model { id, .. } => Ok((id, decl.min_context)),
        Resolution::BoundButMissing(id) => Err(ApiError::model_not_downloaded(&id)),
        Resolution::Unbound => Err(ApiError::role_unbound(&app_id_of(st, key), role)),
    }
}

fn app_id_of(st: &AppState, key: &str) -> String {
    st.store.read().unwrap().apps.get(key).map(|a| a.app_id.clone()).unwrap_or_else(|| key.to_string())
}

fn ollama_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

async fn ollama_passthrough(State(st): State<Shared>, caller: Caller, method: Method, Path(rest): Path<String>, body: Bytes) -> Response {
    let path = format!("/api/{rest}");
    let key = match connected(&st, &caller) {
        Ok(k) => k,
        Err(e) => return ollama_error(StatusCode::from_u16(e.status).unwrap_or(StatusCode::FORBIDDEN), &e.message),
    };
    if !ollama::allowed(method.as_str(), &path) {
        return ollama_error(StatusCode::FORBIDDEN, "Not available through Local Router. Apps download models with /lr/v1/downloads.");
    }
    let mut resolved = None;
    let body = if method == Method::POST && !body.is_empty() {
        let Ok(mut v) = serde_json::from_slice::<Value>(&body) else { return ollama_error(StatusCode::BAD_REQUEST, "invalid JSON") };
        if let Some(m) = v.get("model").and_then(|m| m.as_str()).map(str::to_owned) {
            match resolve_model(&st, &key, &m).await {
                Ok((id, min_ctx)) => {
                    v["model"] = json!(id);
                    resolved = Some(id);
                    if matches!(path.as_str(), "/api/chat" | "/api/generate") {
                        let lr_ctx = v.get("lr_context").and_then(|c| c.as_u64()).map(|c| c as u32);
                        if v.pointer("/options/num_ctx").is_none()
                            && let Some(ctx) = lr_ctx.or(min_ctx) {
                                if !v["options"].is_object() {
                                    v["options"] = json!({});
                                }
                                v["options"]["num_ctx"] = json!(ctx);
                            }
                    }
                }
                Err(e) => return ollama_error(StatusCode::from_u16(e.status).unwrap_or(StatusCode::CONFLICT), &e.message),
            }
        }
        if let Some(ka) = v.get("keep_alive").cloned() {
            v["keep_alive"] = ollama::clamp_keep_alive(&ka);
        }
        if let Some(obj) = v.as_object_mut() {
            obj.retain(|k, _| !k.starts_with("lr_"));
        }
        Bytes::from(serde_json::to_vec(&v).unwrap_or_default())
    } else {
        body
    };
    touch(&st, &key);
    forward(&st, method, &path, body, resolved).await
}

async fn openai_passthrough(State(st): State<Shared>, caller: Caller, req: Request) -> R<Response> {
    let key = connected(&st, &caller)?;
    let path = req.uri().path().to_string();
    let bytes = axum::body::to_bytes(req.into_body(), 4 << 20).await.map_err(|_| ApiError::payload_too_large("request too large"))?;
    let mut v: Value = serde_json::from_slice(&bytes).map_err(|_| ApiError::invalid("invalid JSON"))?;
    let model = v.get("model").and_then(|m| m.as_str()).ok_or_else(|| ApiError::invalid("model is required").with_param("model"))?.to_string();
    let (id, _) = resolve_model(&st, &key, &model).await?;
    v["model"] = json!(id);
    if let Some(obj) = v.as_object_mut() {
        obj.retain(|k, _| !k.starts_with("lr_"));
    }
    touch(&st, &key);
    Ok(forward(&st, Method::POST, &path, Bytes::from(serde_json::to_vec(&v).unwrap()), Some(id)).await)
}

/// Send to Ollama and stream the answer back unchanged.
async fn forward(st: &AppState, method: Method, path: &str, body: Bytes, resolved: Option<String>) -> Response {
    let url = format!("{}{}", st.ollama.base, path);
    let mut req = st.ollama.http.request(reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap(), &url);
    if !body.is_empty() {
        req = req.header("Content-Type", "application/json").body(body);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = resp.headers().get("content-type").cloned();
            let mut out = Response::new(Body::from_stream(resp.bytes_stream()));
            *out.status_mut() = status;
            if let Some(ct) = ct.and_then(|c| HeaderValue::from_bytes(c.as_bytes()).ok()) {
                out.headers_mut().insert(header::CONTENT_TYPE, ct);
            }
            if let Some(id) = resolved.and_then(|m| HeaderValue::from_str(&m).ok()) {
                out.headers_mut().insert("LR-Resolved-Model", id);
            }
            out
        }
        Err(_) => E(ApiError::upstream_unavailable("Ollama isn't running or didn't answer.")).into_response(),
    }
}

// ─── Speech ─────────────────────────────────────────────────────────────────

struct Upload {
    bytes: Vec<u8>,
    ext: Option<String>,
    fields: HashMap<String, Vec<String>>,
}

async fn read_upload(mut mp: Multipart) -> Result<Upload, ApiError> {
    let mut bytes = None;
    let mut ext = None;
    let mut fields: HashMap<String, Vec<String>> = HashMap::new();
    while let Some(field) = mp.next_field().await.map_err(|e| ApiError::invalid(format!("bad upload: {e}")))? {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            ext = field.file_name().and_then(|f| f.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase());
            bytes = Some(field.bytes().await.map_err(|e| ApiError::payload_too_large(format!("upload failed: {e}")))?.to_vec());
        } else {
            let text = field.text().await.map_err(|e| ApiError::invalid(format!("bad field {name}: {e}")))?;
            fields.entry(name).or_default().push(text);
        }
    }
    let bytes = bytes.ok_or_else(|| ApiError::invalid("file is required").with_param("file"))?;
    Ok(Upload { bytes, ext, fields })
}

impl Upload {
    fn get(&self, k: &str) -> Option<&str> {
        self.fields.get(k).and_then(|v| v.first()).map(|s| s.as_str())
    }
    fn flag(&self, k: &str) -> bool {
        matches!(self.get(k), Some("true" | "1"))
    }
}

/// The speech model a request names (PROTOCOL.md 11.1).
async fn speech_model(st: &AppState, key: &str, requested: &str, role: &str) -> Result<String, ApiError> {
    let model = match requested {
        "whisper-1" | "gpt-4o-transcribe" | "gpt-4o-mini-transcribe" | "" => format!("localrouter/{role}"),
        m => m.to_string(),
    };
    let (id, _) = resolve_model(st, key, &model).await?;
    let known = installed_speech(st).await.iter().any(|(m, ..)| m == &id);
    if !known {
        return Err(if localrouter_speech::catalog::whisper_by_model_id(&id).is_some() {
            ApiError::model_not_downloaded(&id)
        } else {
            ApiError::not_found(format!("{id} isn't a speech model Local Router can run"))
        });
    }
    Ok(id)
}

async fn decode(upload: Upload) -> Result<localrouter_audio::Audio, ApiError> {
    let Upload { bytes, ext, .. } = upload;
    tokio::task::spawn_blocking(move || localrouter_audio::decode_bytes(bytes, ext.as_deref()))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::unsupported_media(format!("can't decode the audio: {e:#}")))
}

fn parse_segments(s: &str) -> Result<Vec<localrouter_speech::Turn>, ApiError> {
    #[derive(Deserialize)]
    struct Seg {
        start: f64,
        end: f64,
        #[serde(default)]
        speaker: Option<String>,
    }
    let segs: Vec<Seg> = serde_json::from_str(s).map_err(|e| ApiError::invalid(format!("lr_segments: {e}")).with_param("lr_segments"))?;
    Ok(segs
        .into_iter()
        .filter(|s| s.end > s.start)
        .map(|s| localrouter_speech::Turn { start: s.start, end: s.end, speaker: s.speaker.unwrap_or_else(|| "S1".into()) })
        .collect())
}

fn speech_response(v: Value, text: Option<String>, format: &str, model: &str) -> Response {
    let mut resp = match text {
        Some(t) => {
            let ct = if format == "vtt" { "text/vtt; charset=utf-8" } else { "text/plain; charset=utf-8" };
            ([(header::CONTENT_TYPE, ct)], t).into_response()
        }
        None => Json(v).into_response(),
    };
    if let Ok(h) = HeaderValue::from_str(model) {
        resp.headers_mut().insert("LR-Resolved-Model", h);
    }
    resp
}

async fn transcriptions(State(st): State<Shared>, caller: Caller, mp: Multipart) -> R<Response> {
    let key = connected(&st, &caller)?;
    let up = read_upload(mp).await?;
    let format = up.get("response_format").unwrap_or("json").to_string();
    if !["json", "text", "srt", "vtt", "verbose_json", "lr_speakers_json"].contains(&format.as_str()) {
        return Err(ApiError::invalid(format!("unknown response_format {format}")).with_param("response_format").into());
    }
    let stream = up.flag("stream");
    if stream && !["json", "verbose_json", "lr_speakers_json"].contains(&format.as_str()) {
        return Err(ApiError::invalid("stream=true works with json, verbose_json and lr_speakers_json").with_param("stream").into());
    }
    let priority = up.get("lr_priority").unwrap_or("interactive").to_string();
    if !["live", "interactive", "batch"].contains(&priority.as_str()) {
        return Err(ApiError::invalid("lr_priority is live, interactive or batch").with_param("lr_priority").into());
    }
    let model = speech_model(&st, &key, up.get("model").unwrap_or(""), "transcribe").await?;
    let granularities: Vec<&str> = up.fields.get("timestamp_granularities[]").map(|v| v.iter().map(|s| s.as_str()).collect()).unwrap_or_default();
    let segments = match up.get("lr_segments") {
        Some(s) => Some(parse_segments(s)?),
        None => None,
    };
    let diarize = segments.is_none() && (format == "lr_speakers_json" || up.flag("lr_diarize"));
    if diarize && !installed_speech(&st).await.iter().any(|(_, c, ..)| *c == Capability::Diarization) {
        return Err(ApiError::model_not_downloaded(&localrouter_speech::catalog::speaker_model_id()).into());
    }
    let req = TranscribeRequest {
        model_id: model.clone(),
        language: up.get("language").filter(|l| !l.is_empty() && *l != "auto").map(str::to_owned),
        prompt: up.get("prompt").map(str::to_owned),
        diarize,
        segments,
        offset: up.get("lr_offset").and_then(|o| o.parse().ok()).unwrap_or(0.0),
        words: granularities.contains(&"word"),
    };
    let audio = decode(up).await?;
    let audio_s = audio.duration_s();
    st.events.send("activity", json!({ "app_id": app_id_of(&st, &key), "model": model, "kind": "transcription", "phase": "started", "audio_seconds": audio_s }), Audience::Admin);
    touch(&st, &key);
    if stream {
        return Ok(transcription_stream(st, key, model, format, priority, audio, req));
    }
    let _job = speech_turn(&st, &priority, &model).await;
    let st2 = st.clone();
    let result = tokio::task::spawn_blocking(move || st2.speech.transcribe(&audio, &req).map(|t| (t, req)))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    st.events.send("activity", json!({ "app_id": app_id_of(&st, &key), "model": model, "kind": "transcription", "phase": "finished", "audio_seconds": audio_s }), Audience::Admin);
    let (t, req) = result;
    let (v, text) = speech::render(&t, &format, &req);
    Ok(speech_response(v, text, &format, &model))
}

/// Wait for the speech engine. Files take turns, whole; a live window that
/// uses the model already in memory goes between the windows of a running
/// file instead of waiting for all of it (PROTOCOL.md 11.4 `lr_priority`).
async fn speech_turn<'a>(st: &'a AppState, priority: &str, model: &str) -> Option<tokio::sync::MutexGuard<'a, ()>> {
    if priority == "live" && st.speech.asr_is(model) {
        return None;
    }
    Some(st.speech.job.lock().await)
}

/// `stream=true`: each finished segment as soon as it's ready, then the whole answer.
fn transcription_stream(st: Shared, key: String, model: String, format: String, priority: String, audio: localrouter_audio::Audio, req: TranscribeRequest) -> Response {
    let resolved = model.clone();
    let body = async_stream(move |tx| async move {
        let audio_s = audio.duration_s();
        let _job = speech_turn(&st, &priority, &model).await;
        let st2 = st.clone();
        let fmt = format.clone();
        let tx2 = tx.clone();
        let result = tokio::task::spawn_blocking(move || {
            let mut n = 0usize;
            let send = |kind: &str, data: Value| {
                let _ = tx2.blocking_send(SseEvent::default().event(kind).data(data.to_string()));
            };
            let mut sink = |u: localrouter_speech::Update<'_>| match u {
                localrouter_speech::Update::Turn(turn) => {
                    let seg = speech::segment_json(n, turn, &fmt, &req);
                    n += 1;
                    let delta = if n == 1 { turn.text.clone() } else { format!(" {}", turn.text) };
                    send("transcript.text.delta", json!({ "type": "transcript.text.delta", "delta": delta }));
                    send("transcript.text.segment", json!({ "type": "transcript.text.segment", "segment": seg }));
                }
                localrouter_speech::Update::Line(l) => {
                    send("lr.transcript.line", json!({ "type": "lr.transcript.line", "line": speech::line_json(l, &fmt, &req) }));
                }
                localrouter_speech::Update::Speakers(s) if fmt == "lr_speakers_json" => {
                    send("lr.transcript.speakers", json!({ "type": "lr.transcript.speakers", "speakers": s }));
                }
                localrouter_speech::Update::WaitingForSpeakers if fmt == "lr_speakers_json" => {
                    send("lr.transcript.phase", json!({ "type": "lr.transcript.phase", "phase": "speakers" }));
                }
                _ => {}
            };
            st2.speech.transcribe_streaming(&audio, &req, &mut sink).map(|t| (t, req))
        })
        .await;
        st.events.send("activity", json!({ "app_id": app_id_of(&st, &key), "model": model, "kind": "transcription", "phase": "finished", "audio_seconds": audio_s }), Audience::Admin);
        let event = match result {
            Ok(Ok((t, req))) => {
                let (mut v, _) = speech::render(&t, &format, &req);
                v["type"] = json!("transcript.text.done");
                SseEvent::default().event("transcript.text.done").data(v.to_string())
            }
            Ok(Err(e)) => error_event(ApiError::internal(format!("{e:#}"))),
            Err(e) => error_event(ApiError::internal(e.to_string())),
        };
        let _ = tx.send(event).await;
    });
    let mut resp = Sse::new(body).into_response();
    if let Ok(h) = HeaderValue::from_str(&resolved) {
        resp.headers_mut().insert("LR-Resolved-Model", h);
    }
    resp
}

fn error_event(e: ApiError) -> SseEvent {
    SseEvent::default().event("error").data(json!({ "type": "error", "error": e }).to_string())
}

async fn diarizations(State(st): State<Shared>, caller: Caller, mp: Multipart) -> R<Response> {
    let key = connected(&st, &caller)?;
    let up = read_upload(mp).await?;
    let format = up.get("response_format").unwrap_or("json").to_string();
    if format != "json" && format != "rttm" {
        return Err(ApiError::invalid(format!("unknown response_format {format}")).with_param("response_format").into());
    }
    let model = speech_model(&st, &key, up.get("model").unwrap_or(""), "diarize").await?;
    let audio = decode(up).await?;
    let duration = audio.duration_s();
    touch(&st, &key);
    let _job = st.speech.job.lock().await;
    let st2 = st.clone();
    let turns = tokio::task::spawn_blocking(move || st2.speech.diarize(&audio))
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    let (v, text) = speech::render_diarization(&turns, duration, &format, "upload");
    Ok(speech_response(v, text, &format, &model))
}

// ─── Admin (Local Router's own window) ──────────────────────────────────────

async fn admin_apps(State(st): State<Shared>, caller: Caller) -> R<Json<Value>> {
    admin(&caller)?;
    let inv = Inventory::get(&st).await;
    let keys: Vec<String> = st.store.read().unwrap().apps.keys().cloned().collect();
    let mut apps: Vec<Value> = keys.iter().filter_map(|k| record_for(&st, k, &inv)).map(|r| json!(r)).collect();
    let key_ids: Vec<String> = st.store.read().unwrap().keys.iter().map(|k| format!("key:{}", k.id)).collect();
    apps.extend(key_ids.iter().filter_map(|k| record_for(&st, k, &inv)).map(|r| json!(r)));
    Ok(Json(json!({ "apps": apps })))
}

/// Find an app's approval key by its app id (or `key:<id>`).
fn key_for_app(store: &Store, app: &str) -> Option<String> {
    if app.starts_with("key:") {
        return store.keys.iter().any(|k| format!("key:{}", k.id) == app).then(|| app.to_string());
    }
    store.apps.iter().find(|(_, a)| a.app_id == app).map(|(k, _)| k.clone())
}

async fn admin_remove_app(State(st): State<Shared>, caller: Caller, Path(app): Path<String>) -> R<StatusCode> {
    admin(&caller)?;
    let mut store = st.store.write().unwrap();
    let key = key_for_app(&store, &app).ok_or_else(|| ApiError::not_found("unknown app"))?;
    store.apps.remove(&key);
    let _ = store.save();
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Bind {
    model: Option<String>,
}

async fn admin_bind_app_role(State(st): State<Shared>, caller: Caller, Path((app, role)): Path<(String, String)>, Json(b): Json<Bind>) -> R<StatusCode> {
    admin(&caller)?;
    if !valid_role_name(&role) {
        return Err(ApiError::invalid("invalid role name").into());
    }
    let key = {
        let mut store = st.store.write().unwrap();
        let key = key_for_app(&store, &app).ok_or_else(|| ApiError::not_found("unknown app"))?;
        let bindings = if let Some(id) = key.strip_prefix("key:") {
            &mut store.keys.iter_mut().find(|k| k.id == id).unwrap().bindings
        } else {
            &mut store.apps.get_mut(&key).unwrap().bindings
        };
        match &b.model {
            Some(m) => bindings.insert(role.clone(), m.clone()),
            None => bindings.remove(&role),
        };
        let _ = store.save();
        key
    };
    let kind = if b.model.is_some() { "role.bound" } else { "role.unbound" };
    st.events.send(kind, json!({ "role": role, "model": b.model }), Audience::App(key));
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_bind_global(State(st): State<Shared>, caller: Caller, Path(role): Path<String>, Json(b): Json<Bind>) -> R<StatusCode> {
    admin(&caller)?;
    if !localrouter_core::WELL_KNOWN_ROLES.iter().any(|(r, _)| *r == role) {
        return Err(ApiError::invalid("global bindings exist for chat, embed, transcribe and diarize").into());
    }
    let affected: Vec<String> = {
        let mut store = st.store.write().unwrap();
        match &b.model {
            Some(m) => store.global.insert(role.clone(), m.clone()),
            None => store.global.remove(&role),
        };
        let _ = store.save();
        store.apps.keys().cloned().collect()
    };
    let kind = if b.model.is_some() { "role.bound" } else { "role.unbound" };
    for key in affected {
        st.events.send(kind, json!({ "role": role, "model": b.model }), Audience::App(key));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Answer {
    approve: bool,
}

async fn admin_answer(State(st): State<Shared>, caller: Caller, Path(rid): Path<String>, Json(a): Json<Answer>) -> R<StatusCode> {
    admin(&caller)?;
    let p = {
        let mut pending = st.pending.lock().unwrap();
        pending.expire();
        let p = pending.by_id.get_mut(&rid).ok_or_else(|| ApiError::not_found("unknown request"))?;
        if p.status != PendingStatus::Pending {
            return Err(ApiError::invalid("this request was already answered or expired").into());
        }
        p.status = if a.approve { PendingStatus::Approved } else { PendingStatus::Denied };
        p.clone()
    };
    if a.approve {
        let mut store = st.store.write().unwrap();
        store.apps.insert(p.identity_key.clone(), state::new_entry(&p.request, &p.trust));
        let _ = store.save();
    }
    st.events.send("connect.updated", json!({ "request_id": rid, "status": if a.approve { "approved" } else { "denied" } }), Audience::Admin);
    Ok(StatusCode::NO_CONTENT)
}

async fn admin_keys(State(st): State<Shared>, caller: Caller) -> R<Json<Value>> {
    admin(&caller)?;
    let store = st.store.read().unwrap();
    let keys: Vec<Value> = store.keys.iter().map(|k| json!({ "id": k.id, "name": k.name, "created": k.created, "last_used": k.last_used })).collect();
    Ok(Json(json!({ "keys": keys })))
}

#[derive(Deserialize)]
struct NewKey {
    name: String,
}

async fn admin_create_key(State(st): State<Shared>, caller: Caller, Json(n): Json<NewKey>) -> R<(StatusCode, Json<Value>)> {
    admin(&caller)?;
    let secret = state::new_key_secret();
    let entry = KeyEntry {
        id: format!("k_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]),
        name: clean_text(&n.name, 60),
        hash: state::key_hash(&secret),
        created: localrouter_core::now_rfc3339(),
        last_used: None,
        roles: vec![],
        bindings: Default::default(),
    };
    let body = json!({ "id": entry.id, "name": entry.name, "created": entry.created, "last_used": null, "secret": secret });
    let mut store = st.store.write().unwrap();
    store.keys.push(entry);
    let _ = store.save();
    Ok((StatusCode::CREATED, Json(body)))
}

async fn admin_revoke_key(State(st): State<Shared>, caller: Caller, Path(id): Path<String>) -> R<StatusCode> {
    admin(&caller)?;
    let mut store = st.store.write().unwrap();
    let before = store.keys.len();
    store.keys.retain(|k| k.id != id);
    if store.keys.len() == before {
        return Err(ApiError::not_found("unknown key").into());
    }
    let _ = store.save();
    Ok(StatusCode::NO_CONTENT)
}

/// Check the Hugging Face cache again now (the person just allowed the
/// drive); the `hf_cache.state` event tells the result.
async fn admin_check_hf_cache(State(st): State<Shared>, caller: Caller) -> R<StatusCode> {
    admin(&caller)?;
    st.hf_check.notify_one();
    Ok(StatusCode::ACCEPTED)
}

async fn admin_shutdown(State(st): State<Shared>, caller: Caller) -> R<StatusCode> {
    admin(&caller)?;
    st.shutdown.notify_waiters();
    Ok(StatusCode::ACCEPTED)
}

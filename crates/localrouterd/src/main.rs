//! `localrouterd`: Local Router's background service.
//!
//! Listens on a Unix socket (apps on this Mac, identified by code signature)
//! and on 127.0.0.1/::1 (tools with an `lra_` key), writes the discovery file
//! when ready, and removes it again on shutdown. See `spec/PROTOCOL.md`.
//!
//! Environment (development and tests): `LR_SUPPORT_DIR` (state folder),
//! `LR_PORT` (first TCP port to try), `LR_OLLAMA_URL`.

// ApiError is the protocol's error body (PROTOCOL.md 12); returning it by
// value keeps handlers simple, and requests aren't hot enough for its size to matter.
#![allow(clippy::result_large_err)]

mod app;
mod downloads;
mod events;
mod holds;
mod ollama;
mod speech;
mod state;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, Result};
use localrouter_core::{Endpoint, HfCacheInfo, OllamaInfo, PROTOCOL, TCP_PORT, TCP_PORT_LAST};
use localrouter_hf::{Cache, CacheLocation, CacheState};
use tokio::net::{TcpListener, UnixListener};

use crate::app::{AppState, Shared, SocketPeer, Transport};
use crate::events::Audience;

#[tokio::main]
async fn main() -> Result<()> {
    let support = localrouter_core::support_dir();
    std::fs::create_dir_all(&support).with_context(|| format!("creating {}", support.display()))?;
    std::fs::set_permissions(&support, std::fs::Permissions::from_mode(0o700))?;

    let socket_path = localrouter_core::socket_path();
    let _ = std::fs::remove_file(&socket_path); // stale socket from a crash
    let unix = UnixListener::bind(&socket_path).with_context(|| format!("binding {}", socket_path.display()))?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    let first: u16 = std::env::var("LR_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(TCP_PORT);
    let last = if first == TCP_PORT { TCP_PORT_LAST } else { first.saturating_add(9) };
    let (tcp4, port) = bind_tcp(first, last).await?;
    let tcp6 = TcpListener::bind(("::1", port)).await.ok();

    // Only the path now: checking the drive can wait on a permission prompt.
    let hub = CacheLocation::resolve_path();
    let ollama_url = std::env::var("LR_OLLAMA_URL").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let st: Shared = Arc::new(AppState {
        instance_id: uuid::Uuid::new_v4().to_string(),
        store: RwLock::new(state::Store::load()),
        pending: Mutex::new(state::PendingRequests::default()),
        events: events::Events::new(),
        ollama: ollama::Ollama::new(ollama_url.clone()),
        speech: Arc::new(speech::Speech::new()),
        hf: RwLock::new(HfCacheInfo { path: Some(hub.display().to_string()), state: "checking".into() }),
        ollama_state: RwLock::new(OllamaInfo { url: ollama_url, state: "stopped".into(), version: None }),
        resident: Mutex::new(Default::default()),
        downloads: downloads::Downloads::new(),
        holds: holds::Holds::default(),
        shutdown: tokio::sync::Notify::new(),
    });
    refresh_ollama(&st).await;

    let socket_app = app::router(st.clone(), Transport::Socket).into_make_service_with_connect_info::<SocketPeer>();
    let tcp_app = app::router(st.clone(), Transport::Tcp).into_make_service();
    tokio::spawn(async move { axum::serve(unix, socket_app).await });
    tokio::spawn({
        let app = tcp_app.clone();
        async move { axum::serve(tcp4, app).await }
    });
    if let Some(l6) = tcp6 {
        tokio::spawn(async move { axum::serve(l6, tcp_app).await });
    }

    let endpoint = Endpoint {
        protocols: vec![PROTOCOL],
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        instance_id: st.instance_id.clone(),
        pid: std::process::id(),
        started_at: localrouter_core::now_rfc3339(),
        http: format!("http://127.0.0.1:{port}"),
        socket: Some(socket_path.display().to_string()),
        hf_cache: st.hf.read().unwrap().clone(),
        ollama: st.ollama_state.read().unwrap().clone(),
        capabilities: app::CAPABILITIES.iter().map(|s| s.to_string()).collect(),
    };
    let endpoint_path = localrouter_core::endpoint_path();
    endpoint.write_atomic(&endpoint_path)?;
    eprintln!("localrouterd {} ready: {} and {}", env!("CARGO_PKG_VERSION"), socket_path.display(), endpoint.http);

    // Check the Hugging Face cache off the main path, then announce the result.
    tokio::spawn({
        let st = st.clone();
        let endpoint_path = endpoint_path.clone();
        async move {
            let hub2 = hub.clone();
            let state = tokio::task::spawn_blocking(move || CacheLocation::check(&hub2)).await.unwrap_or(CacheState::Unwritable);
            let name = match state {
                CacheState::Ok => "ok",
                CacheState::MissingVolume => "missing_volume",
                CacheState::Unwritable => "unwritable",
            };
            st.speech.set_cache((state == CacheState::Ok).then(|| Cache::new(&hub)));
            let info = HfCacheInfo { path: Some(hub.display().to_string()), state: name.into() };
            *st.hf.write().unwrap() = info.clone();
            if let Ok(mut ep) = Endpoint::read(&endpoint_path) {
                ep.hf_cache = info.clone();
                let _ = ep.write_atomic(&endpoint_path);
            }
            st.events.send("hf_cache.state", serde_json::to_value(&info).unwrap(), Audience::Everyone);
        }
    });

    tokio::spawn({
        let st = st.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                refresh_ollama(&st).await;
                for model in st.holds.expire() {
                    app::release(&st, &model).await;
                }
            }
        }
    });

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = term.recv() => {},
        _ = st.shutdown.notified() => {},
    }
    st.events.send("shutdown", serde_json::json!({ "restarting": false }), Audience::Everyone);
    tokio::time::sleep(Duration::from_millis(200)).await; // let the event reach open streams
    let _ = std::fs::remove_file(&endpoint_path);
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

async fn bind_tcp(first: u16, last: u16) -> Result<(TcpListener, u16)> {
    for port in first..=last {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((l, port));
        }
    }
    anyhow::bail!("ports {first}–{last} on 127.0.0.1 are all taken")
}

async fn refresh_ollama(st: &AppState) {
    let version = st.ollama.version().await;
    let state = if version.is_some() { "running" } else { "stopped" };
    let changed = {
        let mut cur = st.ollama_state.write().unwrap();
        let changed = cur.state != state || cur.version != version;
        cur.state = state.into();
        cur.version = version.clone();
        changed
    };
    if changed {
        st.events.send("ollama.state", serde_json::json!({ "state": state, "version": version }), Audience::Everyone);
    }
}

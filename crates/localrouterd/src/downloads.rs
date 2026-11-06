//! Downloads that apps ask for (PROTOCOL.md 8). One job per model: when two
//! apps ask for the same model they share it, and each sees it as its own.
//! Progress goes out as events to the apps that asked, at most 4 a second.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use localrouter_core::ApiError;
use localrouter_hf::{Cache, MirrorFile, Place};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::events::{Audience, Events};

/// Local Router's own window, when it starts a download.
pub const ADMIN: &str = "localrouter";

/// Finished downloads stay listed this long.
const KEEP_FINISHED: Duration = Duration::from_secs(60 * 60);

/// Downloads that run at the same time; the rest wait as `queued`.
const PARALLEL: usize = 2;

#[derive(Debug, Deserialize)]
pub struct DownloadRequest {
    pub model: ModelRef,
    pub source: String,
    /// For gated Hugging Face repositories: used for this download only.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub mirror: Option<Mirror>,
}

#[derive(Debug, Deserialize)]
pub struct ModelRef {
    pub store: String,
    pub id: String,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub revision: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Mirror {
    pub revision: String,
    pub files: Vec<MirrorFile>,
}

/// What a job does, decided when the request is checked.
#[derive(Debug)]
pub enum Plan {
    Ollama { name: String },
    HfFile { repo: String, file: String, revision: String, token: Option<String> },
    HfRepo { repo: String, revision: String, token: Option<String> },
    Mirror { repo: String, commit: String, files: Vec<MirrorFile> },
}

impl Plan {
    /// The model id (PROTOCOL.md 2) the download is for; jobs with the same
    /// one are shared.
    pub fn model_id(&self) -> String {
        match self {
            Plan::Ollama { name } => name.clone(),
            Plan::HfFile { repo, file, revision, .. } => with_revision(format!("hf:{repo}/{file}"), revision),
            Plan::HfRepo { repo, revision, .. } => with_revision(format!("hf:{repo}"), revision),
            Plan::Mirror { repo, commit, .. } => format!("hf:{repo}@{commit}"),
        }
    }

    pub fn source(&self) -> &'static str {
        match self {
            Plan::Ollama { .. } => "ollama",
            Plan::HfFile { .. } | Plan::HfRepo { .. } => "hf",
            Plan::Mirror { .. } => "mirror",
        }
    }
}

fn with_revision(id: String, revision: &str) -> String {
    if revision == "main" { id } else { format!("{id}@{revision}") }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Queued,
    Running,
    Verifying,
    Done,
    Failed,
    Cancelled,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Verifying => "verifying",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }

    pub fn finished(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
}

pub struct Progress {
    pub state: State,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub error: Option<ApiError>,
    finished_at: Option<Instant>,
    last_event: Option<Instant>,
}

pub struct Job {
    pub id: String,
    pub model: String,
    pub source: &'static str,
    pub progress: Mutex<Progress>,
    pub cancel: Arc<AtomicBool>,
    /// Approval keys of the apps that asked for it, or [`ADMIN`].
    pub starters: Mutex<BTreeSet<String>>,
}

impl Job {
    pub fn started_by(&self, key: &str) -> bool {
        self.starters.lock().unwrap().contains(key)
    }

    /// The download record (PROTOCOL.md 8) as `viewer` sees it.
    pub fn record(&self, viewer: Option<&str>) -> Value {
        let p = self.progress.lock().unwrap();
        let mut v = json!({
            "id": self.id, "model": self.model, "source": self.source, "state": p.state.as_str(),
            "bytes_done": p.bytes_done, "bytes_total": p.bytes_total,
            "started_by_you": viewer.is_some_and(|k| self.started_by(k)),
        });
        if let Some(e) = &p.error {
            v["error"] = serde_json::to_value(e).unwrap_or(Value::Null);
        }
        v
    }

    fn audience(&self) -> Audience {
        let keys: Vec<String> = self.starters.lock().unwrap().iter().filter(|k| *k != ADMIN).cloned().collect();
        if keys.is_empty() { Audience::Admin } else { Audience::Apps(keys) }
    }

    /// Progress from the download; an event at most every 250 ms.
    fn update(&self, events: &Events, done: u64, total: Option<u64>) {
        let send = {
            let mut p = self.progress.lock().unwrap();
            if p.state.finished() {
                return;
            }
            p.bytes_done = done;
            p.bytes_total = total.or(p.bytes_total);
            p.state = if total == Some(done) && done > 0 { State::Verifying } else { State::Running };
            let due = p.last_event.is_none_or(|t| t.elapsed() >= Duration::from_millis(250));
            if due {
                p.last_event = Some(Instant::now());
            }
            due
        };
        if send {
            let (done, total) = {
                let p = self.progress.lock().unwrap();
                (p.bytes_done, p.bytes_total)
            };
            events.send("download.progress", json!({ "id": self.id, "bytes_done": done, "bytes_total": total }), self.audience());
        }
    }

    /// Nothing to download: done at once.
    pub fn finish_now(&self, events: &Events) {
        self.finish(events, Ok(()));
    }

    fn finish(&self, events: &Events, result: Result<(), ApiError>) {
        {
            let mut p = self.progress.lock().unwrap();
            if p.state.finished() {
                return;
            }
            p.finished_at = Some(Instant::now());
            match result {
                Ok(()) => {
                    p.state = State::Done;
                    if let Some(t) = p.bytes_total {
                        p.bytes_done = t;
                    }
                }
                Err(_) if self.cancel.load(Ordering::Relaxed) => p.state = State::Cancelled,
                Err(e) => {
                    p.state = State::Failed;
                    p.error = Some(e);
                }
            }
        }
        let state = self.progress.lock().unwrap().state;
        let kind = if state == State::Done { "download.done" } else { "download.failed" };
        events.send(kind, json!({ "download": self.record(None) }), self.audience());
    }
}

pub struct Downloads {
    jobs: Mutex<Vec<Arc<Job>>>,
    slots: Arc<tokio::sync::Semaphore>,
}

impl Downloads {
    pub fn new() -> Self {
        Downloads { jobs: Mutex::new(Vec::new()), slots: Arc::new(tokio::sync::Semaphore::new(PARALLEL)) }
    }

    /// Jobs `key` started (all of them for [`ADMIN`]), newest first.
    pub fn list(&self, key: &str) -> Vec<Arc<Job>> {
        let mut jobs = self.jobs.lock().unwrap();
        jobs.retain(|j| j.progress.lock().unwrap().finished_at.is_none_or(|t| t.elapsed() < KEEP_FINISHED));
        jobs.iter().rev().filter(|j| key == ADMIN || j.started_by(key)).cloned().collect()
    }

    pub fn get(&self, key: &str, id: &str) -> Option<Arc<Job>> {
        self.list(key).into_iter().find(|j| j.id == id)
    }

    /// Join the running download of the same model, or start one. The bool
    /// says whether a new job was created.
    pub fn start(&self, key: &str, plan: &Plan) -> (Arc<Job>, bool) {
        let model = plan.model_id();
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(job) = jobs.iter().find(|j| j.model == model && !j.progress.lock().unwrap().state.finished()) {
            job.starters.lock().unwrap().insert(key.to_string());
            return (job.clone(), false);
        }
        let job = Arc::new(Job {
            id: format!("dl_{}", uuid::Uuid::new_v4().simple()),
            model,
            source: plan.source(),
            progress: Mutex::new(Progress {
                state: State::Queued,
                bytes_done: 0,
                bytes_total: None,
                error: None,
                finished_at: None,
                last_event: None,
            }),
            cancel: Arc::new(AtomicBool::new(false)),
            starters: Mutex::new(BTreeSet::from([key.to_string()])),
        });
        jobs.push(job.clone());
        (job, true)
    }

    /// An app gives up its interest; the download stops when nobody else
    /// wants it. Local Router's window stops it outright.
    pub fn cancel(&self, key: &str, job: &Job) {
        let nobody_left = {
            let mut s = job.starters.lock().unwrap();
            if key == ADMIN {
                s.clear();
            } else {
                s.remove(key);
            }
            s.is_empty()
        };
        if nobody_left {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn slots(&self) -> Arc<tokio::sync::Semaphore> {
        self.slots.clone()
    }
}

impl Default for Downloads {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a job to the end: wait for a slot, download, announce the result.
pub async fn run(job: Arc<Job>, plan: Plan, st: crate::app::Shared) {
    let Ok(_slot) = st.downloads.slots().acquire_owned().await else { return };
    if job.cancel.load(Ordering::Relaxed) {
        job.finish(&st.events, Err(ApiError::internal("cancelled")));
        return;
    }
    job.progress.lock().unwrap().state = State::Running;
    let result = match plan {
        Plan::Ollama { name } => {
            let (j, s) = (job.clone(), st.clone());
            let r = st.ollama.pull(&name, move |done, total| j.update(&s.events, done, total), &job.cancel).await;
            r.map_err(|e| ApiError::upstream_unavailable(e.to_string()))
        }
        plan => match st.speech.target() {
            None => Err(no_target()),
            Some(target) => {
                let (j, s) = (job.clone(), st.clone());
                let roots = st.speech.roots();
                tokio::task::spawn_blocking(move || {
                    let (place, cache) = where_to(target, &roots, &plan);
                    crate::places::prepare(&s.places, place)?;
                    let others: Vec<Cache> = roots.into_iter().map(|(_, c)| c).filter(|c| c.hub() != cache.hub()).collect();
                    hf_download(&cache, &others, plan, &j, &s.events)
                })
                .await
                .unwrap_or_else(|e| Err(ApiError::internal(e.to_string())))
            }
        },
    };
    job.finish(&st.events, result);
}

/// The folder for new models isn't usable (drive not connected, not allowed).
pub fn no_target() -> ApiError {
    ApiError::new(503, "server_error", "internal", "The folder for new models isn't available. Check Local Router's Storage settings.")
}

/// Where a download goes: the target, except that a mirror completes a
/// repository where it already is (its checks are about the files there).
fn where_to(target: (Place, Cache), roots: &[(Place, Cache)], plan: &Plan) -> (Place, Cache) {
    if let Plan::Mirror { repo, .. } = plan
        && let Some(found) = roots.iter().find(|(_, c)| c.repo_dir(repo).is_ok_and(|d| d.is_dir()))
    {
        return found.clone();
    }
    target
}

/// Download into `cache`; files already in one of the `others` folders are skipped.
fn hf_download(cache: &Cache, others: &[Cache], plan: Plan, job: &Job, events: &Events) -> Result<(), ApiError> {
    let mut progress = |done: u64, total: Option<u64>| job.update(events, done, total);
    match plan {
        Plan::HfFile { repo, file, revision, token } => {
            let cache = cache.clone().with_token(token);
            cache.download_cancellable(&repo, &file, &revision, &mut progress, &job.cancel).map(|_| ()).map_err(hf_error)
        }
        Plan::HfRepo { repo, revision, token } => {
            let cache = cache.clone().with_token(token);
            let (commit, files) = cache.list_files(&repo, &revision).map_err(hf_error)?;
            let elsewhere = |f: &String| others.iter().any(|c| c.snapshot_path(&repo, f, &commit).ok().flatten().is_some());
            let wanted: Vec<&String> = files.iter().filter(|f| !elsewhere(f)).collect();
            // Bytes aren't known up front for a whole repository: count files.
            let n = files.len() as u64;
            let skipped = n - wanted.len() as u64;
            for (i, file) in wanted.iter().enumerate() {
                let mut ignore = |_: u64, _: Option<u64>| {};
                // By branch, so refs/<branch> is written; pinned to the commit we listed.
                let rev = if i + 1 == wanted.len() { revision.as_str() } else { commit.as_str() };
                cache.download_cancellable(&repo, file, rev, &mut ignore, &job.cancel).map_err(hf_error)?;
                job.update(events, skipped + i as u64 + 1, Some(n));
            }
            Ok(())
        }
        Plan::Mirror { repo, commit, files } => {
            #[cfg(feature = "test-admin")]
            let cache = &cache.clone().with_mirror_guard(localrouter_hf::MirrorGuard::AllowLoopbackForTests);
            cache.install_mirror(&repo, &commit, &files, &mut progress, &job.cancel).map(|_| ()).map_err(hf_error)
        }
        Plan::Ollama { .. } => unreachable!("Ollama pulls run async"),
    }
}

pub fn hf_error(e: localrouter_hf::Error) -> ApiError {
    use localrouter_hf::Error as H;
    match e {
        H::NotFound(what) => ApiError::not_found(format!("{what} wasn't found")),
        H::Conflict(m) => ApiError::new(409, "invalid_request_error", "conflict", m),
        H::NotAllowed(m) => ApiError::forbidden(m),
        H::InvalidPath(m) => ApiError::invalid(m),
        H::Checksum(f) => ApiError::new(502, "upstream_error", "upstream_unavailable", format!("{f} arrived damaged (checksum mismatch)")),
        H::Http { status, url } => ApiError::upstream_unavailable(format!("{url} answered {status}")),
        H::Network(m) => ApiError::upstream_unavailable(m),
        H::Cancelled => ApiError::internal("cancelled"),
        other => ApiError::internal(other.to_string()),
    }
}

/// The Hugging Face organisations each signing team owns, for mirrors (PROTOCOL.md 8.1 rule 1).
pub fn mirror_orgs(team: Option<&str>) -> &'static [&'static str] {
    match team {
        Some(localrouter_core::ASKANTIS_TEAM) => &["askantis"],
        _ => &[],
    }
}

/// An Ollama model name as `ollama pull` takes it.
pub fn valid_ollama_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 300
        && !name.starts_with("localrouter/")
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._:/-".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan::HfFile { repo: "a/b".into(), file: "f.bin".into(), revision: "main".into(), token: None }
    }

    #[test]
    fn two_apps_share_one_download() {
        let d = Downloads::new();
        let (a, new_a) = d.start("app1", &plan());
        let (b, new_b) = d.start("app2", &plan());
        assert!(new_a && !new_b);
        assert_eq!(a.id, b.id);
        assert_eq!(a.model, "hf:a/b/f.bin");
        assert!(a.record(Some("app2"))["started_by_you"].as_bool().unwrap());
        assert!(!a.record(Some("app3"))["started_by_you"].as_bool().unwrap());
        assert_eq!(d.list("app3").len(), 0);
        assert_eq!(d.list(ADMIN).len(), 1);

        // One app cancelling doesn't stop it for the other.
        d.cancel("app1", &a);
        assert!(!a.cancel.load(Ordering::Relaxed));
        assert!(d.get("app1", &a.id).is_none());
        d.cancel("app2", &a);
        assert!(a.cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn progress_events_are_throttled() {
        let events = Events::new();
        let mut rx = events.subscribe();
        let d = Downloads::new();
        let (job, _) = d.start("app1", &plan());
        for i in 0..100 {
            job.update(&events, i, Some(1000));
        }
        job.finish(&events, Ok(()));
        let mut kinds = vec![];
        while let Ok(e) = rx.try_recv() {
            kinds.push(e.kind);
        }
        assert_eq!(kinds, vec!["download.progress", "download.done"]);
        assert_eq!(job.record(None)["bytes_done"], 1000);
        // A finished job doesn't change any more.
        job.update(&events, 5, Some(1000));
        assert_eq!(job.record(None)["state"], "done");
    }

    #[test]
    fn ollama_names() {
        for good in ["qwen3.5:9b", "hf.co/unsloth/Qwen3-4B-Instruct-2507-GGUF:Q4_K_M", "library/llama3"] {
            assert!(valid_ollama_name(good), "{good}");
        }
        for bad in ["", "localrouter/chat", "a b", "x;rm", "../x", "a\nb"] {
            assert!(!valid_ollama_name(bad), "{bad:?}");
        }
    }
}

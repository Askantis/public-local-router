//! The standard Hugging Face cache, shared with `huggingface_hub`, `transformers`,
//! llama.cpp and every other tool that follows it (PROTOCOL.md, section 2).
//!
//! - [`CacheLocation::resolve`] finds the cache the way `huggingface_hub` does,
//!   plus `launchctl getenv` on macOS, and notices a drive that isn't connected.
//! - [`Cache::snapshot_path`] finds a file that is already there.
//! - [`Cache::download`] fetches a file from huggingface.co into the standard
//!   layout (`blobs/`, `snapshots/<commit>/`, `refs/`), resumably and with the
//!   same lock files as `huggingface_hub`, so Python tools see it too.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// Where the cache is and whether it can be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheLocation {
    /// The `hub` folder, which holds `models--<org>--<repo>` folders.
    pub hub: PathBuf,
    pub state: CacheState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    Ok,
    /// The cache is on a drive (`/Volumes/<name>`) that isn't connected.
    MissingVolume,
    Unwritable,
}

impl CacheLocation {
    /// `HF_HUB_CACHE` → `HF_HOME/hub` → `XDG_CACHE_HOME/huggingface/hub` →
    /// `~/.cache/huggingface/hub`. Each variable is read from the process
    /// environment first, then (macOS) from `launchctl getenv`, because apps
    /// started from the Finder don't see shell profiles.
    pub fn resolve() -> Self {
        Self::resolve_with(&|name| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| launchctl_getenv(name))
        })
    }

    /// Only the path, without touching the disk. Checking the state can block:
    /// macOS asks the user before a background process reads a removable drive.
    pub fn resolve_path() -> PathBuf {
        Self::path_with(&|name| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| launchctl_getenv(name))
        })
    }

    /// Whether `hub` can be used (may block, see [`resolve_path`](Self::resolve_path)).
    pub fn check(hub: &Path) -> CacheState {
        cache_state(hub)
    }

    /// [`resolve`](Self::resolve) with an explicit variable lookup, for tests.
    pub fn resolve_with(lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        let hub = Self::path_with(lookup);
        let state = cache_state(&hub);
        CacheLocation { hub, state }
    }

    fn path_with(lookup: &dyn Fn(&str) -> Option<String>) -> PathBuf {
        let home = home_dir();
        if let Some(hub) = lookup("HF_HUB_CACHE") {
            expand(&hub, &home)
        } else if let Some(hf_home) = lookup("HF_HOME") {
            expand(&hf_home, &home).join("hub")
        } else if let Some(xdg) = lookup("XDG_CACHE_HOME") {
            expand(&xdg, &home).join("huggingface").join("hub")
        } else {
            home.join(".cache").join("huggingface").join("hub")
        }
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn expand(path: &str, home: &Path) -> PathBuf {
    let path = path.trim();
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

#[cfg(target_os = "macos")]
fn launchctl_getenv(name: &str) -> Option<String> {
    let out = std::process::Command::new("/bin/launchctl").args(["getenv", name]).output().ok()?;
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(not(target_os = "macos"))]
fn launchctl_getenv(_name: &str) -> Option<String> {
    None
}

/// The `/Volumes/<name>` mount point `path` is on, if that drive isn't
/// connected. Creating folders there would put them on the startup disk.
pub fn unmounted_volume(path: &Path) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let mut parts = path.components();
    let (Some(Component::RootDir), Some(Component::Normal(v)), Some(Component::Normal(name))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if v != "Volumes" {
        return None;
    }
    let mount = Path::new("/Volumes").join(name);
    (!is_mount_point(&mount)).then_some(mount)
}

fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(path), path.parent().map(fs::metadata)) {
        (Ok(m), Some(Ok(parent))) => m.dev() != parent.dev(),
        _ => false,
    }
}

fn cache_state(hub: &Path) -> CacheState {
    if unmounted_volume(hub).is_some() {
        return CacheState::MissingVolume;
    }
    // The nearest existing ancestor must be writable.
    let mut probe = hub.to_path_buf();
    while !probe.exists() {
        if !probe.pop() {
            return CacheState::Unwritable;
        }
    }
    let test = probe.join(format!(".localrouter-write-test-{}", std::process::id()));
    match File::create(&test) {
        Ok(_) => {
            let _ = fs::remove_file(&test);
            CacheState::Ok
        }
        Err(_) => CacheState::Unwritable,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the Hugging Face cache at {0} isn't available: {1:?}")]
    Unavailable(PathBuf, CacheState),
    #[error("invalid repository id or file path: {0}")]
    InvalidPath(String),
    #[error("{0} wasn't found on huggingface.co")]
    NotFound(String),
    #[error("huggingface.co answered {status} for {url}")]
    Http { status: u16, url: String },
    #[error("download of {0} failed the checksum")]
    Checksum(String),
    #[error("huggingface.co didn't send the headers needed to place {0} in the cache")]
    MissingHeaders(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("network error: {0}")]
    Network(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A Hugging Face cache rooted at a `hub` folder.
#[derive(Debug, Clone)]
pub struct Cache {
    hub: PathBuf,
    endpoint: String,
    token: Option<String>,
}

/// Progress callback: `(bytes_done, bytes_total)`.
pub type Progress<'a> = &'a mut dyn FnMut(u64, Option<u64>);

impl Cache {
    pub fn new(hub: impl Into<PathBuf>) -> Self {
        Cache { hub: hub.into(), endpoint: "https://huggingface.co".into(), token: None }
    }

    /// The cache at the standard place, or an error if it can't be used.
    pub fn standard() -> Result<Self> {
        let loc = CacheLocation::resolve();
        match loc.state {
            CacheState::Ok => Ok(Cache::new(loc.hub)),
            state => Err(Error::Unavailable(loc.hub, state)),
        }
    }

    /// Use another server than huggingface.co (tests, mirrors with the HF API).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into().trim_end_matches('/').to_string();
        self
    }

    /// A token for gated or private repositories. Used for requests, never stored.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token;
        self
    }

    pub fn hub(&self) -> &Path {
        &self.hub
    }

    /// `models--<org>--<repo>` for a repository id.
    pub fn repo_dir(&self, repo: &str) -> Result<PathBuf> {
        Ok(self.hub.join(format!("models--{}", checked_repo(repo)?.replace('/', "--"))))
    }

    /// The commit a revision points to: a commit hash as is, or what
    /// `refs/<branch>` says.
    pub fn resolve_revision(&self, repo: &str, revision: &str) -> Result<Option<String>> {
        if is_commit_hash(revision) {
            return Ok(Some(revision.to_string()));
        }
        let refs = self.repo_dir(repo)?.join("refs").join(checked_segment(revision)?);
        match fs::read_to_string(refs) {
            Ok(s) => Ok(Some(s.trim().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The snapshot folder of a revision, if it's in the cache.
    pub fn snapshot_dir(&self, repo: &str, revision: &str) -> Result<Option<PathBuf>> {
        let Some(commit) = self.resolve_revision(repo, revision)? else { return Ok(None) };
        let dir = self.repo_dir(repo)?.join("snapshots").join(commit);
        Ok(dir.is_dir().then_some(dir))
    }

    /// A file of a revision, if it's in the cache and its blob is complete.
    pub fn snapshot_path(&self, repo: &str, file: &str, revision: &str) -> Result<Option<PathBuf>> {
        let Some(dir) = self.snapshot_dir(repo, revision)? else { return Ok(None) };
        let path = dir.join(checked_file(file)?);
        // A symlink whose blob is missing doesn't count.
        Ok(fs::metadata(&path).is_ok().then_some(path))
    }

    /// Download `file` of `repo` at `revision` into the cache, unless it's
    /// already there, and return its snapshot path. Resumes a partial
    /// download and checks the SHA-256 of large (LFS) files.
    pub fn download(
        &self,
        repo: &str,
        file: &str,
        revision: &str,
        progress: Progress<'_>,
    ) -> Result<PathBuf> {
        if let Some(path) = self.snapshot_path(repo, file, revision)? {
            return Ok(path);
        }
        let file = checked_file(file)?;
        let repo_dir = self.repo_dir(repo)?;
        let url = format!("{}/{}/resolve/{}/{}", self.endpoint, checked_repo(repo)?, revision, file);

        let meta = self.head(&url)?;
        let commit = meta.commit.ok_or_else(|| Error::MissingHeaders(url.clone()))?;
        if !is_commit_hash(&commit) || !is_etag(&meta.etag) {
            return Err(Error::MissingHeaders(url));
        }
        let blob = repo_dir.join("blobs").join(&meta.etag);
        let pointer = repo_dir.join("snapshots").join(&commit).join(file);

        if !blob.is_file() {
            fs::create_dir_all(blob.parent().unwrap())?;
            let lock_path = self
                .hub
                .join(".locks")
                .join(repo_dir.file_name().unwrap())
                .join(format!("{}.lock", meta.etag));
            fs::create_dir_all(lock_path.parent().unwrap())?;
            let lock = OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path)?;
            lock.lock()?; // the same lock huggingface_hub's filelock takes
            if !blob.is_file() {
                let incomplete = blob.with_file_name(format!("{}.incomplete", meta.etag));
                self.fetch(&meta.location.unwrap_or(url.clone()), &incomplete, meta.size, progress)?;
                if meta.etag.len() == 64 {
                    let got = sha256_file(&incomplete)?;
                    if got != meta.etag {
                        let _ = fs::remove_file(&incomplete);
                        return Err(Error::Checksum(file.to_string()));
                    }
                }
                fs::rename(&incomplete, &blob)?;
            }
            drop(lock);
        }

        fs::create_dir_all(pointer.parent().unwrap())?;
        if fs::symlink_metadata(&pointer).is_err() {
            link_or_copy(&blob, &pointer, file)?;
        }
        if !is_commit_hash(revision) {
            let refs = repo_dir.join("refs").join(checked_segment(revision)?);
            fs::create_dir_all(refs.parent().unwrap())?;
            fs::write(refs, &commit)?;
        }
        Ok(pointer)
    }

    fn agent(&self, redirects: u32) -> ureq::Agent {
        ureq::Agent::config_builder()
            .max_redirects(redirects)
            .timeout_connect(Some(Duration::from_secs(20)))
            // Headers must come quickly; a large body may take as long as it takes
            // (ureq's body timeout covers the whole transfer, not a stall).
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn head(&self, url: &str) -> Result<HeadMeta> {
        let mut req = self.agent(0).head(url).header("User-Agent", user_agent());
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = req.call().map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 404 || status == 401 {
            return Err(Error::NotFound(url.to_string()));
        }
        if !(200..400).contains(&status) {
            return Err(Error::Http { status, url: url.to_string() });
        }
        let h = |name: &str| {
            resp.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string())
        };
        // Like huggingface_hub: X-Linked-Etag for LFS files, else ETag.
        let etag = h("x-linked-etag").or_else(|| h("etag")).unwrap_or_default();
        let etag = etag.trim_start_matches("W/").trim_matches('"').to_string();
        let size = h("x-linked-size").or_else(|| h("content-length")).and_then(|s| s.parse().ok());
        let location = if (300..400).contains(&status) { h("location") } else { None };
        let location = location.map(|l| {
            if l.starts_with('/') { format!("{}{}", self.endpoint, l) } else { l }
        });
        Ok(HeadMeta { commit: h("x-repo-commit"), etag, size, location })
    }

    fn fetch(&self, url: &str, dest: &Path, total: Option<u64>, progress: Progress<'_>) -> Result<()> {
        let mut done = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
        let mut req = self.agent(5).get(url).header("User-Agent", user_agent());
        // The token goes to huggingface.co only, never to the CDN it redirects to.
        if let (Some(t), true) = (&self.token, url.starts_with(&self.endpoint)) {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        if done > 0 {
            req = req.header("Range", format!("bytes={done}-"));
        }
        let resp = req.call().map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let mut out = match status {
            206 => OpenOptions::new().append(true).open(dest)?,
            200 => {
                done = 0;
                File::create(dest)?
            }
            _ => return Err(Error::Http { status, url: url.to_string() }),
        };
        let mut body = resp.into_body().into_reader();
        let mut buf = vec![0u8; 1 << 20];
        progress(done, total);
        loop {
            let n = body.read(&mut buf).map_err(|e| Error::Network(e.to_string()))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done += n as u64;
            if let Some(t) = total
                && done > t {
                    return Err(Error::Network(format!("{url} sent more than {t} bytes")));
                }
            progress(done, total);
        }
        out.sync_all()?;
        if let Some(t) = total
            && done != t {
                return Err(Error::Network(format!("{url} ended after {done} of {t} bytes")));
            }
        Ok(())
    }
}

struct HeadMeta {
    commit: Option<String>,
    etag: String,
    size: Option<u64>,
    location: Option<String>,
}

fn user_agent() -> String {
    format!("localrouter-hf/{}", env!("CARGO_PKG_VERSION"))
}

fn is_commit_hash(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_etag(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn checked_segment(s: &str) -> Result<&str> {
    let bad = s.is_empty()
        || s == "."
        || s == ".."
        || s.chars().any(|c| c == '/' || c == '\\' || c == ':' || c.is_control());
    if bad { Err(Error::InvalidPath(s.to_string())) } else { Ok(s) }
}

fn checked_repo(repo: &str) -> Result<&str> {
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(org), Some(name), None) => {
            checked_segment(org)?;
            checked_segment(name)?;
            Ok(repo)
        }
        _ => Err(Error::InvalidPath(repo.to_string())),
    }
}

/// A file path inside a repository: relative, with no empty, `.` or `..` parts.
fn checked_file(file: &str) -> Result<&str> {
    if file.is_empty() || file.starts_with('/') {
        return Err(Error::InvalidPath(file.to_string()));
    }
    for part in file.split('/') {
        checked_segment(part)?;
    }
    Ok(file)
}

/// `snapshots/<commit>/<file>` → `../../blobs/<etag>` (one more `..` per folder
/// in `file`), like huggingface_hub. Where symlinks can't be made, copy.
fn link_or_copy(blob: &Path, pointer: &Path, file: &str) -> io::Result<()> {
    let depth = file.matches('/').count() + 2;
    let mut target = PathBuf::new();
    for _ in 0..depth {
        target.push("..");
    }
    target.push("blobs");
    target.push(blob.file_name().unwrap());
    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(&target, pointer).is_ok() {
            return Ok(());
        }
    }
    fs::copy(blob, pointer).map(|_| ())
}

pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn resolution_order_matches_huggingface_hub() {
        let home = home_dir();
        let all = lookup(&[("HF_HUB_CACHE", "/a/hub-x"), ("HF_HOME", "/b"), ("XDG_CACHE_HOME", "/c")]);
        assert_eq!(CacheLocation::resolve_with(&all).hub, PathBuf::from("/a/hub-x"));
        let home_only = lookup(&[("HF_HOME", "/b"), ("XDG_CACHE_HOME", "/c")]);
        assert_eq!(CacheLocation::resolve_with(&home_only).hub, PathBuf::from("/b/hub"));
        let xdg = lookup(&[("XDG_CACHE_HOME", "/c")]);
        assert_eq!(CacheLocation::resolve_with(&xdg).hub, PathBuf::from("/c/huggingface/hub"));
        let none = lookup(&[]);
        assert_eq!(CacheLocation::resolve_with(&none).hub, home.join(".cache/huggingface/hub"));
        let tilde = lookup(&[("HF_HOME", "~/models")]);
        assert_eq!(CacheLocation::resolve_with(&tilde).hub, home.join("models/hub"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn notices_a_drive_that_isnt_connected() {
        let loc = CacheLocation::resolve_with(&lookup(&[("HF_HOME", "/Volumes/LRTestNotThere/hf")]));
        assert_eq!(loc.state, CacheState::MissingVolume);
        assert!(!Path::new("/Volumes/LRTestNotThere").exists());
        assert_eq!(unmounted_volume(Path::new("/Users/x/.cache")), None);
    }

    #[test]
    fn rejects_paths_that_escape_the_repository() {
        for bad in ["../x", "a/../b", "./a", "a//b", "/etc/passwd", "a/", "a\\b", "c:d", "", "x\ny"] {
            assert!(checked_file(bad).is_err(), "{bad:?} should be rejected");
        }
        for good in ["ggml-model-q8_0.bin", "sub/dir/file.bin", ".gitattributes", "a.b/c..d"] {
            assert!(checked_file(good).is_ok(), "{good:?} should be accepted");
        }
        assert!(checked_repo("org/name").is_ok());
        for bad in ["org", "org/name/x", "../name", "org/..", ""] {
            assert!(checked_repo(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn finds_files_in_a_python_made_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(tmp.path());
        let repo = cache.repo_dir("openai/whisper-large-v3-turbo").unwrap();
        let commit = "41f01f3fe87f28c78e2fbf8b568835947dd65ed9";
        fs::create_dir_all(repo.join("refs")).unwrap();
        fs::write(repo.join("refs/main"), commit).unwrap();
        fs::create_dir_all(repo.join("blobs")).unwrap();
        fs::write(repo.join("blobs/abc"), b"weights").unwrap();
        let snap = repo.join("snapshots").join(commit);
        fs::create_dir_all(&snap).unwrap();
        link_or_copy(&repo.join("blobs/abc"), &snap.join("model.safetensors"), "model.safetensors").unwrap();

        let found = cache.snapshot_path("openai/whisper-large-v3-turbo", "model.safetensors", "main").unwrap();
        assert_eq!(found, Some(snap.join("model.safetensors")));
        assert_eq!(fs::read(found.unwrap()).unwrap(), b"weights");
        assert_eq!(cache.snapshot_path("openai/whisper-large-v3-turbo", "missing.bin", "main").unwrap(), None);
        assert_eq!(cache.snapshot_path("other/repo", "x", "main").unwrap(), None);
        // A pointer whose blob is gone isn't a file in the cache.
        fs::remove_file(repo.join("blobs/abc")).unwrap();
        assert_eq!(cache.snapshot_path("openai/whisper-large-v3-turbo", "model.safetensors", "main").unwrap(), None);
    }

    #[test]
    fn nested_files_link_back_to_the_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let blobs = tmp.path().join("blobs");
        fs::create_dir_all(&blobs).unwrap();
        fs::write(blobs.join("e1"), b"x").unwrap();
        let pointer = tmp.path().join("snapshots/c/sub/dir/f.bin");
        fs::create_dir_all(pointer.parent().unwrap()).unwrap();
        link_or_copy(&blobs.join("e1"), &pointer, "sub/dir/f.bin").unwrap();
        assert_eq!(fs::read(&pointer).unwrap(), b"x");
        #[cfg(unix)]
        assert_eq!(fs::read_link(&pointer).unwrap(), PathBuf::from("../../../../blobs/e1"));
    }

    /// Real download from huggingface.co. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn downloads_like_huggingface_hub() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::new(tmp.path());
        let mut calls = 0;
        let path = cache
            .download("ggml-org/whisper-vad", "ggml-silero-v6.2.0.bin", "main", &mut |_, _| calls += 1)
            .unwrap();
        assert!(path.starts_with(tmp.path().join("models--ggml-org--whisper-vad/snapshots")));
        assert!(fs::metadata(&path).unwrap().len() > 100_000);
        assert!(calls > 0);
        let again = cache.download("ggml-org/whisper-vad", "ggml-silero-v6.2.0.bin", "main", &mut |_, _| {}).unwrap();
        assert_eq!(path, again);
        assert!(tmp.path().join("models--ggml-org--whisper-vad/refs/main").is_file());
    }
}

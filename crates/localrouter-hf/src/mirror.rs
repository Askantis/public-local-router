//! Files from an app maker's own server, placed in the Hugging Face cache
//! exactly where `huggingface_hub` would put them (PROTOCOL.md 8.1 and 8.2).
//!
//! A mirror writes into a cache that other programs trust, so every rule is
//! enforced here, not by the caller: public HTTPS addresses only (checked on
//! every connection, so redirects and DNS answers can't point inside the
//! network), checked paths, no symbolic links followed, a size cap, the
//! SHA-256 of every file, and nothing already in the cache is replaced.

use std::fmt;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use crate::{
    checked_file, checked_repo, fetch, is_commit_hash, is_etag, link_or_copy, sha256_file, user_agent, Cache, Error,
    Progress, Result,
};

/// One file of a mirror download, as the app supplies it.
#[derive(Clone, Serialize, Deserialize)]
pub struct MirrorFile {
    pub path: String,
    /// Used for this download only, never written to disk or logs.
    pub url: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub etag: String,
}

impl fmt::Debug for MirrorFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MirrorFile")
            .field("path", &self.path)
            .field("size_bytes", &self.size_bytes)
            .field("sha256", &self.sha256)
            .finish_non_exhaustive()
    }
}

/// Which addresses a mirror may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorGuard {
    /// `https` to public addresses only. The only setting outside tests.
    PublicHttpsOnly,
    /// Also `http` to this computer, for tests against a local server.
    AllowLoopbackForTests,
}

/// Files of 10 MB or more are named by their SHA-256 (rule 3).
const LARGE: u64 = 10 * 1024 * 1024;

impl Cache {
    /// Place `files` of `repo` at `commit` into the cache, downloading what's
    /// missing. Returns the snapshot folder. `refs/main` is written only for a
    /// repository the cache doesn't know yet; otherwise apps name the commit.
    pub fn install_mirror(
        &self,
        repo: &str,
        commit: &str,
        files: &[MirrorFile],
        progress: Progress<'_>,
        cancel: &AtomicBool,
    ) -> Result<PathBuf> {
        checked_repo(repo)?;
        if !is_commit_hash(commit) {
            return Err(Error::InvalidPath(format!("revision {commit} isn't a commit hash")));
        }
        if files.is_empty() {
            return Err(Error::InvalidPath("a mirror needs at least one file".into()));
        }
        for f in files {
            self.check_file(f)?;
        }

        let repo_dir = self.repo_dir(repo)?;
        let new_repo = fs::symlink_metadata(&repo_dir).is_err();
        let snapshot = repo_dir.join("snapshots").join(commit);

        // Rule 2: never over someone else's files.
        let mut missing = Vec::new();
        for f in files {
            let pointer = snapshot.join(&f.path);
            let blob = repo_dir.join("blobs").join(&f.etag);
            if fs::symlink_metadata(&pointer).is_ok() {
                if !content_matches(&pointer, f)? {
                    return Err(Error::Conflict(format!(
                        "{repo}/{} is already in the cache with different contents",
                        f.path
                    )));
                }
            } else if fs::symlink_metadata(&blob).is_ok() && !content_matches(&blob, f)? {
                return Err(Error::Conflict(format!("the cache already has a different file named {}", f.etag)));
            } else {
                missing.push(f);
            }
        }

        // The cache folder itself may be a link the user made (to another
        // drive, say); only what's inside it is checked.
        fs::create_dir_all(&self.hub)?;
        let total: u64 = missing.iter().map(|f| f.size_bytes).sum();
        let mut before = 0u64;
        progress(0, Some(total));
        let agent = self.mirror_agent();
        for f in &missing {
            real_dir(&self.hub, &repo_dir.join("blobs"))?;
            let blob = repo_dir.join("blobs").join(&f.etag);
            if fs::symlink_metadata(&blob).is_err() {
                let lock_path = self.hub.join(".locks").join(repo_dir.file_name().unwrap()).join(format!("{}.lock", f.etag));
                real_dir(&self.hub, lock_path.parent().unwrap())?;
                let lock = crate::open_no_follow_keep(&lock_path)?;
                lock.lock()?; // the same lock huggingface_hub takes
                if fs::symlink_metadata(&blob).is_err() {
                    let incomplete = blob.with_file_name(format!("{}.incomplete", f.etag));
                    let mut each = |done: u64, _: Option<u64>| progress(before + done, Some(total));
                    fetch(&agent, &f.url, None, &incomplete, Some(f.size_bytes), &mut each, cancel)?;
                    // Rule 5: checked before it becomes visible.
                    if sha256_file(&incomplete)? != f.sha256 {
                        let _ = fs::remove_file(&incomplete);
                        return Err(Error::Checksum(f.path.clone()));
                    }
                    fs::rename(&incomplete, &blob)?;
                }
                drop(lock);
            }
            let pointer = snapshot.join(&f.path);
            real_dir(&self.hub, pointer.parent().unwrap())?;
            if fs::symlink_metadata(&pointer).is_err() {
                link_or_copy(&blob, &pointer, &f.path)?;
            }
            before += f.size_bytes;
            progress(before, Some(total));
        }

        // Files that were already there still need their pointers.
        for f in files {
            let pointer = snapshot.join(&f.path);
            if fs::symlink_metadata(&pointer).is_err() {
                real_dir(&self.hub, pointer.parent().unwrap())?;
                link_or_copy(&repo_dir.join("blobs").join(&f.etag), &pointer, &f.path)?;
            }
        }
        if new_repo {
            real_dir(&self.hub, &repo_dir.join("refs"))?;
            fs::write(repo_dir.join("refs").join("main"), commit)?;
        }
        Ok(snapshot)
    }

    /// Rules 3 and 4 for every file, without touching the cache or the network.
    pub fn check_mirror_files(&self, files: &[MirrorFile]) -> Result<()> {
        files.iter().try_for_each(|f| self.check_file(f))
    }

    /// Rules 3 and 4 for one file, before anything is fetched.
    fn check_file(&self, f: &MirrorFile) -> Result<()> {
        checked_file(&f.path)?;
        let hex64 = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if !hex64(&f.sha256) {
            return Err(Error::InvalidPath(format!("sha256 of {} isn't 64 lowercase hex characters", f.path)));
        }
        if !is_etag(&f.etag) {
            return Err(Error::InvalidPath(format!("etag of {} isn't 40 or 64 hex characters", f.path)));
        }
        if f.size_bytes >= LARGE && f.etag != f.sha256 {
            return Err(Error::InvalidPath(format!("{} is large, so its etag must be its sha256", f.path)));
        }
        let uri: Uri = f.url.parse().map_err(|_| Error::NotAllowed(format!("the address of {} isn't valid", f.path)))?;
        let host = uri.host().unwrap_or_default();
        match (uri.scheme_str(), self.guard) {
            (Some("https"), _) => {}
            (Some("http"), MirrorGuard::AllowLoopbackForTests) if is_loopback_host(host) => {}
            _ => return Err(Error::NotAllowed(format!("{} must be downloaded over https", f.path))),
        }
        Ok(())
    }

    fn mirror_agent(&self) -> ureq::Agent {
        let config = ureq::Agent::config_builder()
            .max_redirects(5)
            // A proxy would be resolved instead of the mirror, hiding where it goes.
            .proxy(None)
            .timeout_connect(Some(Duration::from_secs(20)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            .user_agent(user_agent())
            .build();
        ureq::Agent::with_parts(config, DefaultConnector::default(), PublicOnly { guard: self.guard })
    }
}

/// Whether a file in the cache has the declared contents. A blob named by its
/// SHA-256 with the right size was checked when it was written (by Local
/// Router or by `huggingface_hub`); anything else is hashed.
fn content_matches(path: &Path, f: &MirrorFile) -> Result<bool> {
    let Ok(meta) = fs::metadata(path) else { return Ok(false) };
    if meta.len() != f.size_bytes {
        return Ok(false);
    }
    let named_by_hash = fs::canonicalize(path)
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .is_some_and(|n| n == f.sha256);
    Ok(named_by_hash || sha256_file(path)? == f.sha256)
}

/// Create `dir` below `root` folder by folder, refusing symbolic links, so a
/// link planted in the cache can't send files elsewhere (rule 3).
fn real_dir(root: &Path, dir: &Path) -> Result<()> {
    let rel = dir.strip_prefix(root).map_err(|_| Error::InvalidPath(dir.display().to_string()))?;
    let mut at = root.to_path_buf();
    for part in rel.components() {
        at.push(part);
        match fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(Error::NotAllowed(format!("{} is a symbolic link", at.display())));
            }
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(Error::InvalidPath(at.display().to_string())),
            Err(_) => fs::create_dir(&at).or_else(|e| if at.is_dir() { Ok(()) } else { Err(e) })?,
        }
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// Resolves only to public addresses, for every connection a download makes,
/// redirects included (rule 4).
#[derive(Debug)]
struct PublicOnly {
    guard: MirrorGuard,
}

impl Resolver for PublicOnly {
    fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> std::result::Result<ResolvedSocketAddrs, ureq::Error> {
        let host = uri.host().unwrap_or_default().to_ascii_lowercase();
        let test_local = self.guard == MirrorGuard::AllowLoopbackForTests && is_loopback_host(&host);
        if uri.scheme_str() != Some("https") && !test_local {
            return Err(ureq::Error::BadUri(format!("{host}: mirrors are https only")));
        }
        if host.trim_end_matches('.').ends_with(".local") {
            return Err(ureq::Error::BadUri(format!("{host}: local network names aren't allowed")));
        }
        let mut out = self.empty();
        out.truncate(0);
        for addr in DefaultResolver::default().resolve(uri, config, timeout)?.iter() {
            if is_public(addr.ip()) || (test_local && addr.ip().is_loopback()) {
                out.push(*addr);
            }
        }
        if out.is_empty() {
            return Err(ureq::Error::BadUri(format!("{host} isn't a public address")));
        }
        Ok(out)
    }
}

/// Not this computer, the local network, link-local, CGNAT, multicast or reserved.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local, RFC 4193
                || (s[0] & 0xffc0) == 0xfe80 // link-local
                || (s[0] & 0xffc0) == 0xfec0 // site-local (deprecated)
                || v6 == Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0))
        }
    }
}

fn is_public_v4(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    !(v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // CGNAT, RFC 6598
        || o[0] >= 240)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write as _};
    use std::net::TcpListener;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    #[test]
    fn only_public_addresses() {
        for bad in ["127.0.0.1", "10.1.2.3", "172.16.0.9", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0",
            "::1", "fd00::1", "fe80::1", "::ffff:192.168.1.1", "224.0.0.1", "255.255.255.255"]
        {
            assert!(!is_public(bad.parse().unwrap()), "{bad} should be refused");
        }
        for good in ["20.60.1.1", "151.101.1.1", "2a00:1450::1"] {
            assert!(is_public(good.parse().unwrap()), "{good} should be allowed");
        }
    }

    fn file(path: &str, url: &str, body: &[u8]) -> MirrorFile {
        let sha = hex::encode(sha2::Sha256::digest(body));
        MirrorFile { path: path.into(), url: url.into(), size_bytes: body.len() as u64, sha256: sha.clone(), etag: sha }
    }

    use sha2::Digest as _;

    /// A tiny HTTP server that answers every GET with `body`; counts requests.
    fn serve(body: &'static [u8]) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let h = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                h.fetch_add(1, Ordering::SeqCst);
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let mut s = stream;
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(body);
            }
        });
        (url, hits)
    }

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn installs_into_the_standard_layout_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, hits) = serve(b"weights!");
        let cache = Cache::new(tmp.path()).with_mirror_guard(MirrorGuard::AllowLoopbackForTests);
        let files = [file("ggml-model-q8_0.bin", &format!("{url}/blob?sig=secret"), b"weights!")];
        let never = AtomicBool::new(false);
        let snap = cache.install_mirror("askantis/gruezi-v1", COMMIT, &files, &mut |_, _| {}, &never).unwrap();
        assert_eq!(fs::read(snap.join("ggml-model-q8_0.bin")).unwrap(), b"weights!");
        assert_eq!(cache.snapshot_path("askantis/gruezi-v1", "ggml-model-q8_0.bin", "main").unwrap(), Some(snap.join("ggml-model-q8_0.bin")));
        // Present now: no second download.
        cache.install_mirror("askantis/gruezi-v1", COMMIT, &files, &mut |_, _| {}, &never).unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        // Nothing of the URL is kept anywhere in the cache.
        let mut stack = vec![tmp.path().to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(d).unwrap().flatten() {
                let p = e.path();
                assert!(!p.to_string_lossy().contains("secret"));
                if p.is_dir() && !fs::symlink_metadata(&p).unwrap().file_type().is_symlink() {
                    stack.push(p);
                } else if p.is_file() {
                    assert!(!String::from_utf8_lossy(&fs::read(&p).unwrap()).contains("secret"));
                }
            }
        }
    }

    #[test]
    fn refuses_a_wrong_checksum_and_keeps_nothing_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, _) = serve(b"tampered");
        let cache = Cache::new(tmp.path()).with_mirror_guard(MirrorGuard::AllowLoopbackForTests);
        let mut f = file("m.bin", &format!("{url}/m"), b"original");
        f.size_bytes = 8;
        let err = cache.install_mirror("askantis/x", COMMIT, &[f], &mut |_, _| {}, &AtomicBool::new(false)).unwrap_err();
        assert!(matches!(err, Error::Checksum(_)), "{err}");
        assert_eq!(cache.snapshot_path("askantis/x", "m.bin", COMMIT).unwrap(), None);
    }

    #[test]
    fn never_replaces_files_already_in_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, _) = serve(b"new");
        let cache = Cache::new(tmp.path()).with_mirror_guard(MirrorGuard::AllowLoopbackForTests);
        let repo = cache.repo_dir("askantis/x").unwrap();
        let snap = repo.join("snapshots").join(COMMIT);
        fs::create_dir_all(&snap).unwrap();
        fs::write(snap.join("m.bin"), b"old").unwrap();
        let err = cache
            .install_mirror("askantis/x", COMMIT, &[file("m.bin", &format!("{url}/m"), b"new")], &mut |_, _| {}, &AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err}");
        assert_eq!(fs::read(snap.join("m.bin")).unwrap(), b"old");
        // An existing repository keeps its refs/main.
        fs::create_dir_all(repo.join("refs")).unwrap();
        fs::write(repo.join("refs/main"), "f".repeat(40)).unwrap();
        cache
            .install_mirror("askantis/x", COMMIT, &[file("other.bin", &format!("{url}/o"), b"new")], &mut |_, _| {}, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(fs::read_to_string(repo.join("refs/main")).unwrap(), "f".repeat(40));
    }

    #[test]
    fn checks_addresses_paths_and_names_before_fetching() {
        let tmp = tempfile::tempdir().unwrap();
        let strict = Cache::new(tmp.path());
        let never = AtomicBool::new(false);
        let try_one = |cache: &Cache, f: MirrorFile| cache.install_mirror("askantis/x", COMMIT, &[f], &mut |_, _| {}, &never);
        for url in ["http://example.com/m", "https://127.0.0.1/m", "https://10.0.0.1/m", "https://printer.local/m", "file:///etc/passwd", "ftp://example.com/m"] {
            let err = try_one(&strict, file("m.bin", url, b"x")).unwrap_err();
            assert!(matches!(err, Error::NotAllowed(_) | Error::Network(_)), "{url}: {err}");
        }
        for path in ["../m.bin", "a/../../m", "/abs", "a\\b", "c:d"] {
            assert!(matches!(try_one(&strict, file(path, "https://example.com/m", b"x")).unwrap_err(), Error::InvalidPath(_)), "{path}");
        }
        let mut big = file("big.bin", "https://example.com/m", b"x");
        big.size_bytes = LARGE;
        big.etag = "a".repeat(40);
        assert!(matches!(try_one(&strict, big).unwrap_err(), Error::InvalidPath(_)));
        assert!(matches!(
            strict.install_mirror("askantis/x", "main", &[file("m", "https://example.com/m", b"x")], &mut |_, _| {}, &never),
            Err(Error::InvalidPath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_planted_links() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (url, _) = serve(b"weights!");
        let cache = Cache::new(tmp.path()).with_mirror_guard(MirrorGuard::AllowLoopbackForTests);
        let repo = cache.repo_dir("askantis/x").unwrap();
        fs::create_dir_all(&repo).unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.join("blobs")).unwrap();
        let err = cache
            .install_mirror("askantis/x", COMMIT, &[file("m.bin", &format!("{url}/m"), b"weights!")], &mut |_, _| {}, &AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(err, Error::NotAllowed(_)), "{err}");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn stops_when_cancelled() {
        let tmp = tempfile::tempdir().unwrap();
        let (url, _) = serve(b"weights!");
        let cache = Cache::new(tmp.path()).with_mirror_guard(MirrorGuard::AllowLoopbackForTests);
        let cancel = AtomicBool::new(true);
        let err = cache
            .install_mirror("askantis/x", COMMIT, &[file("m.bin", &format!("{url}/m"), b"weights!")], &mut |_, _| {}, &cancel)
            .unwrap_err();
        assert!(matches!(err, Error::Cancelled), "{err}");
    }
}

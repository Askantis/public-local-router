//! Where models live (PROTOCOL.md section 2): the **Askantis models folder**
//! (`~/Library/Application Support/Askantis/Models`), shared by every Askantis
//! app, and the **Hugging Face cache** of people who already use Hugging Face
//! tools. Both use the same layout (`models--<org>--<repo>/…`), so a [`Cache`]
//! works on either; models are found in both, and new downloads go to one
//! target, chosen by [`ModelPlaces::default_place`] unless the person picked.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{unmounted_volume, Cache, CacheLocation};

/// Where new models go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// The Askantis models folder, shared by every Askantis app.
    Askantis,
    /// The Hugging Face cache, shared with Hugging Face tools.
    Huggingface,
    /// A folder the person chose.
    Custom,
}

impl Place {
    pub fn as_str(&self) -> &'static str {
        match self {
            Place::Askantis => "askantis",
            Place::Huggingface => "huggingface",
            Place::Custom => "custom",
        }
    }

    pub fn parse(s: &str) -> Option<Place> {
        match s {
            "askantis" => Some(Place::Askantis),
            "huggingface" => Some(Place::Huggingface),
            "custom" => Some(Place::Custom),
            _ => None,
        }
    }
}

/// How the person set the app up: "I just want it to work" or "I want to tinker".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    Simple,
    Tinker,
}

/// The two standard places on this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPlaces {
    /// The Askantis models folder (hub layout).
    pub askantis: PathBuf,
    /// The Hugging Face cache's `hub` folder.
    pub huggingface: PathBuf,
    /// A Hugging Face variable (`HF_HUB_CACHE`, `HF_HOME`, `XDG_CACHE_HOME`) is set.
    hf_configured: bool,
}

const HF_VARS: [&str; 3] = ["HF_HUB_CACHE", "HF_HOME", "XDG_CACHE_HOME"];

impl ModelPlaces {
    /// Reads the environment (and `launchctl getenv` on macOS) like
    /// [`CacheLocation::resolve_path`]; touches no disk.
    pub fn resolve() -> Self {
        Self::resolve_with(&crate::env_lookup, &crate::home_dir())
    }

    /// [`resolve`](Self::resolve) with an explicit variable lookup and home folder, for tests.
    pub fn resolve_with(lookup: &dyn Fn(&str) -> Option<String>, home: &Path) -> Self {
        let askantis = match lookup("ASKANTIS_MODELS") {
            Some(dir) => crate::expand(&dir, home),
            None => home.join("Library").join("Application Support").join("Askantis").join("Models"),
        };
        ModelPlaces {
            askantis,
            huggingface: CacheLocation::path_with_home(lookup, home),
            hf_configured: HF_VARS.iter().any(|v| lookup(v).is_some()),
        }
    }

    /// Whether this computer already uses Hugging Face: a Hugging Face
    /// variable is set, or the cache holds at least one model. With a variable
    /// set nothing is read (the cache may be on a drive macOS asks about).
    pub fn has_huggingface(&self) -> bool {
        if self.hf_configured {
            return true;
        }
        match fs::read_dir(&self.huggingface) {
            Ok(entries) => entries.flatten().any(|e| e.file_name().to_string_lossy().starts_with("models--")),
            Err(_) => false,
        }
    }

    /// Where new models go unless the person chose: the Askantis folder for
    /// "I just want it to work"; for tinkerers the Hugging Face cache if they
    /// have one, else the Askantis folder.
    pub fn default_place(&self, setup: Setup) -> Place {
        match setup {
            Setup::Tinker if self.has_huggingface() => Place::Huggingface,
            _ => Place::Askantis,
        }
    }

    /// The folder of a place (`custom` is the chosen folder; empty falls back to Askantis).
    pub fn path(&self, place: Place, custom: &str) -> PathBuf {
        match place {
            Place::Askantis => self.askantis.clone(),
            Place::Huggingface => self.huggingface.clone(),
            Place::Custom if !custom.trim().is_empty() => PathBuf::from(custom.trim()),
            Place::Custom => self.askantis.clone(),
        }
    }

    /// The folders to look in, `target` first, without duplicates.
    pub fn roots(&self, target: &Path) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for p in [target, &self.askantis, &self.huggingface] {
            if !roots.iter().any(|r| r == p) {
                roots.push(p.to_path_buf());
            }
        }
        roots
    }

    /// A file of a model, wherever it is: `target` first, then the other
    /// places. Folders on a drive that isn't connected are skipped.
    pub fn find(&self, target: &Path, repo: &str, file: &str, revision: &str) -> Option<PathBuf> {
        self.roots(target)
            .into_iter()
            .filter(|root| unmounted_volume(root).is_none())
            .find_map(|root| Cache::new(root).snapshot_path(repo, file, revision).ok().flatten())
    }

    /// The folder a repository is in, wherever it is (`target` first).
    pub fn find_repo(&self, target: &Path, repo: &str) -> Option<PathBuf> {
        self.roots(target)
            .into_iter()
            .filter(|root| unmounted_volume(root).is_none())
            .filter_map(|root| Cache::new(root).repo_dir(repo).ok())
            .find(|dir| dir.is_dir())
    }

    /// Creates the Askantis folder if needed and keeps it out of Time Machine
    /// backups (everything in it can be downloaded again).
    pub fn prepare_askantis(&self) -> io::Result<()> {
        let fresh = !self.askantis.exists();
        fs::create_dir_all(&self.askantis)?;
        if fresh {
            exclude_from_backup(&self.askantis);
        }
        Ok(())
    }

    /// Records which models an app uses (`hf:<org>/<repo>` or the repository
    /// id), so removing them from one app never removes another app's.
    pub fn record_usage(&self, app_id: &str, models: &[String]) -> io::Result<()> {
        let dir = self.askantis.join(".apps");
        fs::create_dir_all(&dir)?;
        let mut models: Vec<String> = models.iter().map(|m| normalize(m)).collect();
        models.sort();
        models.dedup();
        let body = serde_json::to_vec_pretty(&Usage { app_id: app_id.to_string(), models })?;
        let path = dir.join(format!("{}.json", safe_name(app_id)));
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, body)?;
        fs::rename(tmp, path)
    }

    /// Whether another app said it uses `repo`.
    pub fn used_by_others(&self, app_id: &str, repo: &str) -> bool {
        let repo = normalize(repo);
        let Ok(entries) = fs::read_dir(self.askantis.join(".apps")) else { return false };
        entries.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "json")).any(|e| {
            fs::read(e.path())
                .ok()
                .and_then(|b| serde_json::from_slice::<Usage>(&b).ok())
                .is_some_and(|u| u.app_id != app_id && u.models.iter().any(|m| *m == repo))
        })
    }

    /// Removes a repository an app downloaded, from every place, unless
    /// another app uses it. Returns the bytes freed, or `None` when kept.
    pub fn remove_if_unused(&self, target: &Path, app_id: &str, repo: &str) -> io::Result<Option<u64>> {
        if self.used_by_others(app_id, repo) {
            return Ok(None);
        }
        let mut freed = 0;
        for root in self.roots(target) {
            if unmounted_volume(&root).is_some() {
                continue;
            }
            let Ok(dir) = Cache::new(&root).repo_dir(repo) else { continue };
            if dir.is_dir() {
                freed += dir_size(&dir);
                fs::remove_dir_all(&dir)?;
            }
        }
        Ok(Some(freed))
    }
}

#[derive(Serialize, Deserialize)]
struct Usage {
    app_id: String,
    models: Vec<String>,
}

/// `hf:org/repo/file@rev` → `org/repo`.
fn normalize(id: &str) -> String {
    let id = id.strip_prefix("hf:").unwrap_or(id);
    let id = id.split('@').next().unwrap_or(id);
    id.split('/').take(2).collect::<Vec<_>>().join("/")
}

fn safe_name(app_id: &str) -> String {
    app_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

/// Bytes of the files in a folder (blobs; symlinks not followed).
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

#[cfg(target_os = "macos")]
fn exclude_from_backup(path: &Path) {
    let _ = std::process::Command::new("/usr/bin/tmutil").arg("addexclusion").arg(path).output();
}

#[cfg(not(target_os = "macos"))]
fn exclude_from_backup(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn places(vars: &[(&str, &str)], home: &Path) -> ModelPlaces {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ModelPlaces::resolve_with(&move |k| map.get(k).cloned(), home)
    }

    fn put(root: &Path, repo: &str, file: &str) {
        let dir = root.join(format!("models--{}", repo.replace('/', "--")));
        let commit = "a".repeat(40);
        fs::create_dir_all(dir.join("refs")).unwrap();
        fs::write(dir.join("refs/main"), &commit).unwrap();
        let snap = dir.join("snapshots").join(&commit);
        fs::create_dir_all(&snap).unwrap();
        fs::write(snap.join(file), b"model").unwrap();
    }

    #[test]
    fn standard_paths() {
        let home = Path::new("/Users/someone");
        let p = places(&[], home);
        assert_eq!(p.askantis, home.join("Library/Application Support/Askantis/Models"));
        assert_eq!(p.huggingface, home.join(".cache/huggingface/hub"));
        let p = places(&[("ASKANTIS_MODELS", "~/m"), ("HF_HOME", "/x")], home);
        assert_eq!(p.askantis, home.join("m"));
        assert_eq!(p.huggingface, Path::new("/x/hub"));
    }

    #[test]
    fn huggingface_detection_and_default_place() {
        let home = tempfile::tempdir().unwrap();
        let p = places(&[], home.path());
        assert!(!p.has_huggingface());
        assert_eq!(p.default_place(Setup::Tinker), Place::Askantis);
        // An empty cache folder doesn't count; a model does.
        fs::create_dir_all(&p.huggingface).unwrap();
        assert!(!p.has_huggingface());
        put(&p.huggingface, "org/repo", "f.bin");
        assert!(p.has_huggingface());
        assert_eq!(p.default_place(Setup::Tinker), Place::Huggingface);
        assert_eq!(p.default_place(Setup::Simple), Place::Askantis);
        // A variable counts without reading anything.
        let p = places(&[("HF_HOME", "/Volumes/NotThere/hf")], home.path());
        assert!(p.has_huggingface());
    }

    #[test]
    fn find_looks_in_target_first_then_elsewhere() {
        let home = tempfile::tempdir().unwrap();
        let p = places(&[], home.path());
        assert_eq!(p.find(&p.askantis, "org/repo", "f.bin", "main"), None);
        put(&p.huggingface, "org/repo", "f.bin");
        let found = p.find(&p.askantis, "org/repo", "f.bin", "main").unwrap();
        assert!(found.starts_with(&p.huggingface));
        put(&p.askantis, "org/repo", "f.bin");
        assert!(p.find(&p.askantis, "org/repo", "f.bin", "main").unwrap().starts_with(&p.askantis));
        assert_eq!(p.roots(&p.askantis).len(), 2);
        assert_eq!(p.path(Place::Custom, ""), p.askantis);
    }

    #[test]
    fn removal_keeps_what_another_app_uses() {
        let home = tempfile::tempdir().unwrap();
        let p = places(&[], home.path());
        put(&p.askantis, "org/shared", "f.bin");
        put(&p.askantis, "org/mine", "f.bin");
        p.record_usage("app.one", &["hf:org/shared/f.bin".into(), "org/mine".into()]).unwrap();
        p.record_usage("app.two", &["hf:org/shared@main".into()]).unwrap();
        assert!(p.used_by_others("app.one", "org/shared"));
        assert!(!p.used_by_others("app.one", "org/mine"));
        assert_eq!(p.remove_if_unused(&p.askantis, "app.one", "org/shared").unwrap(), None);
        assert!(p.remove_if_unused(&p.askantis, "app.one", "org/mine").unwrap().unwrap() > 0);
        assert!(p.find_repo(&p.askantis, "org/mine").is_none());
        assert!(p.find_repo(&p.askantis, "org/shared").is_some());
    }
}

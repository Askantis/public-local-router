//! What `localrouterd` remembers: connected apps (keyed by identity, never by
//! the app id they claim), role bindings, `lra_` keys and pending connect
//! requests. Saved to `apps.json` in the support folder, readable by the user only.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use localrouter_core::{
    AppRecord, Capability, ConnectRequest, RoleBinding, RoleDeclaration, WELL_KNOWN_ROLES, now_rfc3339,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppEntry {
    pub app_id: String,
    pub name: String,
    pub version: Option<String>,
    pub publisher: Option<String>,
    pub trust: String,
    pub connected_at: String,
    pub last_used: Option<String>,
    #[serde(default)]
    pub roles: Vec<RoleDeclaration>,
    /// Bindings the user chose for this app: role → model id.
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEntry {
    pub id: String,
    pub name: String,
    /// SHA-256 hex of the secret; the secret itself is never stored.
    pub hash: String,
    pub created: String,
    pub last_used: Option<String>,
    #[serde(default)]
    pub roles: Vec<RoleDeclaration>,
    #[serde(default)]
    pub bindings: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Askantis-signed apps connect without asking (PROTOCOL.md 5.2).
    pub auto_connect_askantis: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { auto_connect_askantis: true }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Store {
    /// approval key (PROTOCOL.md 5.2) → app
    #[serde(default)]
    pub apps: BTreeMap<String, AppEntry>,
    #[serde(default)]
    pub keys: Vec<KeyEntry>,
    /// Global bindings of the well-known roles: role → model id.
    #[serde(default)]
    pub global: BTreeMap<String, String>,
    #[serde(default)]
    pub settings: Settings,
}

impl Store {
    pub fn path() -> PathBuf {
        localrouter_core::support_dir().join("apps.json")
    }

    pub fn load() -> Self {
        fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path();
        let dir = path.parent().unwrap();
        fs::create_dir_all(dir)?;
        let tmp = dir.join(".apps.json.tmp");
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        f.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(tmp, path)
    }

    /// An app id a signed app has used can't be taken by an unsigned caller.
    pub fn app_id_owned_by_signed(&self, app_id: &str) -> bool {
        self.apps.values().any(|a| a.app_id == app_id && (a.trust == "askantis" || a.trust == "signed"))
    }

    pub fn key_by_secret(&self, secret: &str) -> Option<usize> {
        let hash = hex::encode(Sha256::digest(secret.as_bytes()));
        // Compare every key in constant time so timing doesn't tell which key matched.
        let mut found = None;
        for (i, k) in self.keys.iter().enumerate() {
            if constant_time_eq(k.hash.as_bytes(), hash.as_bytes()) {
                found = Some(i);
            }
        }
        found
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A new `lra_` key: 32 random bytes, base64url without padding (256 bits).
pub fn new_key_secret() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    format!("lra_{}", base64url(&bytes))
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

pub fn key_hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

// ─── Pending connect requests ───────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingStatus {
    Pending,
    Approved,
    Denied,
    Expired,
}

#[derive(Debug, Clone)]
pub struct Pending {
    pub identity_key: String,
    pub trust: String,
    pub request: ConnectRequest,
    pub status: PendingStatus,
    pub created: Instant,
}

pub const PENDING_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Default)]
pub struct PendingRequests {
    pub by_id: HashMap<String, Pending>,
}

impl PendingRequests {
    /// A request id with 128 bits of randomness.
    pub fn new_id() -> String {
        format!("cr_{}", uuid::Uuid::new_v4().simple())
    }

    pub fn expire(&mut self) {
        for p in self.by_id.values_mut() {
            if p.status == PendingStatus::Pending && p.created.elapsed() > PENDING_TTL {
                p.status = PendingStatus::Expired;
            }
        }
        self.by_id.retain(|_, p| p.created.elapsed() < PENDING_TTL * 2);
    }

    /// At most one waiting prompt per identity.
    pub fn waiting_for(&self, identity_key: &str) -> Option<String> {
        self.by_id
            .iter()
            .find(|(_, p)| p.identity_key == identity_key && p.status == PendingStatus::Pending)
            .map(|(id, _)| id.clone())
    }
}

// ─── Roles (PROTOCOL.md 6) ──────────────────────────────────────────────────

/// What resolution needs to know about an installed model.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub capabilities: Vec<Capability>,
    pub context_length: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    Model { id: String, source: &'static str },
    /// The user's binding for this app points at a model that isn't installed.
    BoundButMissing(String),
    Unbound,
}

/// The declaration for `role`: the app's own, or the well-known one.
pub fn declaration<'a>(declared: &'a [RoleDeclaration], role: &str) -> Option<std::borrow::Cow<'a, RoleDeclaration>> {
    if let Some(d) = declared.iter().find(|d| d.role == role) {
        return Some(std::borrow::Cow::Borrowed(d));
    }
    WELL_KNOWN_ROLES.iter().find(|(r, _)| *r == role).map(|(r, cap)| {
        std::borrow::Cow::Owned(RoleDeclaration {
            role: r.to_string(),
            capability: *cap,
            min_context: None,
            languages: vec![],
            suggestions: vec![],
            required: true,
            label: None,
        })
    })
}

fn fits(c: &Candidate, d: &RoleDeclaration) -> bool {
    c.capabilities.contains(&d.capability)
        && match (d.min_context, c.context_length) {
            (Some(need), Some(have)) => have >= need,
            _ => true,
        }
}

/// Resolve a role in the order of PROTOCOL.md 6.2.
pub fn resolve(
    decl: &RoleDeclaration,
    app_bindings: &BTreeMap<String, String>,
    global: &BTreeMap<String, String>,
    installed: &[Candidate],
) -> Resolution {
    let find = |id: &str| installed.iter().find(|c| c.id == id);
    // 1. The user's choice for this app; never silently replaced.
    if let Some(id) = app_bindings.get(&decl.role) {
        return match find(id) {
            Some(_) => Resolution::Model { id: id.clone(), source: "app" },
            None => Resolution::BoundButMissing(id.clone()),
        };
    }
    // 2. The global binding of the role, or of the well-known role with the same capability.
    let global_role = if WELL_KNOWN_ROLES.iter().any(|(r, _)| *r == decl.role) {
        decl.role.as_str()
    } else {
        decl.capability.well_known_role()
    };
    if let Some(id) = global.get(global_role)
        && let Some(c) = find(id)
            && fits(c, decl) {
                return Resolution::Model { id: id.clone(), source: "global" };
            }
    // 3. The app's suggestions that are installed and fit.
    for s in &decl.suggestions {
        if let Some(c) = find(&s.model_id())
            && fits(c, decl) {
                return Resolution::Model { id: c.id.clone(), source: "suggestion" };
            }
    }
    // Speech roles: a single installed model of the capability is the obvious choice.
    if matches!(decl.capability, Capability::Transcription | Capability::Diarization) {
        let mut matching = installed.iter().filter(|c| fits(c, decl));
        if let (Some(only), None) = (matching.next(), matching.next()) {
            return Resolution::Model { id: only.id.clone(), source: "global" };
        }
    }
    Resolution::Unbound
}

pub fn app_record(entry: &AppEntry, global: &BTreeMap<String, String>, installed: &[Candidate]) -> AppRecord {
    let mut roles: Vec<RoleBinding> = Vec::new();
    let mut names: Vec<&str> = entry.roles.iter().map(|d| d.role.as_str()).collect();
    for (r, _) in WELL_KNOWN_ROLES {
        if !names.contains(&r) {
            names.push(r);
        }
    }
    for name in names {
        let Some(decl) = declaration(&entry.roles, name) else { continue };
        let (model, source) = match resolve(&decl, &entry.bindings, global, installed) {
            Resolution::Model { id, source } => (Some(id), source),
            Resolution::BoundButMissing(id) => (Some(id), "app"),
            Resolution::Unbound => (None, "none"),
        };
        roles.push(RoleBinding {
            role: decl.role.clone(),
            capability: decl.capability,
            model,
            source: source.into(),
            label: decl.label.clone(),
            required: decl.required,
        });
    }
    AppRecord {
        app_id: entry.app_id.clone(),
        name: entry.name.clone(),
        version: entry.version.clone(),
        publisher: entry.publisher.clone(),
        trust: entry.trust.clone(),
        connected_at: Some(entry.connected_at.clone()),
        last_used: entry.last_used.clone(),
        roles,
    }
}

pub fn new_entry(req: &ConnectRequest, trust: &str) -> AppEntry {
    AppEntry {
        app_id: req.app_id.clone(),
        name: req.name.clone(),
        version: Some(req.version.clone()),
        publisher: req.publisher.clone(),
        trust: trust.into(),
        connected_at: now_rfc3339(),
        last_used: None,
        roles: req.roles.clone(),
        bindings: BTreeMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localrouter_core::ModelRef;

    fn cand(id: &str, caps: &[Capability], ctx: Option<u32>) -> Candidate {
        Candidate { id: id.into(), capabilities: caps.to_vec(), context_length: ctx }
    }

    fn chapters() -> RoleDeclaration {
        RoleDeclaration {
            role: "chapters".into(),
            capability: Capability::Chat,
            min_context: Some(32768),
            languages: vec!["de".into()],
            suggestions: vec![ModelRef { store: "ollama".into(), id: "qwen3.5:9b".into(), file: None, revision: None }],
            required: false,
            label: Some("Chapters".into()),
        }
    }

    #[test]
    fn resolution_order_follows_the_protocol() {
        let installed = vec![
            cand("qwen3.5:9b", &[Capability::Chat], Some(262_144)),
            cand("tiny:1b", &[Capability::Chat], Some(4096)),
            cand("big:27b", &[Capability::Chat], Some(131_072)),
        ];
        let none = BTreeMap::new();
        // 3. suggestion, when nothing is bound.
        assert_eq!(resolve(&chapters(), &none, &none, &installed), Resolution::Model { id: "qwen3.5:9b".into(), source: "suggestion" });
        // 2. global binding of the capability's well-known role ("chat") wins over suggestions…
        let global: BTreeMap<_, _> = [("chat".to_string(), "big:27b".to_string())].into();
        assert_eq!(resolve(&chapters(), &none, &global, &installed), Resolution::Model { id: "big:27b".into(), source: "global" });
        // …but not if it doesn't fit (context too small).
        let small: BTreeMap<_, _> = [("chat".to_string(), "tiny:1b".to_string())].into();
        assert_eq!(resolve(&chapters(), &none, &small, &installed), Resolution::Model { id: "qwen3.5:9b".into(), source: "suggestion" });
        // 1. the app binding wins, and a missing one is reported, not replaced.
        let app: BTreeMap<_, _> = [("chapters".to_string(), "tiny:1b".to_string())].into();
        assert_eq!(resolve(&chapters(), &app, &global, &installed), Resolution::Model { id: "tiny:1b".into(), source: "app" });
        let gone: BTreeMap<_, _> = [("chapters".to_string(), "deleted:7b".to_string())].into();
        assert_eq!(resolve(&chapters(), &gone, &global, &installed), Resolution::BoundButMissing("deleted:7b".into()));
        // Nothing fits.
        assert_eq!(resolve(&chapters(), &none, &none, &[cand("tiny:1b", &[Capability::Chat], Some(4096))]), Resolution::Unbound);
    }

    #[test]
    fn a_single_speech_model_is_used_without_binding() {
        let installed = vec![cand("hf:ggerganov/whisper.cpp/ggml-large-v3-turbo-q8_0.bin", &[Capability::Transcription], None)];
        let decl = declaration(&[], "transcribe").unwrap();
        let none = BTreeMap::new();
        assert!(matches!(resolve(&decl, &none, &none, &installed), Resolution::Model { source: "global", .. }));
        let two = vec![installed[0].clone(), cand("hf:askantis/gruezi-v1/ggml-model-q8_0.bin", &[Capability::Transcription], None)];
        assert_eq!(resolve(&decl, &none, &none, &two), Resolution::Unbound);
    }

    #[test]
    fn keys_are_long_random_and_stored_hashed() {
        let a = new_key_secret();
        let b = new_key_secret();
        assert_ne!(a, b);
        assert!(a.starts_with("lra_") && a.len() == 4 + 43, "{a}");
        let mut store = Store::default();
        store.keys.push(KeyEntry { id: "k1".into(), name: "tool".into(), hash: key_hash(&a), created: now_rfc3339(), last_used: None, roles: vec![], bindings: BTreeMap::new() });
        assert_eq!(store.key_by_secret(&a), Some(0));
        assert_eq!(store.key_by_secret(&b), None);
        assert!(!serde_json::to_string(&store).unwrap().contains(&a));
    }

    #[test]
    fn base64url_matches_the_standard() {
        assert_eq!(base64url(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }
}

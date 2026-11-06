//! The `hf` store's folders (PROTOCOL.md 2): the Askantis models folder and
//! the Hugging Face cache. Models are found in both; new downloads go to one
//! target, chosen in Local Router (default: [`ModelPlaces::default_place`]
//! for tinkerers). Checking a folder can wait on a macOS permission prompt,
//! so it runs off the request path and announces what it found.

use std::path::{Path, PathBuf};
use std::time::Duration;

use localrouter_core::{Endpoint, HfCacheInfo};
use localrouter_hf::{Cache, CacheLocation, CacheState, ModelPlaces, Place, Setup};
use serde_json::json;

use crate::app::{AppState, Shared};
use crate::events::Audience;

/// Where new models go: the place and its folder.
pub fn target(st: &AppState) -> (Place, PathBuf) {
    let store = st.store.read().unwrap();
    let place = store.settings.download_target.unwrap_or(Place::Askantis);
    let custom = store.settings.download_path.clone().unwrap_or_default();
    (place, st.places.path(place, &custom))
}

/// The target for a computer that never chose one: Hugging Face's cache if
/// it already uses Hugging Face, else the Askantis folder. Local Router's
/// users are tinkerers (PROTOCOL.md 2).
pub fn initial_target(places: &ModelPlaces) -> Place {
    places.default_place(Setup::Tinker)
}

/// Which place a folder is.
pub fn place_of(places: &ModelPlaces, path: &Path) -> Place {
    if path == places.askantis {
        Place::Askantis
    } else if path == places.huggingface {
        Place::Huggingface
    } else {
        Place::Custom
    }
}

/// Every folder to look in, target first, with its state (`None`: not checked yet).
pub fn ordered(places: &ModelPlaces, target: &Path, state: &dyn Fn(&Path) -> Option<CacheState>) -> Vec<(Place, PathBuf, Option<CacheState>)> {
    places
        .roots(target)
        .into_iter()
        .map(|p| {
            let s = state(&p);
            (place_of(places, &p), p, s)
        })
        .collect()
}

/// What one check found.
#[derive(Clone, PartialEq, Debug)]
struct Found {
    target: (Place, PathBuf),
    roots: Vec<(Place, PathBuf, Option<CacheState>)>,
}

impl Found {
    fn state_of(&self, path: &Path) -> Option<CacheState> {
        self.roots.iter().find(|(_, p, _)| p == path).and_then(|(_, _, s)| *s)
    }

    fn info(&self, path: &Path) -> HfCacheInfo {
        HfCacheInfo {
            path: Some(path.display().to_string()),
            state: self.state_of(path).map(|s| s.as_str()).unwrap_or("checking").into(),
        }
    }

    fn all_ok(&self) -> bool {
        self.roots.iter().all(|(_, _, s)| *s == Some(CacheState::Ok))
    }
}

/// Writes the `hf` store's fields into an endpoint (discovery file).
pub fn fill_endpoint(st: &AppState, ep: &mut Endpoint) {
    let (place, path) = target(st);
    ep.hf_cache = st.hf.read().unwrap().clone();
    ep.askantis_models = Some(st.askantis.read().unwrap().clone());
    ep.download_target = Some(place.as_str().into());
    ep.download_path = Some(path.display().to_string());
}

/// Checks the folders now and whenever asked, and every 15 s while one of
/// them can't be used (drive not connected, not allowed yet). Each change
/// updates the speech engine, the discovery file and the events.
pub async fn watch(st: Shared, endpoint_path: PathBuf) {
    let mut last: Option<Found> = None;
    loop {
        let (place, path) = target(&st);
        let places = st.places.clone();
        let target_path = path.clone();
        let found = tokio::task::spawn_blocking(move || {
            let roots = ordered(&places, &target_path, &|p| Some(CacheLocation::check(p)));
            Found { target: (place, target_path), roots }
        })
        .await
        .unwrap_or_else(|_| Found { target: (place, path.clone()), roots: vec![] });

        if last.as_ref() != Some(&found) {
            apply(&st, &found, last.as_ref(), &endpoint_path);
            last = Some(found.clone());
        }
        if found.all_ok() {
            st.hf_check.notified().await;
        } else {
            let _ = tokio::time::timeout(Duration::from_secs(15), st.hf_check.notified()).await;
        }
    }
}

fn apply(st: &AppState, found: &Found, before: Option<&Found>, endpoint_path: &Path) {
    let usable: Vec<(Place, Cache)> = found
        .roots
        .iter()
        .filter(|(_, _, s)| *s == Some(CacheState::Ok))
        .map(|(pl, p, _)| (*pl, Cache::new(p)))
        .collect();
    let target = (found.state_of(&found.target.1) == Some(CacheState::Ok)).then(|| (found.target.0, Cache::new(&found.target.1)));
    st.speech.set_roots(usable, target);

    let hf = found.info(&st.places.huggingface);
    let askantis = found.info(&st.places.askantis);
    let hf_changed = before.is_none_or(|b| b.info(&st.places.huggingface) != hf);
    let askantis_changed = before.is_none_or(|b| b.info(&st.places.askantis) != askantis);
    *st.hf.write().unwrap() = hf.clone();
    *st.askantis.write().unwrap() = askantis.clone();
    if let Ok(mut ep) = Endpoint::read(endpoint_path) {
        fill_endpoint(st, &mut ep);
        let _ = ep.write_atomic(endpoint_path);
    }
    if hf_changed {
        st.events.send("hf_cache.state", serde_json::to_value(&hf).unwrap(), Audience::Everyone);
    }
    if askantis_changed {
        st.events.send("askantis_models.state", serde_json::to_value(&askantis).unwrap(), Audience::Everyone);
    }
    if before.is_some_and(|b| b.target != found.target) {
        st.events.send(
            "download_target",
            json!({ "target": found.target.0.as_str(), "path": found.target.1.display().to_string() }),
            Audience::Everyone,
        );
    }
}

/// The Askantis folder is created (and kept out of backups) only when the
/// first model goes into it.
pub fn prepare(places: &ModelPlaces, place: Place) -> Result<(), localrouter_core::ApiError> {
    if place != Place::Askantis {
        return Ok(());
    }
    places
        .prepare_askantis()
        .map_err(|e| localrouter_core::ApiError::internal(format!("Couldn't create the Askantis models folder {}: {e}", places.askantis.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn places(home: &Path, vars: &[(&str, String)]) -> ModelPlaces {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ModelPlaces::resolve_with(&move |k| map.get(k).cloned(), home)
    }

    #[test]
    fn target_comes_first_and_places_are_named() {
        let home = tempfile::tempdir().unwrap();
        let p = places(home.path(), &[("ASKANTIS_MODELS", home.path().join("a").display().to_string())]);
        let ok = |_: &Path| Some(CacheState::Ok);
        let r = ordered(&p, &p.huggingface, &ok);
        assert_eq!(r.iter().map(|(pl, ..)| *pl).collect::<Vec<_>>(), vec![Place::Huggingface, Place::Askantis]);
        let r = ordered(&p, &p.askantis, &ok);
        assert_eq!(r.iter().map(|(pl, ..)| *pl).collect::<Vec<_>>(), vec![Place::Askantis, Place::Huggingface]);
        let custom = home.path().join("elsewhere");
        let r = ordered(&p, &custom, &ok);
        assert_eq!(r.iter().map(|(pl, ..)| *pl).collect::<Vec<_>>(), vec![Place::Custom, Place::Askantis, Place::Huggingface]);
    }

    #[test]
    fn initial_target_follows_huggingface_use() {
        let home = tempfile::tempdir().unwrap();
        let fresh = places(home.path(), &[]);
        assert_eq!(initial_target(&fresh), Place::Askantis);
        let with_var = places(home.path(), &[("HF_HUB_CACHE", home.path().join("hub").display().to_string())]);
        assert_eq!(initial_target(&with_var), Place::Huggingface);
    }

    #[test]
    fn askantis_folder_is_made_only_for_it() {
        let home = tempfile::tempdir().unwrap();
        let p = places(home.path(), &[("ASKANTIS_MODELS", home.path().join("m").display().to_string())]);
        prepare(&p, Place::Huggingface).unwrap();
        assert!(!p.askantis.exists());
        prepare(&p, Place::Askantis).unwrap();
        assert!(p.askantis.is_dir());
    }
}

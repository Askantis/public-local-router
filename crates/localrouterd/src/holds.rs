//! Models apps ask Local Router to keep loaded (PROTOCOL.md 7.2). A hold
//! belongs to one app and ends when the app lets go, disconnects, or its
//! `keep_alive` passes; the model is released when the last hold ends.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_KEEP: Duration = Duration::from_secs(15 * 60);
pub const MAX_KEEP: Duration = Duration::from_secs(4 * 60 * 60);

struct Hold {
    context: Option<u32>,
    until: Instant,
}

#[derive(Default)]
pub struct Holds {
    by_model: Mutex<HashMap<String, HashMap<String, Hold>>>,
}

impl Holds {
    /// Hold `model` for `key`. Returns the context to load it with: the
    /// largest any holder asked for.
    pub fn add(&self, model: &str, key: &str, context: Option<u32>, keep: Duration) -> Option<u32> {
        let mut m = self.by_model.lock().unwrap();
        let holders = m.entry(model.to_string()).or_default();
        holders.insert(key.to_string(), Hold { context, until: Instant::now() + keep.min(MAX_KEEP) });
        holders.values().filter_map(|h| h.context).max()
    }

    /// The largest context other apps hold `model` with, if any hold it.
    pub fn others(&self, model: &str, key: &str) -> Option<Option<u32>> {
        let m = self.by_model.lock().unwrap();
        let others: Vec<&Hold> = m.get(model)?.iter().filter(|(k, _)| *k != key).map(|(_, h)| h).collect();
        (!others.is_empty()).then(|| others.iter().filter_map(|h| h.context).max())
    }

    /// Let go of one hold; true if that was the last one.
    pub fn remove(&self, model: &str, key: &str) -> bool {
        let mut m = self.by_model.lock().unwrap();
        let Some(holders) = m.get_mut(model) else { return false };
        if holders.remove(key).is_none() {
            return false;
        }
        let last = holders.is_empty();
        if last {
            m.remove(model);
        }
        last
    }

    /// Every hold of an app that disconnected; the models nobody holds any more.
    pub fn remove_app(&self, key: &str) -> Vec<String> {
        self.retain(|k, _| k != key)
    }

    /// Holds whose `keep_alive` passed; the models nobody holds any more.
    pub fn expire(&self) -> Vec<String> {
        let now = Instant::now();
        self.retain(|_, h| h.until > now)
    }

    fn retain(&self, keep: impl Fn(&str, &Hold) -> bool) -> Vec<String> {
        let mut m = self.by_model.lock().unwrap();
        let mut released = Vec::new();
        m.retain(|model, holders| {
            let before = holders.len();
            holders.retain(|k, h| keep(k, h));
            if holders.is_empty() && before > 0 {
                released.push(model.clone());
                false
            } else {
                true
            }
        });
        released
    }

    /// `(holders, held by key)` for the inventory.
    pub fn count(&self, model: &str, key: &str) -> (usize, bool) {
        let m = self.by_model.lock().unwrap();
        m.get(model).map(|h| (h.len(), h.contains_key(key))).unwrap_or((0, false))
    }
}

/// `"15m"`, `"4h"`, `"90s"`, or a number of seconds; at most 4 hours.
pub fn parse_keep(v: Option<&serde_json::Value>) -> Result<Duration, String> {
    let Some(v) = v else { return Ok(DEFAULT_KEEP) };
    let secs = match v {
        serde_json::Value::Number(n) => n.as_f64().filter(|s| *s > 0.0).ok_or("keep_alive must be positive")?,
        serde_json::Value::String(s) => {
            let s = s.trim();
            let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len()));
            let n: f64 = num.parse().map_err(|_| format!("keep_alive {s:?} isn't a duration like 15m"))?;
            n * match unit {
                "" | "s" => 1.0,
                "m" => 60.0,
                "h" => 3600.0,
                _ => return Err(format!("keep_alive {s:?} isn't a duration like 15m")),
            }
        }
        _ => return Err("keep_alive is a duration like \"15m\"".into()),
    };
    if secs <= 0.0 {
        return Err("keep_alive must be positive".into());
    }
    Ok(Duration::from_secs_f64(secs).min(MAX_KEEP))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_last_hold_releases_the_model() {
        let h = Holds::default();
        assert_eq!(h.add("m", "a", Some(8192), DEFAULT_KEEP), Some(8192));
        assert_eq!(h.add("m", "b", Some(32768), DEFAULT_KEEP), Some(32768));
        assert_eq!(h.others("m", "a"), Some(Some(32768)));
        assert_eq!(h.count("m", "a"), (2, true));
        assert!(!h.remove("m", "a"));
        assert_eq!(h.others("m", "b"), None);
        assert_eq!(h.remove_app("b"), vec!["m".to_string()]);
        assert_eq!(h.count("m", "b"), (0, false));
    }

    #[test]
    fn holds_expire() {
        let h = Holds::default();
        h.add("m", "a", None, Duration::from_millis(1));
        h.add("n", "a", None, DEFAULT_KEEP);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(h.expire(), vec!["m".to_string()]);
        assert_eq!(h.count("n", "a"), (1, true));
    }

    #[test]
    fn keep_alive_values() {
        assert_eq!(parse_keep(None).unwrap(), DEFAULT_KEEP);
        assert_eq!(parse_keep(Some(&json!("90s"))).unwrap(), Duration::from_secs(90));
        assert_eq!(parse_keep(Some(&json!("2h"))).unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_keep(Some(&json!("10h"))).unwrap(), MAX_KEEP);
        assert_eq!(parse_keep(Some(&json!(30))).unwrap(), Duration::from_secs(30));
        for bad in [json!("soon"), json!(-1), json!("5d"), json!(true)] {
            assert!(parse_keep(Some(&bad)).is_err(), "{bad}");
        }
    }
}

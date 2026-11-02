//! Server-sent events (PROTOCOL.md 10): a broadcast channel plus a buffer of
//! the last 1000 events for `Last-Event-ID` resume.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast;

pub const BUFFER: usize = 1000;

/// Who an event is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    Everyone,
    /// One app, by approval key.
    App(String),
    /// Local Router's own window.
    Admin,
}

#[derive(Debug, Clone)]
pub struct Event {
    pub id: u64,
    pub kind: String,
    pub data: Value,
    pub audience: Audience,
}

pub struct Events {
    tx: broadcast::Sender<Event>,
    inner: Mutex<(u64, VecDeque<Event>)>,
}

impl Events {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Events { tx, inner: Mutex::new((0, VecDeque::with_capacity(BUFFER))) }
    }

    pub fn send(&self, kind: &str, data: Value, audience: Audience) {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        let ev = Event { id: g.0, kind: kind.into(), data, audience };
        if g.1.len() == BUFFER {
            g.1.pop_front();
        }
        g.1.push_back(ev.clone());
        let _ = self.tx.send(ev);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Events after `last_id`, or `None` if the buffer no longer reaches back
    /// that far (the client then gets `reset`).
    pub fn since(&self, last_id: u64) -> Option<Vec<Event>> {
        let g = self.inner.lock().unwrap();
        let oldest = g.1.front().map(|e| e.id).unwrap_or(g.0 + 1);
        if last_id + 1 < oldest && last_id < g.0 {
            return None;
        }
        Some(g.1.iter().filter(|e| e.id > last_id).cloned().collect())
    }
}

impl Default for Events {
    fn default() -> Self {
        Self::new()
    }
}

pub fn visible_to(ev: &Event, app_key: Option<&str>, admin: bool) -> bool {
    match &ev.audience {
        Audience::Everyone => true,
        Audience::Admin => admin,
        Audience::App(k) => admin || app_key == Some(k.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_and_reset() {
        let ev = Events::new();
        for i in 0..5 {
            ev.send("x", serde_json::json!(i), Audience::Everyone);
        }
        let after2: Vec<u64> = ev.since(2).unwrap().iter().map(|e| e.id).collect();
        assert_eq!(after2, vec![3, 4, 5]);
        assert!(ev.since(5).unwrap().is_empty());
        for i in 0..BUFFER + 10 {
            ev.send("y", serde_json::json!(i), Audience::Everyone);
        }
        assert!(ev.since(2).is_none(), "too old: reset");
    }

    #[test]
    fn apps_only_see_their_own_events() {
        let e = |a: Audience| Event { id: 1, kind: "k".into(), data: Value::Null, audience: a };
        assert!(visible_to(&e(Audience::Everyone), Some("a"), false));
        assert!(visible_to(&e(Audience::App("a".into())), Some("a"), false));
        assert!(!visible_to(&e(Audience::App("b".into())), Some("a"), false));
        assert!(!visible_to(&e(Audience::Admin), Some("a"), false));
        assert!(visible_to(&e(Audience::Admin), None, true));
    }
}

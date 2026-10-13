//! Status tray registry: the cards on the bottom Arcane drawer.
//!
//! Anything — a module, a worker thread, the app itself — registers a card by a stable id
//! and updates it in place; the UI thread mirrors the registry on the next pump. The registry
//! is process-global and thread-safe on purpose: the callers are not on the UI thread, and
//! a handle threaded through every one of them would be the whole cost of the feature.
//!
//! A card is a *status* (always shown, e.g. AI MODEL) or, at [`Level::Needs`], an ask that
//! needs the human: those are also lifted into the attention cards, so they count toward
//! the lip badge and hold the drawer open until seen. `on_activate` runs when it is clicked.
#![allow(dead_code)] // a registration API: callers arrive module by module
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Plain status.
    Info,
    /// Healthy, shown in the accent colour.
    Ok,
    /// Degraded; worth a glance.
    Warn,
    /// Needs the human's response.
    Needs,
}

type Activate = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct StatusItem {
    pub id: String,
    pub label: String,
    pub value: String,
    pub glyph: String,
    pub level: Level,
    /// Lower sorts first; ties keep registration order.
    pub order: i32,
    pub on_activate: Option<Activate>,
}

impl StatusItem {
    pub fn new(id: &str, label: &str, value: &str) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            value: value.into(),
            glyph: "✦".into(),
            level: Level::Info,
            order: 0,
            on_activate: None,
        }
    }
    pub fn level(mut self, l: Level) -> Self {
        self.level = l;
        self
    }
    pub fn glyph(mut self, g: &str) -> Self {
        self.glyph = g.into();
        self
    }
    pub fn order(mut self, o: i32) -> Self {
        self.order = o;
        self
    }
    pub fn on_activate(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_activate = Some(Arc::new(f));
        self
    }
}

static ITEMS: Mutex<Vec<StatusItem>> = Mutex::new(Vec::new());
static REV: AtomicU64 = AtomicU64::new(1);

fn items() -> MutexGuard<'static, Vec<StatusItem>> {
    ITEMS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Bumped on every change; the pump writes the UI only when it moved.
pub fn revision() -> u64 {
    REV.load(Ordering::Relaxed)
}

/// Register a card, replacing any with the same id (its position is kept).
pub fn register(item: StatusItem) {
    let mut v = items();
    match v.iter_mut().find(|i| i.id == item.id) {
        Some(slot) => *slot = item,
        None => v.push(item),
    }
    REV.fetch_add(1, Ordering::Relaxed);
}

/// Change a registered card's value and level; false when there is no such id. A no-op
/// update does not bump the revision, so a module may call this every tick.
pub fn update(id: &str, value: &str, level: Level) -> bool {
    let mut v = items();
    let Some(i) = v.iter_mut().find(|i| i.id == id) else {
        return false;
    };
    if i.value != value || i.level != level {
        i.value = value.into();
        i.level = level;
        REV.fetch_add(1, Ordering::Relaxed);
    }
    true
}

pub fn remove(id: &str) -> bool {
    let mut v = items();
    let n = v.len();
    v.retain(|i| i.id != id);
    let gone = v.len() != n;
    if gone {
        REV.fetch_add(1, Ordering::Relaxed);
    }
    gone
}

/// Run a card's click handler (outside the lock). False when it has none.
pub fn activate(id: &str) -> bool {
    let f = items().iter().find(|i| i.id == id).and_then(|i| i.on_activate.clone());
    f.map(|f| f()).is_some()
}

/// Display order: by `order`, then registration order.
pub fn snapshot() -> Vec<StatusItem> {
    let mut v = items().clone();
    v.sort_by_key(|i| i.order);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_update_remove_round_trip() {
        register(StatusItem::new("t.rt", "MODEL", "a"));
        let r = revision();
        assert!(update("t.rt", "b", Level::Ok));
        assert!(revision() > r);
        let r = revision();
        assert!(update("t.rt", "b", Level::Ok), "same value is still found");
        assert_eq!(revision(), r, "a no-op update must not wake the UI");
        assert_eq!(snapshot().iter().find(|i| i.id == "t.rt").unwrap().value, "b");
        assert!(remove("t.rt"));
        assert!(!remove("t.rt"));
        assert!(!update("t.rt", "c", Level::Info));
    }

    #[test]
    fn re_registering_replaces_and_activation_fires() {
        use std::sync::atomic::AtomicUsize;
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        register(StatusItem::new("t.act", "A", "1"));
        register(
            StatusItem::new("t.act", "A", "2")
                .level(Level::Needs)
                .on_activate(move || {
                    h.fetch_add(1, Ordering::Relaxed);
                }),
        );
        assert_eq!(snapshot().iter().filter(|i| i.id == "t.act").count(), 1);
        assert!(activate("t.act"));
        assert_eq!(hits.load(Ordering::Relaxed), 1);
        assert!(!activate("t.nope"));
        remove("t.act");
    }
}

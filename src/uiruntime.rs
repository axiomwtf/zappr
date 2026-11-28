//! UI runtime: native renderer + DOM bridge for web (SPEC §16).
//! v1: headless element model — windows/elements are ordinary values,
//! property mutation marks dirty, `ui.run` drives the frame loop.
//! The native renderer backend lands with codegen; the DOM bridge with
//! the wasm target. Element semantics (mutate → update, frame-locked,
//! lazy dirty tracking) live here so both backends share them.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::Arc;

/// One UI element. Elements are values; setting a property updates it.
#[derive(Debug)]
pub struct UiElement {
    pub kind: String,
    /// ordered props; mutation marks dirty → next tick renders one frame
    pub props: Vec<(String, crate::value::Value)>,
    pub children: Vec<Arc<Mutex<UiElement>>>,
    pub dirty: bool,
    /// event handlers: event name → callback (stored as Rc'd func value)
    pub handlers: Vec<(String, Arc<crate::value::FuncVal>)>,
}

impl UiElement {
    pub fn new(kind: &str) -> Self {
        UiElement {
            kind: kind.to_string(),
            props: Vec::new(),
            children: Vec::new(),
            dirty: true,
            handlers: Vec::new(),
        }
    }

    /// Mutate an element's property and it updates (§16.1).
    pub fn set(&mut self, name: &str, v: crate::value::Value) {
        if let Some((_, slot)) = self.props.iter_mut().find(|(n, _)| n == name) {
            *slot = v;
        } else {
            self.props.push((name.to_string(), v));
        }
        self.dirty = true;
    }

    pub fn get(&self, name: &str) -> Option<crate::value::Value> {
        self.props
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }
}

/// A registered route: pattern + page fn (§16.3). Patterns are
/// extensionless by default; `{param}` segments bind like arguments.
#[derive(Debug, Clone)]
pub struct Route {
    pub pattern: String,
    pub page: Option<Arc<crate::value::FuncVal>>,
}

/// The runtime: window registry, routes, navigation log, frame state.
#[derive(Debug, Default)]
pub struct UiRuntime {
    pub windows: Vec<Arc<Mutex<UiElement>>>,
    pub routes: Vec<Route>,
    pub navigations: Vec<String>,
    pub next_window: u32,
    pub last_handler: Option<Arc<crate::value::FuncVal>>,
    /// frame counter — frames emit only on tick boundaries (§16.2)
    pub frame: u64,
    /// dirty windows awaiting render
    pub dirty: Vec<usize>,
    /// §19.1 state lifetime: window address → last use (Hot while used,
    /// Cold when idle, Gone after stateTtl)
    pub last_used: HashMap<usize, std::time::Instant>,
    /// the declared stateTtl string (None → default 10m)
    pub state_ttl: Option<String>,
}

impl UiRuntime {
    pub fn new() -> Self {
        UiRuntime {
            windows: Vec::new(),
            routes: Vec::new(),
            navigations: Vec::new(),
            next_window: 1,
            last_handler: None,
            frame: 0,
            dirty: Vec::new(),
            last_used: HashMap::new(),
            state_ttl: None,
        }
    }

    /// `win.add.text("hi")` → child element value.
    /// First string arg becomes the element's main text/value prop.
    pub fn add_element(parent: &crate::value::Value, kind: &str, args: &[crate::value::Value]) -> crate::value::Value {
        let mut el = UiElement::new(kind);
        if let Some(v) = args.first() {
            match kind {
                "text" | "label" | "button" | "heading" => el.set("text", v.clone()),
                "input" => el.set("placeholder", v.clone()),
                _ => el.set("value", v.clone()),
            }
        }
        // attach to parent's children when parent is a Ui value
        if let crate::value::Value::Ui(parent_el) = parent {
            let mut p = parent_el.lock().unwrap();
            p.children.push(Arc::new(Mutex::new(el)));
            p.dirty = true;
            return crate::value::Value::Ui(p.children.last().unwrap().clone());
        }
        crate::value::Value::Ui(Arc::new(Mutex::new(el)))
    }

    /// `ui.window("demo", 800, 600)` → window element + registry entry.
    pub fn window(&mut self, args: &[crate::value::Value]) -> crate::value::Value {
        self.sweep();
        let title = args.first().map(|v| v.display()).unwrap_or_else(|| "window".into());
        let mut el = UiElement::new("window");
        el.set("title", crate::value::Value::str(title));
        el.set(
            "width",
            args.get(1).cloned().unwrap_or(crate::value::Value::Int(800)),
        );
        el.set(
            "height",
            args.get(2).cloned().unwrap_or(crate::value::Value::Int(600)),
        );
        let rc = Arc::new(Mutex::new(el));
        self.windows.push(rc.clone());
        // §19.1: a freshly created window's state is Hot
        self.touch(&rc);
        crate::value::Value::Ui(rc)
    }

    /// §19.1: mark a window's state as used now (Hot). Keyed by address —
    /// the same element retraced through Arc clones keeps its timestamp.
    pub fn touch(&mut self, w: &Arc<Mutex<UiElement>>) {
        let key = Arc::as_ptr(w) as usize;
        self.last_used.insert(key, std::time::Instant::now());
    }

    /// §19.1 sweep: evict windows whose state has been idle past `ttl`
    /// (Gone tier), LRU order. Revisiting starts fresh — the page fn
    /// simply runs again. Returns how many windows were evicted.
    pub fn evict_cold(&mut self, ttl: std::time::Duration) -> usize {
        let now = std::time::Instant::now();
        // LRU order: oldest last_used first
        let mut order: Vec<(usize, std::time::Instant)> = self
            .last_used
            .iter()
            .map(|(k, t)| (*k, *t))
            .collect();
        order.sort_by_key(|(_, t)| *t);
        let mut evicted = 0;
        for (key, t) in order {
            if now.duration_since(t) < ttl {
                break;
            }
            self.windows
                .retain(|w| Arc::as_ptr(w) as usize != key);
            self.last_used.remove(&key);
            evicted += 1;
        }
        if evicted > 0 {
            self.dirty.retain(|i| *i < self.windows.len());
        }
        evicted
    }

    /// The configured ttl (§19.1) — default 10 minutes.
    pub fn ttl(&self) -> std::time::Duration {
        self.state_ttl
            .as_deref()
            .and_then(parse_ttl)
            .unwrap_or(std::time::Duration::from_secs(600))
    }

    /// §19.1: run the cold-state sweep against the configured ttl.
    /// Called at every UiRuntime entry point — window creation, route
    /// registration, navigation and handler invocation — so idle state
    /// is reclaimed without a background thread.
    pub fn sweep(&mut self) -> usize {
        self.evict_cold(self.ttl())
    }
}

/// `10m` / `30s` / `2h` / `500ms` — the stateTtl subset (§19.1).
fn parse_ttl(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    match unit {
        "ms" => Some(std::time::Duration::from_millis(n)),
        "s" => Some(std::time::Duration::from_secs(n)),
        "m" => Some(std::time::Duration::from_secs(n * 60)),
        "h" => Some(std::time::Duration::from_secs(n * 3600)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_parsing() {
        assert_eq!(parse_ttl("500ms"), Some(std::time::Duration::from_millis(500)));
        assert_eq!(parse_ttl("10m"), Some(std::time::Duration::from_secs(600)));
        assert_eq!(parse_ttl("2h"), Some(std::time::Duration::from_secs(7200)));
        assert_eq!(parse_ttl("bogus"), None);
        assert_eq!(parse_ttl(""), None);
    }

    #[test]
    fn default_ttl_is_ten_minutes() {
        assert_eq!(UiRuntime::new().ttl(), std::time::Duration::from_secs(600));
    }

    #[test]
    fn cold_state_evicted_hot_kept() {
        let mut ui = UiRuntime::new();
        ui.state_ttl = Some("1ms".into());
        let a = ui.window(&[crate::value::Value::str("a")]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = ui.window(&[crate::value::Value::str("b")]);
        // a's state is past the ttl — swept when b's creation swept
        if let crate::value::Value::Ui(a_el) = &a {
            let key = Arc::as_ptr(a_el) as usize;
            assert!(!ui.last_used.contains_key(&key), "cold window should be evicted");
        }
        assert_eq!(ui.windows.len(), 1);
        // b is hot — still registered
        if let crate::value::Value::Ui(b_el) = &b {
            let key = Arc::as_ptr(b_el) as usize;
            assert!(ui.last_used.contains_key(&key));
        }
    }

    #[test]
    fn touch_keeps_window_alive() {
        let mut ui = UiRuntime::new();
        ui.state_ttl = Some("1ms".into());
        let a = ui.window(&[crate::value::Value::str("a")]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        if let crate::value::Value::Ui(el) = &a {
            ui.touch(el);
        }
        ui.sweep();
        assert_eq!(ui.windows.len(), 1, "recently touched window survives the sweep");
    }
}

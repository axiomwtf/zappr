//! UI runtime: native renderer + DOM bridge for web (SPEC §16).
//! v1: headless element model — windows/elements are ordinary values,
//! property mutation marks dirty, `ui.run` drives the frame loop.
//! The native renderer backend lands with codegen; the DOM bridge with
//! the wasm target. Element semantics (mutate → update, frame-locked,
//! lazy dirty tracking) live here so both backends share them.

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
        crate::value::Value::Ui(rc)
    }
}

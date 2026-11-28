//! Runtime values: the one kind of thing in Zappr is values (SPEC §7.4).
//! Types are values too.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Condvar, Mutex};

/// Type representations — values in their own right.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeRepr {
    Int,
    Float,
    Str,
    Bool,
    Bytes,
    List(Box<TypeRepr>),
    Map(Box<TypeRepr>, Box<TypeRepr>),
    Tuple(Vec<TypeRepr>),
    /// named record type `(name: str, age: int)` — a named tuple
    Record {
        name: String,
        fields: Vec<(String, TypeRepr)>,
    },
    /// union — tuple of type values (§7.6)
    Union(Vec<TypeRepr>),
    /// function type (opaque for v1)
    Func,
    /// `(T, null)` optional pad
    Optional(Box<TypeRepr>),
    Ref(Box<TypeRepr>),
    Null,
    Any,
    Err,
    Poison,
    Channel(Box<TypeRepr>),
}

impl TypeRepr {
    pub fn name(&self) -> String {
        match self {
            TypeRepr::Int => "int".into(),
            TypeRepr::Float => "float".into(),
            TypeRepr::Str => "str".into(),
            TypeRepr::Bool => "bool".into(),
            TypeRepr::Bytes => "bytes".into(),
            TypeRepr::List(t) => format!("List({})", t.name()),
            TypeRepr::Map(k, v) => format!("Map({}, {})", k.name(), v.name()),
            TypeRepr::Tuple(ts) => format!(
                "({})",
                ts.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ")
            ),
            TypeRepr::Record { name, .. } => name.clone(),
            TypeRepr::Union(vs) => vs
                .iter()
                .map(|t| t.name())
                .collect::<Vec<_>>()
                .join(" | "),
            TypeRepr::Func => "fn".into(),
            TypeRepr::Optional(t) => format!("({}, null)", t.name()),
            TypeRepr::Ref(t) => format!("&{}", t.name()),
            TypeRepr::Null => "null".into(),
            TypeRepr::Any => "any".into(),
            TypeRepr::Err => "Err".into(),
            TypeRepr::Poison => "poison".into(),
            TypeRepr::Channel(t) => format!("channel({})", t.name()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i128),
    Float(f64),
    Str(Arc<Mutex<String>>),
    Bool(bool),
    /// unnamed tuple — also serves as list element storage via List
    Tuple(Arc<Mutex<Vec<Value>>>),
    /// named record: ordered fields, `.name` and `.0` both work
    Record {
        ty: String,
        fields: Arc<Mutex<Vec<(String, Value)>>>,
    },
    List(Arc<Mutex<Vec<Value>>>),
    Map(Arc<Mutex<Vec<(Value, Value)>>>),
    Func(Arc<FuncVal>),
    /// a type used as a value: `!T = User`, `List(int)`
    Type(TypeRepr),
    Err(Arc<ErrVal>),
    /// poison values (§8.1, §9.3): wrongReturnType, missingArg, exclusiveAccess
    Poison {
        kind: PoisonKind,
        origin: String,
        expected: String,
        actual: String,
    },
    Channel(Arc<ChannelShared>),
    /// A weak handle to shared storage (§15): holds a downgrade of the
    /// referent's Arc, never keeps it alive. Scalars have no shared
    /// storage and are held strongly. Reading yields the (T, null) pad —
    /// an expired reference reads as the null side, never a dangling
    /// pointer.
    Weak(std::sync::Arc<WeakVal>),
    /// UI element — native renderer / DOM bridge share this value (§16)
    Ui(Arc<Mutex<crate::uiruntime::UiElement>>),
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoisonKind {
    WrongReturnType,
    MissingArg,
    ExclusiveAccess,
}

impl PoisonKind {
    pub fn name(&self) -> &'static str {
        match self {
            PoisonKind::WrongReturnType => "wrongReturnType",
            PoisonKind::MissingArg => "missingArg",
            PoisonKind::ExclusiveAccess => "exclusiveAccess",
        }
    }
}

#[derive(Debug)]
pub struct ErrVal {
    pub msg: String,
    pub origin: String,
}

/// The storage a weak handle points at (§15). Shared, mutable values are
/// downgraded Arcs into the referent's own cell; scalars are held strongly
/// (no shared storage to expire).
#[derive(Clone, Debug)]
pub enum WeakVal {
    Strong(Value),
    Str(std::sync::Weak<Mutex<String>>),
    Tuple(std::sync::Weak<Mutex<Vec<Value>>>),
    Record(String, std::sync::Weak<Mutex<Vec<(String, Value)>>>),
    List(std::sync::Weak<Mutex<Vec<Value>>>),
    Map(std::sync::Weak<Mutex<Vec<(Value, Value)>>>),
    Func(std::sync::Weak<FuncVal>),
}

/// Build a weak handle to a value (§15): downgrade the referent's storage
/// Arc so the handle never keeps it alive.
pub fn weak_of(v: &Value) -> WeakVal {
    match v {
        Value::Str(s) => WeakVal::Str(Arc::downgrade(s)),
        Value::Tuple(t) => WeakVal::Tuple(Arc::downgrade(t)),
        Value::Record { ty, fields } => WeakVal::Record(ty.clone(), Arc::downgrade(fields)),
        Value::List(l) => WeakVal::List(Arc::downgrade(l)),
        Value::Map(m) => WeakVal::Map(Arc::downgrade(m)),
        Value::Func(f) => WeakVal::Func(Arc::downgrade(f)),
        other => WeakVal::Strong(other.clone()),
    }
}

impl WeakVal {
    /// Reading a weak yields the (T, null) pad (§15): a live referent
    /// upgrades to its value; an expired one reads as null.
    pub fn upgrade(&self) -> Option<Value> {
        match self {
            WeakVal::Strong(v) => Some(v.clone()),
            WeakVal::Str(w) => w.upgrade().map(Value::Str),
            WeakVal::Tuple(w) => w.upgrade().map(Value::Tuple),
            WeakVal::Record(ty, w) => {
                w.upgrade().map(|f| Value::Record {
                    ty: ty.clone(),
                    fields: f,
                })
            }
            WeakVal::List(w) => w.upgrade().map(Value::List),
            WeakVal::Map(w) => w.upgrade().map(Value::Map),
            WeakVal::Func(w) => w.upgrade().map(Value::Func),
        }
    }
}

/// Lexical environment: stack of scopes (innermost last).
pub type Env = Vec<Arc<Mutex<HashMap<String, Value>>>>;

#[derive(Debug)]
pub struct FuncVal {
    pub name: String,
    /// parameter names; body evaluated by the interpreter
    pub params: Vec<String>,
    /// function body AST index into program — stored as raw pointer-free id
    pub body_id: usize,
    /// captured environment for closures (scopes cloned at capture time)
    pub env: Env,
}

#[derive(Debug, Default)]
pub struct ChannelState {
    pub queue: VecDeque<Value>,
    pub closed: bool,
    /// optional capacity bound (0 = unbounded)
    pub cap: usize,
}

/// A channel's shared half: state plus the condvar both sides wait on (§14).
#[derive(Debug)]
pub struct ChannelShared {
    st: Mutex<ChannelState>,
    cv: Condvar,
}

impl ChannelShared {
    pub fn new(cap: usize) -> Self {
        ChannelShared {
            st: Mutex::new(ChannelState {
                queue: VecDeque::new(),
                closed: false,
                cap,
            }),
            cv: Condvar::new(),
        }
    }

    /// Blocking send (§14). Blocks while a bounded channel is full;
    /// errors if the channel is closed.
    pub fn send(&self, v: Value) -> Result<(), String> {
        let mut g = self.st.lock().unwrap();
        while g.cap > 0 && g.queue.len() >= g.cap && !g.closed {
            g = self.cv.wait(g).unwrap();
        }
        if g.closed {
            return Err("send on closed channel".into());
        }
        g.queue.push_back(v);
        drop(g);
        self.cv.notify_all();
        Ok(())
    }

    /// Blocking receive. Returns `None` only when the channel is closed
    /// and drained — the "drain until closed" rule (§14).
    pub fn recv(&self) -> Option<Value> {
        let mut g = self.st.lock().unwrap();
        loop {
            if let Some(v) = g.queue.pop_front() {
                drop(g);
                self.cv.notify_all();
                return Some(v);
            }
            if g.closed {
                return None;
            }
            g = self.cv.wait(g).unwrap();
        }
    }

    /// Close the channel: wakes every waiter; queued values stay receivable.
    pub fn close(&self) {
        let mut g = self.st.lock().unwrap();
        g.closed = true;
        drop(g);
        self.cv.notify_all();
    }
}

impl Value {
    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(Arc::new(Mutex::new(s.into())))
    }

    pub fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.lock().unwrap().is_empty(),
            Value::List(l) => !l.lock().unwrap().is_empty(),
            Value::Map(m) => !m.lock().unwrap().is_empty(),
            Value::Tuple(t) => !t.lock().unwrap().is_empty(),
            Value::Record { fields, .. } => !fields.lock().unwrap().is_empty(),
            Value::Nothing => false,
            Value::Type(_) => true,
            Value::Func(_) => true,
            Value::Channel(_) => true,
            Value::Ui(_) => true,
            Value::Err(_) => false,
            Value::Poison { .. } => false,
            Value::Weak(_) => true,
        }
    }

    pub fn type_repr(&self) -> TypeRepr {
        match self {
            Value::Int(_) => TypeRepr::Int,
            Value::Float(_) => TypeRepr::Float,
            Value::Str(_) => TypeRepr::Str,
            Value::Bool(_) => TypeRepr::Bool,
            Value::Tuple(t) => TypeRepr::Tuple(
                // homogeneous unknown — element types aren't tracked; Any per slot
                vec![TypeRepr::Any; t.lock().unwrap().len()],
            ),
            Value::Record { ty, fields } => TypeRepr::Record {
                name: ty.clone(),
                fields: fields
                    .lock().unwrap()
                    .iter()
                    .map(|(n, v)| (n.clone(), v.type_repr()))
                    .collect(),
            },
            Value::List(_) => TypeRepr::List(Box::new(TypeRepr::Any)),
            Value::Map(_) => TypeRepr::Map(Box::new(TypeRepr::Any), Box::new(TypeRepr::Any)),
            Value::Func(_) => TypeRepr::Func,
            Value::Type(t) => t.clone(),
            Value::Err(_) => TypeRepr::Err,
            Value::Poison { .. } => TypeRepr::Poison,
            Value::Channel(_) => TypeRepr::Channel(Box::new(TypeRepr::Any)),
            Value::Ui(_) => TypeRepr::Any,
            Value::Weak(w) => match w.upgrade() {
                Some(v) => v.type_repr(),
                None => TypeRepr::Null,
            },
            Value::Nothing => TypeRepr::Any,
        }
    }

    /// autoNull (§7.5): padding is automatic — the null side of the pad
    /// satisfies any expected type, so a plain `(T)` annotation accepts a
    /// null; `(T)` and `(T, null)` are interchangeable; access
    /// auto-unwraps separately.
    pub fn satisfies_autonull(&self, expected: &TypeRepr) -> bool {
        if matches!(self, Value::Nothing) {
            return true;
        }
        if let (Value::Tuple(t), TypeRepr::Tuple(ts)) = (self, expected) {
            let t = t.lock().unwrap();
            if t.len() != ts.len() {
                // one side carries the implied null pad slot
                let pad_expected = ts.len() == t.len() + 1
                    && matches!(ts.last(), Some(TypeRepr::Null));
                let pad_value =
                    t.len() == ts.len() + 1 && matches!(t.last(), Some(Value::Nothing));
                if pad_expected || pad_value {
                    let n = t.len().min(ts.len());
                    return t[..n]
                        .iter()
                        .zip(ts[..n].iter())
                        .all(|(v, ty)| v.satisfies(ty) || matches!(ty, TypeRepr::Any));
                }
                return false;
            }
        }
        self.satisfies(expected)
    }

    /// Does this value satisfy the expected type? Used by `.fnType` and `as` casts.
    pub fn satisfies(&self, expected: &TypeRepr) -> bool {        match expected {
            TypeRepr::Any => true,
            TypeRepr::Null => matches!(self, Value::Nothing),
            TypeRepr::Int => matches!(self, Value::Int(_)),
            TypeRepr::Float => matches!(self, Value::Float(_) | Value::Int(_)),
            TypeRepr::Str => matches!(self, Value::Str(_)),
            TypeRepr::Bool => matches!(self, Value::Bool(_)),
            TypeRepr::Bytes => false,
            TypeRepr::Func => matches!(self, Value::Func(_)),
            TypeRepr::Err => matches!(self, Value::Err(_)),
            TypeRepr::Poison => matches!(self, Value::Poison { .. }),
            TypeRepr::List(inner) => match self {
                Value::List(l) => l
                    .lock().unwrap()
                    .iter()
                    .all(|v| v.satisfies(inner) || matches!(inner.as_ref(), TypeRepr::Any)),
                _ => false,
            },
            TypeRepr::Map(k, v) => match self {
                Value::Map(m) => m.lock().unwrap().iter().all(|(mk, mv)| {
                    mk.satisfies(k) && mv.satisfies(v)
                }),
                _ => false,
            },
            TypeRepr::Tuple(ts) => match self {
                Value::Tuple(t) => {
                    let t = t.lock().unwrap();
                    t.len() == ts.len()
                        && t.iter()
                            .zip(ts.iter())
                            .all(|(v, ty)| v.satisfies(ty) || matches!(ty, TypeRepr::Any))
                }
                Value::Record { fields, .. } => {
                    let f = fields.lock().unwrap();
                    f.len() == ts.len()
                        && f.iter()
                            .zip(ts.iter())
                            .all(|((_, v), ty)| v.satisfies(ty) || matches!(ty, TypeRepr::Any))
                }
                _ => false,
            },
            TypeRepr::Record { name, fields } => match self {
                Value::Record { ty, fields: f } => {
                    ty == name
                        && f.lock().unwrap().len() == fields.len()
                        && f.lock().unwrap()
                            .iter()
                            .zip(fields.iter())
                            .all(|((_, v), (_, fty))| {
                                v.satisfies(fty) || matches!(fty, TypeRepr::Any)
                            })
                }
                // anonymous record with same shape also satisfies (structural)
                _ => false,
            },
            TypeRepr::Union(vs) => vs.iter().any(|t| self.satisfies(t)),
            TypeRepr::Optional(inner) => {
                matches!(self, Value::Nothing) || self.satisfies(inner)
            }
            TypeRepr::Ref(inner) => self.satisfies(inner),
            TypeRepr::Channel(_) => matches!(self, Value::Channel(_)),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Value::Int(i) => i.to_string(),
            Value::Float(f) => {
                if f.fract() == 0.0 {
                    format!("{f:.1}")
                } else {
                    format!("{f}")
                }
            }
            Value::Str(s) => s.lock().unwrap().clone(),
            Value::Bool(b) => b.to_string(),
            Value::Tuple(t) => {
                let inner = t
                    .lock().unwrap()
                    .iter()
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inner})")
            }
            Value::Record { ty, fields } => {
                let inner = fields
                    .lock().unwrap()
                    .iter()
                    .map(|(n, v)| format!("{n}: {}", v.display()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{ty}({inner})")
            }
            Value::List(l) => {
                let inner = l
                    .lock().unwrap()
                    .iter()
                    .map(|v| v.display())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{inner}]")
            }
            Value::Map(m) => {
                let inner = m
                    .lock().unwrap()
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.display(), v.display()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{inner}}}")
            }
            Value::Func(f) => format!("<fn {}>", f.name),
            Value::Type(t) => t.name(),
            Value::Err(e) => {
                if e.origin.is_empty() || e.origin == "Err" {
                    format!("Err({})", e.msg)
                } else {
                    format!("Err({} — origin: {})", e.msg, e.origin)
                }
            }
            Value::Poison {
                kind,
                origin,
                expected,
                actual,
            } => format!(
                "{kind}(expected {expected}, got {actual}, at {origin})",
                kind = kind.name(),
            ),
            Value::Channel(_) => "<channel>".into(),
            Value::Ui(el) => format!("<ui.{}>", el.lock().unwrap().kind),
            Value::Weak(w) => match w.upgrade() {
                Some(v) => v.display(),
                None => "null".into(),
            },
            Value::Nothing => "null".into(),
        }
    }

    /// Poison check: accessing a poison as data fails loudly (§8.1, §9.3).
    pub fn ensure_not_poison(&self, use_site: &str) -> Result<(), String> {
        if let Value::Poison {
            kind,
            origin,
            expected,
            actual,
        } = self
        {
            return Err(format!(
                "{kind}: poisoned value used at {use_site} — expected {expected}, got {actual}, origin {origin}",
                kind = kind.name(),
            ));
        }
        Ok(())
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display())
    }
}

/// Loose equality for `==`/`!=`.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    // a weak reads as the (T, null) pad (§15): compare upgraded, an
    // expired weak equals only null
    match (a, b) {
        (Value::Weak(x), other) => match x.upgrade() {
            Some(v) => values_equal(&v, other),
            None => matches!(other, Value::Nothing),
        },
        (_, Value::Weak(y)) => match y.upgrade() {
            Some(v) => values_equal(a, &v),
            None => matches!(a, Value::Nothing),
        },
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => {
            (*x as f64) == *y
        }
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => *x.lock().unwrap() == *y.lock().unwrap(),
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Nothing, Value::Nothing) => true,
        (Value::Type(x), Value::Type(y)) => x == y,
        (Value::Tuple(x), Value::Tuple(y)) => {
            let (x, y) = (x.lock().unwrap(), y.lock().unwrap());
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| values_equal(a, b))
        }
        (Value::List(x), Value::List(y)) => {
            let (x, y) = (x.lock().unwrap(), y.lock().unwrap());
            x.len() == y.len() && x.iter().zip(y.iter()).all(|(a, b)| values_equal(a, b))
        }
        (Value::Record { ty: tx, fields: x }, Value::Record { ty: ty2, fields: y }) => {
            tx == ty2
                && {
                    let (x, y) = (x.lock().unwrap(), y.lock().unwrap());
                    x.len() == y.len()
                        && x.iter().zip(y.iter()).all(|((nx, a), (ny, b))| {
                            nx == ny && values_equal(a, b)
                        })
                }
        }
        (Value::Err(x), Value::Err(y)) => x.msg == y.msg,
        _ => false,
    }
}

/// Numeric coercion for arithmetic: int+float → float.
pub fn as_num(v: &Value) -> Option<Num> {
    match v {
        Value::Int(i) => Some(Num::Int(*i)),
        Value::Float(f) => Some(Num::Float(*f)),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Num {
    Int(i128),
    Float(f64),
}

impl Num {
    pub fn as_f64(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }
}

// §14: values cross task boundaries — the heap must be Send + Sync.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Value>();
    assert_send_sync::<Env>();
    assert_send_sync::<FuncVal>();
};

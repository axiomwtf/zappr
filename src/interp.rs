//! Tree-walking evaluator — the semantics engine behind `zap run`
//! (in-memory build + execute, SPEC §18) and shared by `zap check`.
//! Native codegen (SPEC §4) consumes the same AST.

use crate::parser::{
    BinOp, Expr, ForPat, Item, MatchArm, Program, Stmt, StrPart, TypeExpr, UnOp,
};
use crate::value::{values_equal, ChannelShared, ErrVal, FuncVal, Num, PoisonKind, TypeRepr, Value};
use crate::value::Env as Scope;
use std::sync::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Detached `spawn`ed tasks (§14) — joined before the program exits so
/// no task is ever orphaned.
static SPAWNED_TASKS: Mutex<Vec<std::thread::JoinHandle<()>>> = Mutex::new(Vec::new());

/// Join every spawned task. Called at program end (run_main): a Zappr
/// program outlives its tasks by construction — no orphans, guaranteed.
pub fn join_spawned_tasks() {
    let handles: Vec<_> = SPAWNED_TASKS.lock().unwrap().drain(..).collect();
    for h in handles {
        let _ = h.join();
    }
}

/// Names a statement declares at the top level of a `parallel` body —
/// the block's static publish surface (§14).
fn stmt_decl_names(stmt: &Stmt) -> Vec<String> {
    match stmt {
        Stmt::Decl { name, .. } => vec![name.clone()],
        Stmt::DeclTuple { names, .. } => names.clone(),
        _ => Vec::new(),
    }
}

#[derive(Debug)]
pub enum Flow {
    Normal,
    Break,
    Continue,
    Return(Value),
}

#[derive(Debug)]
pub struct RuntimeError {
    pub msg: String,
    pub line: usize,
}

/// Program state, read-only after load — shared with task threads (§14).
/// One `Arc<Prog>` is cloned into every `parallel` child and `spawn`ed
/// task instead of the whole AST being re-parsed or copied.
pub struct Prog {
    /// all function bodies across all loaded modules, flat. `Arc` per fn so
    /// readers clone the handle and drop the lock before executing (lambdas
    /// register into this table at runtime — §14 task threads share it).
    pub fns: std::sync::RwLock<Vec<Arc<FnDef>>>,
    /// name → fn indices (a name may exist in several modules)
    pub fn_map: HashMap<String, Vec<usize>>,
    /// module namespace → module
    pub modules: HashMap<String, Module>,
    /// entry module name (main.znz → "main")
    pub entry: String,
    /// zapYaml vals from Zappr.yaml
    pub zap_yaml: Value,
    /// ui element registry — shared, mutated by ui builtins on any task
    pub ui: Arc<Mutex<crate::uiruntime::UiRuntime>>,
    /// argv for the running program / custom cli command (§9.5)
    pub cli_args: Vec<String>,
    /// owning project (set by vm_for) — lets ui.run hand off to the
    /// compiled native renderer (§16)
    pub project: Option<std::sync::Arc<crate::project::Project>>,
    /// autoNull: true (§7.5) — absent values read as wrongReturnType poison
    pub auto_null: bool,
}

pub struct Vm {
    /// read-only program + config, shared across task threads
    pub prog: Arc<Prog>,
    /// module of the function currently executing (for bare-name resolution)
    cur_module: String,
    /// global scope (module top-level decls live in their module scope)
    scopes: Scope,
    /// line number tracking for errors
    pub cur_line: usize,
    /// §15.1 runtime exclusivity guard: per-cell access flags for borrows
    /// that escaped the checker's sight (closures, stored fields, tasks)
    access: Arc<Mutex<AccessInner>>,
    /// §14 cancellation flag for tasks inside a failing `parallel` block —
    /// checked at statement safe points; None on the main Vm
    cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// > 0 while evaluating a route pattern argument — `{param}` segments
    /// stay literal (§16.3)
    pattern_depth: usize,
}

/// Live access state for one guarded cell (§15.1).
#[derive(Default)]
struct Access {
    readers: u32,
    writer: bool,
    ro: String,
    wo: String,
}

#[derive(Default)]
struct AccessInner {
    /// cells marked as escaped — only these carry the access flag
    guarded: HashSet<usize>,
    /// currently-held accesses, keyed by cell
    active: HashMap<usize, Access>,
}

/// RAII access permit — releases the cell's read/write slot on drop.
struct AccessPermit {
    inner: Arc<Mutex<AccessInner>>,
    key: usize,
    write: bool,
    armed: bool,
}

impl AccessPermit {
    fn disarmed() -> Self {
        AccessPermit { inner: Arc::new(Mutex::new(AccessInner::default())), key: 0, write: false, armed: false }
    }
}

impl Drop for AccessPermit {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        if let Some(a) = g.active.get_mut(&self.key) {
            if self.write {
                a.writer = false;
            } else {
                a.readers = a.readers.saturating_sub(1);
            }
            if a.readers == 0 && !a.writer {
                g.active.remove(&self.key);
            }
        }
    }
}

#[derive(Debug)]
pub struct FnDef {
    pub name: String,
    pub module: String,
    pub params: Vec<(String, Option<TypeExpr>)>,
    pub body: Vec<Stmt>,
    /// scope captured at module load (module-level decls)
    pub env: Scope,
}

#[derive(Debug)]
pub struct Module;

impl Vm {
    pub fn new() -> Self {
        Vm {
            prog: Arc::new(Prog {
                fns: std::sync::RwLock::new(Vec::new()),
                fn_map: HashMap::new(),
                modules: HashMap::new(),
                entry: "main".into(),
                zap_yaml: Value::Map(Arc::new(Mutex::new(Vec::new()))),
                ui: Arc::new(Mutex::new(crate::uiruntime::UiRuntime::new())),
                cli_args: Vec::new(),
                project: None,
                auto_null: false,
            }),
            cur_module: "main".into(),
            scopes: vec![Arc::new(Mutex::new(HashMap::new()))],
            cur_line: 0,
            access: Arc::new(Mutex::new(AccessInner::default())),
            cancel: None,
            pattern_depth: 0,
        }
    }

    /// A task-thread Vm sharing the program, the §15.1 guard and the
    /// enclosing scopes — with its own cancel flag and cursor state (§14).
    fn fork_task(&self, cancel: Option<Arc<std::sync::atomic::AtomicBool>>) -> Vm {
        Vm {
            prog: self.prog.clone(),
            cur_module: self.cur_module.clone(),
            scopes: self.scopes.clone(),
            cur_line: self.cur_line,
            access: self.access.clone(),
            cancel,
            pattern_depth: 0,
        }
    }

    /// Load-time setters — call before any task thread exists; they take
    /// the one remaining strong reference apart via `Arc::get_mut`.
    fn prog_mut(&mut self) -> &mut Prog {
        Arc::get_mut(&mut self.prog).expect("prog mutated after task threads started")
    }

    pub fn set_cli_args(&mut self, args: Vec<String>) {
        self.prog_mut().cli_args = args;
    }

    /// autoNull: true (§7.5) — absent values read as wrongReturnType poison
    pub fn set_auto_null(&mut self, on: bool) {
        self.prog_mut().auto_null = on;
    }

    pub fn set_project(&mut self, p: Option<std::sync::Arc<crate::project::Project>>) {
        self.prog_mut().project = p;
    }

    pub fn set_zap_yaml(&mut self, v: Value) {
        self.prog_mut().zap_yaml = v;
    }

    // ---------- scope helpers ----------

    fn push_scope(&mut self) {
        self.scopes.push(Arc::new(Mutex::new(HashMap::new())));
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare(&mut self, name: &str, val: Value) -> Result<(), RuntimeError> {
        // no-shadowing (§6.3): name must not exist in any enclosing scope
        for s in self.scopes.iter().rev() {
            if s.lock().unwrap().contains_key(name) {
                return Err(RuntimeError {
                    msg: format!("redeclaration of `{name}` — names are declared exactly once (SPEC §6.3)"),
                    line: self.cur_line,
                });
            }
        }
        let scope = self.scopes.last().unwrap().clone();
        let _p = self
            .enter_scope(&scope, true, &format!("declare `{name}`"))
            .map_err(|m| RuntimeError { msg: m, line: self.cur_line })?;
        self.escape_value(&val);
        scope.lock().unwrap().insert(name.to_string(), val);
        Ok(())
    }

    fn set(&mut self, name: &str, val: Value) -> Result<(), RuntimeError> {
        let found = self
            .scopes
            .iter()
            .rev()
            .find(|s| s.lock().unwrap().contains_key(name))
            .cloned();
        if let Some(s) = found {
            let _p = self
                .enter_scope(&s, true, &format!("assign `{name}`"))
                .map_err(|m| RuntimeError { msg: m, line: self.cur_line })?;
            self.escape_value(&val);
            s.lock().unwrap().insert(name.to_string(), val);
            return Ok(());
        }
        Err(RuntimeError {
            msg: format!("reassignment of undeclared `{name}` — use `!{name} = ...` to declare"),
            line: self.cur_line,
        })
    }

    fn get(&self, name: &str) -> Option<Value> {
        for s in self.scopes.iter().rev() {
            let _p = match self.enter_scope(s, false, "binding read") {
                Ok(p) => p,
                Err(m) => {
                    return Some(Value::Poison {
                        kind: PoisonKind::ExclusiveAccess,
                        origin: m,
                        expected: "read access".into(),
                        actual: "conflicting access".into(),
                    })
                }
            };
            if let Some(v) = s.lock().unwrap().get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    // ---------- §15.1 exclusivity guard ----------

    fn cell_key<T>(rc: &Arc<T>) -> usize {
        Arc::as_ptr(rc) as usize
    }

    /// Identity of a shared value's storage cell, if it has one.
    fn value_key(v: &Value) -> Option<usize> {
        match v {
            Value::Tuple(t) => Some(Self::cell_key(t)),
            Value::List(l) => Some(Self::cell_key(l)),
            Value::Map(m) => Some(Self::cell_key(m)),
            Value::Record { fields, .. } => Some(Self::cell_key(fields)),
            Value::Channel(c) => Some(Self::cell_key(c)),
            Value::Ui(u) => Some(Self::cell_key(u)),
            _ => None,
        }
    }

    /// Cells shared by construction — always carry the flag.
    fn always_guarded(v: &Value) -> bool {
        matches!(v, Value::Channel(_) | Value::Ui(_))
    }

    /// Acquire a read (`write=false`) or write access on a value's cell.
    /// Unguarded cells cost one hash lookup and no tracking (§15.1: the
    /// flag is only carried where borrows escape the checker).
    fn enter_val(&self, v: &Value, write: bool, origin: &str) -> Result<AccessPermit, String> {
        // Channels are exempt from the §15.1 cell guard: a channel is its
        // own synchronization (Mutex + Condvar inside), and concurrent
        // send/recv across tasks is the sanctioned use (§14). Guarding the
        // cell would forbid the one pattern the channel exists for.
        if matches!(v, Value::Channel(_)) {
            return Ok(AccessPermit::disarmed());
        }
        let Some(key) = Self::value_key(v) else {
            return Ok(AccessPermit::disarmed());
        };
        self.access_enter(key, Self::always_guarded(v), write, origin)
    }

    fn enter_scope(
        &self,
        scope: &Arc<Mutex<HashMap<String, Value>>>,
        write: bool,
        origin: &str,
    ) -> Result<AccessPermit, String> {
        self.access_enter(Self::cell_key(scope), false, write, origin)
    }

    fn access_enter(
        &self,
        key: usize,
        always: bool,
        write: bool,
        origin: &str,
    ) -> Result<AccessPermit, String> {
        let inner = self.access.clone();
        let armed = {
            let mut g = inner.lock().unwrap();
            if !(always || g.guarded.contains(&key)) {
                false
            } else {
                let a = g.active.entry(key).or_default();
                if write {
                    if a.readers > 0 || a.writer {
                        let other = if a.writer {
                            format!("write `{}`", a.wo)
                        } else {
                            format!("{} read(s) by `{}`", a.readers, a.ro)
                        };
                        return Err(format!(
                            "exclusiveAccess: write `{origin}` conflicts with {other} — at most one writer or readers, never both (§15.1)"
                        ));
                    }
                    a.writer = true;
                    a.wo = origin.to_string();
                } else {
                    if a.writer {
                        return Err(format!(
                            "exclusiveAccess: read `{origin}` conflicts with write `{}` (§15.1)",
                            a.wo
                        ));
                    }
                    a.readers += 1;
                    if a.readers == 1 {
                        a.ro = origin.to_string();
                    }
                }
                true
            }
        };
        Ok(AccessPermit { inner, key, write, armed })
    }

    /// Mark a value's storage (and everything reachable through it) as
    /// escaped — it now carries the per-object access flag (§15.1).
    fn escape_value(&self, v: &Value) {
        let mut seen = HashSet::new();
        self.escape_value_seen(v, &mut seen);
    }

    fn escape_value_seen(&self, v: &Value, seen: &mut HashSet<usize>) {
        // values whose sharing lives outside their own cell
        match v {
            Value::Func(f) => {
                let id = Arc::as_ptr(f) as usize;
                if seen.insert(id) {
                    self.escape_env_seen(&f.env, seen);
                }
                return;
            }
            Value::Weak(w) => {
                let inner = w.lock().unwrap().clone();
                self.escape_value_seen(&inner, seen);
                return;
            }
            _ => {}
        }
        let Some(key) = Self::value_key(v) else { return };
        if !seen.insert(key) {
            return;
        }
        {
            let mut g = self.access.lock().unwrap();
            if !g.guarded.insert(key) {
                return; // already escaped — children were covered then
            }
        }
        // newly guarded: cover nested storage so deep escapes are seen
        match v {
            Value::Tuple(t) => {
                for x in t.lock().unwrap().iter() {
                    self.escape_value_seen(x, seen);
                }
            }
            Value::List(l) => {
                for x in l.lock().unwrap().iter() {
                    self.escape_value_seen(x, seen);
                }
            }
            Value::Map(m) => {
                for (k, x) in m.lock().unwrap().iter() {
                    self.escape_value_seen(k, seen);
                    self.escape_value_seen(x, seen);
                }
            }
            Value::Record { fields, .. } => {
                for (_, x) in fields.lock().unwrap().iter() {
                    self.escape_value_seen(x, seen);
                }
            }
            _ => {}
        }
    }

    /// Escape every binding visible in an environment — used when a
    /// closure or task captures scopes (§15.1). A scope already guarded
    /// had its contents covered when it was marked; new bindings escape
    /// themselves at declare/set time.
    fn escape_env(&self, env: &Scope) {
        let mut seen = HashSet::new();
        self.escape_env_seen(env, &mut seen);
    }

    fn escape_env_seen(&self, env: &Scope, seen: &mut HashSet<usize>) {
        for scope in env {
            let key = Self::cell_key(scope);
            if !seen.insert(key) {
                continue;
            }
            let fresh = {
                let mut g = self.access.lock().unwrap();
                g.guarded.insert(key)
            };
            if !fresh {
                continue; // already escaped — its bindings covered then
            }
            for (_, v) in scope.lock().unwrap().iter() {
                self.escape_value_seen(v, seen);
            }
        }
    }

    fn escape_scopes(&self) {
        self.escape_env(&self.scopes);
    }

    // ---------- loading ----------

    /// Load a parsed module: register its functions under its namespace.
    pub fn load_module(&mut self, module_name: &str, prog: &Program) -> Result<(), RuntimeError> {
        let module_scope = vec![Arc::new(Mutex::new(HashMap::new()))];
        let mut top_decls: Vec<&Stmt> = Vec::new();
        for item in &prog.items {
            match item {
                Item::Fn {
                    name,
                    params,
                    body,
                    line,
                } => {
                    let _ = line;
                    let p = self.prog_mut();
                    let id = p.fns.read().unwrap().len();
                    p.fns.write().unwrap().push(Arc::new(FnDef {
                        name: name.clone(),
                        module: module_name.to_string(),
                        params: params
                            .iter()
                            .map(|p| (p.name.clone(), p.ty.clone()))
                            .collect(),
                        body: body.clone(),
                        env: module_scope.clone(),
                    }));
                    p.fn_map.entry(name.clone()).or_default().push(id);
                    // qualified name resolves from anywhere
                    p.fn_map
                        .entry(format!("{module_name}.{name}"))
                        .or_default()
                        .push(id);
                }
                Item::TopDecl(decls) => top_decls.extend(decls.iter()),
                Item::Use(_) => {}
            }
        }
        // module-level `!` declarations (§7.6) evaluate once, now, into the
        // module scope every fn of this module shares — fns registered
        // first so init expressions may call them
        if !top_decls.is_empty() {
            let saved = self.scopes.clone();
            self.cur_module = module_name.to_string();
            self.scopes = module_scope.clone();
            for d in &top_decls {
                self.exec_stmt(d)?;
            }
            self.scopes = saved;
        }
        let p = self.prog_mut();
        p.modules.insert(module_name.to_string(), Module);
        Ok(())
    }

    /// Resolve `use.foo.bar` path → module name (fused camelCase).
    pub fn resolve_use(&self, path: &[String]) -> Option<String> {
        // path segments fuse: http/auth/session → httpAuthSession
        let fused = fuse_name(path);
        if self.prog.modules.contains_key(&fused) {
            return Some(fused);
        }
        // maybe path already is the fused name in one segment
        if path.len() == 1 && self.prog.modules.contains_key(&path[0]) {
            return Some(path[0].clone());
        }
        None
    }

    /// `module.fn` → callable value
    pub fn module_fn(&self, module: &str, fname: &str) -> Option<Value> {
        let key = format!("{module}.{fname}");
        let ids = self.prog.fn_map.get(&key)?;
        let id = *ids.first()?;
        let f = self.prog.fns.read().unwrap()[id].clone();
        Some(Value::Func(Arc::new(FuncVal {
            name: format!("{module}.{fname}"),
            params: f.params.iter().map(|(n, _)| n.clone()).collect(),
            body_id: id,
            env: f.env.clone(),
        })))
    }

    // ---------- execution ----------

    /// Call a function by qualified or bare name (used by `zap test`, CLI cmds).
    pub fn call_named(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let ids = self
            .prog.fn_map
            .get(name)
            .cloned()
            .ok_or_else(|| RuntimeError {
                msg: format!("no function `{name}`"),
                line: 0,
            })?;
        let id = ids
            .iter()
            .copied()
            .find(|id| self.prog.fns.read().unwrap()[*id].module == self.cur_module)
            .or_else(|| ids.first().copied())
            .unwrap();
        self.call_fn_id(id, args)
    }

    pub fn run_main(&mut self) -> Result<Value, RuntimeError> {
        // find main() — prefer entry module's main, else any main
        let key = format!("{}.main", self.prog.entry);
        let id = self
            .prog.fn_map
            .get(&key)
            .and_then(|v| v.first().copied())
            .or_else(|| {
                self.prog.fn_map
                    .get("main")
                    .and_then(|v| v.first().copied())
            })
            .ok_or_else(|| RuntimeError {
                msg: format!("no `main()` found (entry module `{}`)", self.prog.entry),
                line: 0,
            })?;
        let r = self.call_fn_id(id, Vec::new());
        // §14: the program outlives its spawned tasks — join them all
        join_spawned_tasks();
        r
    }

    /// Resolve a spawn/parallel callee to a callable `Func` value: a Func
    /// variable, or a user fn by (bare or module-qualified) name.
    fn resolve_callee_func(&self, callee: &Expr) -> Option<Value> {
        let Expr::Ident(name) = callee else {
            return None;
        };
        if let Some(v) = self.get(name) {
            if matches!(v, Value::Func(_)) {
                return Some(v);
            }
        }
        let ids = self
            .prog
            .fn_map
            .get(name)
            .cloned()
            .or_else(|| self.prog.fn_map.get(&format!("{}.{}", self.cur_module, name)).cloned())?;
        let id = *ids.first()?;
        // the table already holds Arc<FnDef> per fn — clone the handle
        Some(Value::Func(Arc::new(FuncVal {
            name: self.prog.fns.read().unwrap()[id].name.clone(),
            params: self.prog.fns.read().unwrap()[id].params.iter().map(|(n, _)| n.clone()).collect(),
            body_id: id,
            env: self.prog.fns.read().unwrap()[id].env.clone(),
        })))
    }

    fn call_fn_id(&mut self, id: usize, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // clone what we need; fns vec isn't mutated during call
        let (name, params, body, env, module) = {
            let f = self.prog.fns.read().unwrap()[id].clone();
            (
                f.name.clone(),
                f.params.clone(),
                f.body.clone(),
                f.env.clone(),
                f.module.clone(),
            )
        };
        let saved_module = std::mem::replace(&mut self.cur_module, module);
        let saved_scopes = std::mem::replace(&mut self.scopes, env);
        self.push_scope();
        // bind args positionally; soft arity (§9.2)
        let mut rest = Vec::new();
        for (i, (pname, _pty)) in params.iter().enumerate() {
            let v = if i < args.len() {
                args[i].clone()
            } else {
                Value::Poison {
                    kind: PoisonKind::MissingArg,
                    origin: format!("call to {name}"),
                    expected: pname.clone(),
                    actual: "<missing>".into(),
                }
            };
            self.declare(pname, v)?;
        }
        if args.len() > params.len() {
            rest = args[params.len()..].to_vec();
        }
        // `args` magic binding: args.count / args.rest
        let args_val = Value::Record {
            ty: "args".into(),
            fields: Arc::new(Mutex::new(vec![
                ("count".into(), Value::Int(args.len() as i128)),
                (
                    "rest".into(),
                    Value::List(Arc::new(Mutex::new(rest))),
                ),
            ])),
        };
        let _ = self.declare("args", args_val);
        let result = self.exec_block(&body).and_then(|flow| match flow {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Nothing),
        });
        self.scopes = saved_scopes;
        self.cur_module = saved_module;
        result
    }

    fn exec_block(&mut self, body: &[Stmt]) -> Result<Flow, RuntimeError> {
        for stmt in body {
            // cancellation safe point (§14): a failed parallel sibling stops
            // this task at the next statement boundary. Err, not Normal —
            // Normal would let an enclosing for-loop treat it as one done
            // iteration and keep spinning.
            if let Some(c) = &self.cancel {
                if c.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(RuntimeError {
                        msg: "cancelled — sibling task failed (§14)".into(),
                        line: self.cur_line,
                    });
                }
            }
            let flow = self.exec_stmt(stmt)?;
            if !matches!(flow, Flow::Normal) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_stmt(&mut self, stmt: &Stmt) -> Result<Flow, RuntimeError> {
        match stmt {
            Stmt::Decl {
                name,
                ty: _,
                init,
                line,
            } => {
                self.cur_line = *line;
                let v = self.eval(init)?;
                self.declare(name, self.post_decl(name, v))?;
                Ok(Flow::Normal)
            }
            Stmt::DeclTuple { names, init, line } => {
                self.cur_line = *line;
                let v = self.eval(init)?;
                v.ensure_not_poison("tuple destructure")
                    .map_err(|m| RuntimeError {
                        msg: m,
                        line: *line,
                    })?;
                let items = match &v {
                    Value::Tuple(t) => t.lock().unwrap().clone(),
                    Value::List(l) => l.lock().unwrap().clone(),
                    _ => vec![v.clone()],
                };
                for (i, n) in names.iter().enumerate() {
                    let val = items.get(i).cloned().unwrap_or(Value::Nothing);
                    self.declare(n, val)?;
                }
                Ok(Flow::Normal)
            }
            Stmt::Reassign {
                target,
                op,
                value,
                line,
            } => {
                self.cur_line = *line;
                match op {
                    crate::parser::ReassignOp::Inc | crate::parser::ReassignOp::Dec => {
                        let cur = self.get(target).ok_or_else(|| RuntimeError {
                            msg: format!("increment of undeclared `{target}`"),
                            line: *line,
                        })?;
                        let nv = match cur {
                            Value::Int(i) => Value::Int(i + if *op == crate::parser::ReassignOp::Inc { 1 } else { -1 }),
                            Value::Float(f) => Value::Float(f + if *op == crate::parser::ReassignOp::Inc { 1.0 } else { -1.0 }),
                            _ => {
                                return Err(RuntimeError {
                                    msg: format!("cannot increment `{target}` of type {}", cur.type_repr().name()),
                                    line: *line,
                                })
                            }
                        };
                        self.set(target, nv)?;
                        Ok(Flow::Normal)
                    }
                    crate::parser::ReassignOp::Set => {
                        let v = self.eval(value.as_ref().unwrap())?;
                        self.set(target, v)?;
                        Ok(Flow::Normal)
                    }
                }
            }
            Stmt::ReassignField {
                obj,
                field,
                value,
                line,
            } => {
                self.cur_line = *line;
                // `val` is ambient — resolve like zapYaml (§19.2); the map
                // is shared with prog, so the write reaches the CLI's
                // write-back
                let ov = match obj.as_str() {
                    "val" | "zapYaml" => self.prog.zap_yaml.clone(),
                    _ => self.get(obj).ok_or_else(|| RuntimeError {
                        msg: format!("unknown object `{obj}`"),
                        line: *line,
                    })?,
                };
                let vv = self.eval(value)?;
                self.field_set(&ov, field, vv).map_err(|m| RuntimeError {
                    msg: m,
                    line: *line,
                })?;
                Ok(Flow::Normal)
            }
            Stmt::Expr(e) => {
                self.eval(e)?;
                Ok(Flow::Normal)
            }
            Stmt::If {
                cond,
                bind,
                then_body,
                else_body,
                else_bind,
                line,
            } => {
                self.cur_line = *line;
                let c = self.eval(cond)?;
                // Err/poison condition values take the else branch (§8.2) —
                // `if x.fnType(T) as v {} else as e {}` is the boundary pattern
                let is_errish = matches!(c, Value::Err(_) | Value::Poison { .. });
                if !is_errish {
                    c.ensure_not_poison("if condition")
                        .map_err(|m| RuntimeError {
                            msg: m,
                            line: *line,
                        })?;
                }
                if c.truthy() {
                    self.push_scope();
                    if let Some(b) = bind {
                        // `if expr as u` — bind the (narrowed) value
                        self.declare(b, c.clone())?;
                    }
                    let r = self.exec_block(then_body);
                    self.pop_scope();
                    r
                } else if let Some(eb) = else_body {
                    self.push_scope();
                    // `else as e` — bind the condition value (often Err/poison)
                    if let Some(b) = else_bind {
                        self.declare(b, c)?;
                    }
                    let r = self.exec_block(eb);
                    self.pop_scope();
                    r
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::ForIn {
                pat,
                iter,
                body,
                line,
            } => {
                self.cur_line = *line;
                let it = self.eval(iter)?;
                it.ensure_not_poison("for..in")
                    .map_err(|m| RuntimeError {
                        msg: m,
                        line: *line,
                    })?;
                // $(n) count loop
                if let Expr::CountOf(inner) = iter {
                    let n = self.eval(inner)?;
                    let count = match n {
                        Value::Int(i) => i.max(0) as u64,
                        Value::Float(f) => f.max(0.0) as u64,
                        _ => {
                            return Err(RuntimeError {
                                msg: "count loop bound must be numeric".into(),
                                line: *line,
                            })
                        }
                    };
                    for i in 0..count {
                        self.push_scope();
                        match pat {
                            ForPat::Single(n) => {
                                self.declare(n, Value::Int(i as i128))?;
                            }
                            ForPat::Tuple(_) => {}
                        }
                        match self.exec_block(body)? {
                            Flow::Break => {
                                self.pop_scope();
                                break;
                            }
                            Flow::Continue => {}
                            Flow::Return(v) => {
                                self.pop_scope();
                                return Ok(Flow::Return(v));
                            }
                            Flow::Normal => {}
                        }
                        self.pop_scope();
                    }
                    return Ok(Flow::Normal);
                }
                // iterate lists/tuples/maps/channels
                // §15.1: the iteration holds a read access on the
                // collection — a write inside the body fails loudly
                let _iter_guard = match &it {
                    Value::List(_) | Value::Tuple(_) | Value::Map(_) => Some(
                        self.enter_val(&it, false, "for..in iteration")
                            .map_err(|m| RuntimeError { msg: m, line: *line })?,
                    ),
                    _ => None,
                };
                let mut idx = 0usize;
                loop {
                    let next = match &it {
                        Value::List(l) => {
                            let l = l.lock().unwrap();
                            if idx >= l.len() {
                                None
                            } else {
                                Some(l[idx].clone())
                            }
                        }
                        Value::Tuple(t) => {
                            let t = t.lock().unwrap();
                            if idx >= t.len() {
                                None
                            } else {
                                Some(t[idx].clone())
                            }
                        }
                        Value::Map(m) => {
                            let m = m.lock().unwrap();
                            if idx >= m.len() {
                                None
                            } else {
                                let (k, v) = m[idx].clone();
                                Some(Value::Tuple(Arc::new(Mutex::new(vec![k, v]))))
                            }
                        }
                        Value::Channel(ch) => {
                            // drain until closed (§14) — blocks on empty
                            ch.recv()
                        }
                        _ => {
                            return Err(RuntimeError {
                                msg: format!("cannot iterate {}", it.type_repr().name()),
                                line: *line,
                            })
                        }
                    };
                    let Some(item) = next else { break };
                    self.push_scope();
                    match pat {
                        ForPat::Single(n) => {
                            self.declare(n, item)?;
                        }
                        ForPat::Tuple(names) => {
                            let elems = match &item {
                                Value::Tuple(t) => t.lock().unwrap().clone(),
                                Value::List(l) => l.lock().unwrap().clone(),
                                _ => vec![item.clone()],
                            };
                            for (i, n) in names.iter().enumerate() {
                                self.declare(n, elems.get(i).cloned().unwrap_or(Value::Nothing))?;
                            }
                        }
                    }
                    match self.exec_block(body)? {
                        Flow::Break => {
                            self.pop_scope();
                            break;
                        }
                        Flow::Continue => {}
                        Flow::Return(v) => {
                            self.pop_scope();
                            return Ok(Flow::Return(v));
                        }
                        Flow::Normal => {}
                    }
                    idx += 1;
                    self.pop_scope();
                }
                Ok(Flow::Normal)
            }
            Stmt::ForCond { cond, body, line } => {
                self.cur_line = *line;
                loop {
                    let c = self.eval(cond)?;
                    if !c.truthy() {
                        break;
                    }
                    match self.exec_block(body)? {
                        Flow::Break => break,
                        Flow::Continue => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::ForInfinite { body, line } => {
                self.cur_line = *line;
                loop {
                    match self.exec_block(body)? {
                        Flow::Break => break,
                        Flow::Continue => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::ForCStyle {
                init,
                cond,
                post,
                body,
                line,
            } => {
                self.cur_line = *line;
                self.exec_block(init)?;
                loop {
                    let c = self.eval(cond)?;
                    if !c.truthy() {
                        break;
                    }
                    match self.exec_block(body)? {
                        Flow::Break => break,
                        Flow::Continue => {}
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Normal => {}
                    }
                    self.exec_block(post)?;
                }
                Ok(Flow::Normal)
            }
            Stmt::Match {
                expr,
                arms,
                line,
            } => {
                self.cur_line = *line;
                let v = self.eval(expr)?;
                v.ensure_not_poison("match")
                    .map_err(|m| RuntimeError {
                        msg: m,
                        line: *line,
                    })?;
                for arm in arms {
                    let matched = match (&arm.ty, &v) {
                        (None, _) => true, // else arm
                        (Some(_), _) => self.arm_matches(arm, &v),
                    };
                    if matched {
                        self.push_scope();
                        if let Some(b) = &arm.bind {
                            let inner = v.clone();
                            self.declare(b, inner)?;
                        }
                        let r = self.exec_block(&arm.body);
                        self.pop_scope();
                        return r;
                    }
                }
                Err(RuntimeError {
                    msg: "non-exhaustive match — no arm matched and no `else` (SPEC §7.6)".into(),
                    line: *line,
                })
            }
            Stmt::Return(e) => {
                let v = self.eval(e)?;
                Ok(Flow::Return(v))
            }
            Stmt::Parallel { body, line } => {
                self.cur_line = *line;
                self.escape_scopes();
                // Real §14 parallelism: one OS thread per top-level
                // statement of the block, joined at the end of the scope.
                // Children share the enclosing scopes (Mutex-guarded cells)
                // and publish their own `!` declarations upward at the join.
                // First failure arms the cancel flag — siblings stop at their
                // next statement safe point — and every published name
                // becomes the block's Err (§14 failure rule).
                use std::sync::atomic::{AtomicBool, Ordering};
                self.escape_scopes();
                let cancel = Arc::new(AtomicBool::new(false));
                let publish: Mutex<Vec<(String, Value)>> = Mutex::new(Vec::new());
                let first_err: Mutex<Option<RuntimeError>> = Mutex::new(None);
                let ret_flow: Mutex<Option<Value>> = Mutex::new(None);
                std::thread::scope(|s| {
                    for stmt in body {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let cancel = cancel.clone();
                        let mut child = self.fork_task(Some(cancel.clone()));
                        let publish = &publish;
                        let first_err = &first_err;
                        let ret_flow = &ret_flow;
                        s.spawn(move || {
                            child.push_scope();
                            let r = child.exec_stmt(stmt);
                            // the child's private scope holds its published decls
                            let decls: Vec<(String, Value)> = child
                                .scopes
                                .pop()
                                .map(|m| m.lock().unwrap().drain().collect())
                                .unwrap_or_default();
                            for (_, v) in &decls {
                                child.escape_value(v);
                            }
                            let mut pubq = publish.lock().unwrap();
                            match r {
                                Ok(Flow::Return(v)) => {
                                    for d in decls {
                                        pubq.push(d);
                                    }
                                    let mut rf = ret_flow.lock().unwrap();
                                    if rf.is_none() {
                                        *rf = Some(v);
                                    }
                                }
                                Ok(_) => {
                                    for d in decls {
                                        pubq.push(d);
                                    }
                                }
                                Err(e) => {
                                    // failed children's names are published too,
                                    // then poisoned at the join
                                    for d in decls {
                                        pubq.push(d);
                                    }
                                    let mut fe = first_err.lock().unwrap();
                                    if fe.is_none() {
                                        *fe = Some(e);
                                        drop(fe);
                                        cancel.store(true, Ordering::Relaxed);
                                    }
                                }
                            }
                        });
                    }
                });
                // join: all children done or cancelled — no orphans
                let parent = self.scopes.last().unwrap();
                let mut pg = parent.lock().unwrap();
                let mut pubq = publish.lock().unwrap();
                if let Some(e) = first_err.lock().unwrap().take() {
                    // §14 failure rule: no exception to throw — the failure
                    // becomes data. Every name the block published holds the
                    // same Err; names from cancelled children are poisoned
                    // too, so the block's full declared surface is present.
                    let ev = Value::Err(Arc::new(ErrVal {
                        msg: e.msg.clone(),
                        origin: format!("line {}", e.line),
                    }));
                    for stmt in body {
                        for name in stmt_decl_names(stmt) {
                            pg.insert(name, ev.clone());
                        }
                    }
                    pubq.drain(..);
                    drop(pubq);
                    drop(pg);
                    return Ok(Flow::Normal);
                }
                for (k, v) in pubq.drain(..) {
                    pg.insert(k, v);
                }
                drop(pubq);
                drop(pg);
                match ret_flow.lock().unwrap().take() {
                    Some(v) => Ok(Flow::Return(v)),
                    None => Ok(Flow::Normal),
                }
            }
            Stmt::Spawn(e) => {
                // Real §14 spawn: the task runs on its own OS thread from
                // now on — the channel it holds is the sync point. The
                // thread is registered and joined before the program exits:
                // no orphans, ever.
                if let Expr::Call { callee, args } = e {
                    let mut vals = Vec::new();
                    for a in args {
                        vals.push(self.eval(a)?);
                    }
                    for v in &vals {
                        self.escape_value(v);
                    }
                    let f = self
                        .resolve_callee_func(callee)
                        .ok_or_else(|| RuntimeError {
                            msg: "spawn expects a callable function".into(),
                            line: self.cur_line,
                        })?;
                    let mut child = self.fork_task(None);
                    let func = match &f {
                        Value::Func(fv) => fv.clone(),
                        _ => return Ok(Flow::Normal),
                    };
                    let handle = std::thread::spawn(move || {
                        if let Err(e) = child.call_func_value(&func, vals) {
                            eprintln!("zap: spawned task failed: {} (at line {})", e.msg, e.line);
                        }
                    });
                    SPAWNED_TASKS.lock().unwrap().push(handle);
                    Ok(Flow::Normal)
                } else {
                    self.eval(e)?;
                    Ok(Flow::Normal)
                }
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
            Stmt::Use(path) => {
                // namespace already resolved at load time; runtime no-op
                let _ = self.resolve_use(path);
                Ok(Flow::Normal)
            }
        }
    }

    fn arm_matches(&self, arm: &MatchArm, v: &Value) -> bool {
        let Some(ty) = &arm.ty else { return false };
        match ty {
            TypeExpr::Named(name) => match v {
                Value::Record { ty: t, .. } => t == name,
                _ => false,
            },
            TypeExpr::Union(vs) => vs.iter().any(|t| self.type_expr_matches(t, v)),
            _ => {
                let tr = self.type_expr_repr(ty);
                v.satisfies(&tr)
            }
        }
    }

    fn type_expr_matches(&self, ty: &TypeExpr, v: &Value) -> bool {
        match ty {
            TypeExpr::Named(name) => match v {
                Value::Record { ty: t, .. } => t == name,
                _ => false,
            },
            _ => {
                let tr = self.type_expr_repr(ty);
                v.satisfies(&tr)
            }
        }
    }

    fn type_expr_repr(&self, ty: &TypeExpr) -> TypeRepr {
        match ty {
            TypeExpr::Named(n) => match n.as_str() {
                "int" | "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" => {
                    TypeRepr::Int
                }
                "float" | "f32" | "f64" => TypeRepr::Float,
                "str" => TypeRepr::Str,
                "bool" => TypeRepr::Bool,
                _ => {
                    // user type — look up its value
                    match self.get(n) {
                        Some(Value::Type(t)) => t,
                        _ => TypeRepr::Any,
                    }
                }
            },
            TypeExpr::Ref(n) => {
                let r = self.type_expr_repr(&TypeExpr::Named(n.clone()));
                TypeRepr::Ref(Box::new(r))
            }
            TypeExpr::Null => TypeRepr::Null,
            TypeExpr::Tuple(ts) => TypeRepr::Tuple(ts.iter().map(|t| self.type_expr_repr(t)).collect()),
            TypeExpr::Union(vs) => {
                TypeRepr::Union(vs.iter().map(|t| self.type_expr_repr(t)).collect())
            }
            TypeExpr::App { name, args } => match name.as_str() {
                "List" => TypeRepr::List(Box::new(
                    args.first().map(|t| self.type_expr_repr(t)).unwrap_or(TypeRepr::Any),
                )),
                "Map" => TypeRepr::Map(
                    Box::new(args.first().map(|t| self.type_expr_repr(t)).unwrap_or(TypeRepr::Any)),
                    Box::new(args.get(1).map(|t| self.type_expr_repr(t)).unwrap_or(TypeRepr::Any)),
                ),
                _ => TypeRepr::Any,
            },
        }
    }

    /// PascalCase decl of a type value gets its name (§7.6).
    fn post_decl(&self, name: &str, v: Value) -> Value {
        if name.chars().next().map_or(false, |c| c.is_uppercase()) {
            if let Value::Type(TypeRepr::Record { name: n, fields }) = &v {
                if n.is_empty() {
                    return Value::Type(TypeRepr::Record {
                        name: name.to_string(),
                        fields: fields.clone(),
                    });
                }
            }
        }
        v
    }

    fn field_set(&self, obj: &Value, field: &str, val: Value) -> Result<(), String> {
        let _p = self.enter_val(obj, true, &format!("write `.{field}`"))?;
        self.escape_value(&val);
        match obj {
            Value::Ui(el) => {
                // mutate an element's property and it updates (§16.1)
                el.lock().unwrap().set(field, val);
                Ok(())
            }
            Value::Record { fields, ty } => {
                // ui element records stored as Record? (legacy) — allow growth
                let mut f = fields.lock().unwrap();
                if let Some((_, slot)) = f.iter_mut().find(|(n, _)| n == field) {
                    *slot = val;
                    Ok(())
                } else if ty.starts_with("ui.") {
                    f.push((field.to_string(), val));
                    Ok(())
                } else {
                    Err(format!("no field `{field}` on record"))
                }
            }
            Value::Map(m) => {
                let mut m = m.lock().unwrap();
                let key = Value::str(field);
                if let Some((_, v)) = m.iter_mut().find(|(k, _)| values_equal(k, &key)) {
                    *v = val;
                    Ok(())
                } else {
                    m.push((key, val));
                    Ok(())
                }
            }
            _ => Err(format!(
                "cannot set field `{field}` on {}",
                obj.type_repr().name()
            )),
        }
    }

    // ---------- expressions ----------

    pub fn eval(&mut self, e: &Expr) -> Result<Value, RuntimeError> {
        match e {
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Null => Ok(Value::Nothing),
            Expr::Str(parts) => self.eval_str(parts),
            Expr::Ident(name) => self.eval_ident(name),
            Expr::Tuple(items) => {
                let mut vals = Vec::new();
                for i in items {
                    vals.push(self.eval(i)?);
                }
                Ok(Value::Tuple(Arc::new(Mutex::new(vals))))
            }
            Expr::List(items) => {
                let mut vals = Vec::new();
                for i in items {
                    vals.push(self.eval(i)?);
                }
                Ok(Value::List(Arc::new(Mutex::new(vals))))
            }
            Expr::Map(entries) => {
                let mut vals = Vec::new();
                for (k, v) in entries {
                    // a bare-identifier key is a literal key, not a lookup —
                    // `{name: "world"}` must not require a binding `name`
                    let kv = match k {
                        Expr::Ident(n) => Value::str(n.clone()),
                        _ => self.eval(k)?,
                    };
                    vals.push((kv, self.eval(v)?));
                }
                Ok(Value::Map(Arc::new(Mutex::new(vals))))
            }
            Expr::Call { callee, args } => self.eval_call(callee, args),
            Expr::Field { obj, name } => {
                let ov = self.eval(obj)?;
                self.field_get(&ov, name)
            }
            Expr::Unary { op, expr } => {
                let v = self.eval(expr)?;
                match op {
                    UnOp::Neg => match v {
                        Value::Int(i) => Ok(Value::Int(-i)),
                        Value::Float(f) => Ok(Value::Float(-f)),
                        _ => return self.err(format!("cannot negate {}", v.type_repr().name())),
                    },
                    UnOp::Not => Ok(Value::Bool(!v.truthy())),
                }
            }
            Expr::Binary { op, l, r } => self.eval_binary(*op, l, r),
            Expr::Cast { expr, ty } => {
                let v = self.eval(expr)?;
                let tr = self.type_expr_repr(ty);
                // numeric cast: narrow to the named width — a cast truncates
                // because you said so (§7.1). `int` and unknown widths keep
                // the value as-is; non-numeric casts go through satisfies.
                if let TypeRepr::Int = tr {
                    if let Some(w) = cast_width(ty) {
                        return match v {
                            Value::Int(i) => Ok(Value::Int(trunc_int(i, Some(w)))),
                            Value::Float(f) => {
                                Ok(Value::Int(trunc_int(f as i128, Some(w))))
                            }
                            _ => Ok(Value::Poison {
                                kind: PoisonKind::WrongReturnType,
                                origin: "as cast".into(),
                                expected: tr.name(),
                                actual: v.type_repr().name(),
                            }),
                        };
                    }
                    return match v {
                        Value::Int(_) | Value::Float(_) => Ok(v),
                        _ => Ok(Value::Poison {
                            kind: PoisonKind::WrongReturnType,
                            origin: "as cast".into(),
                            expected: tr.name(),
                            actual: v.type_repr().name(),
                        }),
                    };
                }
                if v.satisfies(&tr) {
                    Ok(v)
                } else {
                    Ok(Value::Poison {
                        kind: PoisonKind::WrongReturnType,
                        origin: "as cast".into(),
                        expected: tr.name(),
                        actual: v.type_repr().name(),
                    })
                }
            }
            Expr::CountOf(inner) => self.eval(inner),
            Expr::DeclArg { .. } | Expr::NamedArg { .. } => {
                // only valid inside call args — handled by eval_call
                self.err("decl/named arg outside call".into())
            }
            Expr::RecordType { fields } => {
                let mut flds = Vec::new();
                for (n, t) in fields {
                    flds.push((n.clone(), self.type_expr_repr(t)));
                }
                Ok(Value::Type(TypeRepr::Record {
                    name: String::new(),
                    fields: flds,
                }))
            }
            Expr::Callback { call, body } => {
                // f(args) { ... } — trailing block becomes a lambda arg
                if let Expr::Call { callee, args } = call.as_ref() {
                    let mut vals = Vec::new();
                    for a in args {
                        vals.push(self.eval(a)?);
                    }
                    // register the lambda as a fn capturing current scopes —
                    // captured bindings escape the checker's sight (§15.1)
                    self.escape_scopes();
                    let id = self.prog.fns.read().unwrap().len();
                    self.prog.fns.write().unwrap().push(Arc::new(FnDef {
                        name: "<lambda>".into(),
                        module: self.cur_module.clone(),
                        params: vec![("event".into(), None)],
                        body: body.clone(),
                        env: self.scopes.clone(),
                    }));
                    vals.push(Value::Func(Arc::new(FuncVal {
                        name: "<lambda>".into(),
                        params: vec!["event".into()],
                        body_id: id,
                        env: self.scopes.clone(),
                    })));
                    let named = Vec::new();
                    return self.eval_callee(callee, vals, named);
                }
                self.err("callback on non-call".into())
            }
        }
    }

    fn err<T>(&self, msg: String) -> Result<T, RuntimeError> {
        Err(RuntimeError {
            msg,
            line: self.cur_line,
        })
    }

    fn eval_ident(&mut self, name: &str) -> Result<Value, RuntimeError> {
        // primitive type names are type values in expression position
        if let Some(t) = primitive_type(name) {
            return Ok(Value::Type(t));
        }
        // user function as a first-class value (route("/", Home), spawn f)
        if let Some(ids) = self.prog.fn_map.get(name) {
            let id = ids
                .iter()
                .copied()
                .find(|id| self.prog.fns.read().unwrap()[*id].module == self.cur_module)
                .or_else(|| ids.first().copied())
                .unwrap();
            let f = self.prog.fns.read().unwrap()[id].clone();
            return Ok(Value::Func(Arc::new(FuncVal {
                name: name.to_string(),
                params: f.params.iter().map(|(n, _)| n.clone()).collect(),
                body_id: id,
                env: f.env.clone(),
            })));
        }
        // builtins / ambient namespaces
        match name {
            "print" | "read" | "readInto" | "readDialogue" | "assert" | "typeof" | "len" | "str" | "int" | "float" | "env" | "clock" | "nav" | "ui" | "route" | "onClick" | "weak" | "channel" | "spawn" | "parallel" | "thisShell" | "math" | "json" | "exit" => {
                return Ok(Value::Func(Arc::new(FuncVal {
                    name: name.to_string(),
                    params: vec![],
                    body_id: usize::MAX,
                    env: vec![],
                })));
            }
            _ => {}
        }
        if name == "zapYaml" {
            return Ok(self.prog.zap_yaml.clone());
        }
        if name == "val" {
            // §19.2: val is the same tree as zapYaml — readable everywhere,
            // writable only inside a custom zap CLI (the map is shared, so
            // field writes mutate it and the CLI's caller persists them)
            return Ok(self.prog.zap_yaml.clone());
        }
        let v = self.get(name).ok_or_else(|| RuntimeError {
            msg: format!("unknown name `{name}`"),
            line: self.cur_line,
        })?;
        // autoNull (§7.5): optionality is inferred, padding is automatic,
        // access auto-unwraps — an absent value read surfaces as
        // wrongReturnType at first use. The explicit pad form is unchanged.
        if self.prog.auto_null && matches!(v, Value::Nothing) {
            return Ok(Value::Poison {
                kind: PoisonKind::WrongReturnType,
                origin: format!("optional access to `{name}`"),
                expected: "a value".into(),
                actual: "null".into(),
            });
        }
        Ok(v)
    }

    fn eval_str(&mut self, parts: &[StrPart]) -> Result<Value, RuntimeError> {
        let mut out = String::new();
        for p in parts {
            match p {
                StrPart::Lit(s) => out.push_str(s),
                StrPart::Interp(e) => {
                    // route patterns keep `{param}` literal (§16.3)
                    if self.pattern_depth > 0 {
                        if let Expr::Ident(n) = e {
                            out.push('{');
                            out.push_str(n);
                            out.push('}');
                            continue;
                        }
                    }
                    let v = self.eval(e)?;
                    out.push_str(&v.display());
                }
            }
        }
        Ok(Value::str(out))
    }

    fn field_get(&mut self, obj: &Value, name: &str) -> Result<Value, RuntimeError> {
        let _p = self
            .enter_val(obj, false, &format!("read `.{name}`"))
            .map_err(|m| RuntimeError { msg: m, line: self.cur_line })?;
        obj.ensure_not_poison(&format!(".{name}"))
            .map_err(|m| RuntimeError {
                msg: m,
                line: self.cur_line,
            })?;
        match obj {
            Value::Record { fields, .. } => {
                if let Some(idx) = name
                    .strip_prefix("__idx_")
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    let f = fields.lock().unwrap();
                    return Ok(f.get(idx).map(|(_, v)| v.clone()).unwrap_or(Value::Nothing));
                }
                let f = fields.lock().unwrap();
                for (n, v) in f.iter() {
                    if n == name {
                        return Ok(v.clone());
                    }
                }
                Err(RuntimeError {
                    msg: format!("no field `{name}`"),
                    line: self.cur_line,
                })
            }
            Value::Tuple(t) => {
                if let Some(idx) = name
                    .strip_prefix("__idx_")
                    .and_then(|s| s.parse::<usize>().ok())
                {
                    let t = t.lock().unwrap();
                    return Ok(t.get(idx).cloned().unwrap_or(Value::Nothing));
                }
                Err(RuntimeError {
                    msg: format!("no named field `{name}` on tuple"),
                    line: self.cur_line,
                })
            }
            Value::List(l) => match name {
                // zero-arg accessors are properties — the spec's own
                // examples pass `list.len` bare (§14, §10)
                "len" => Ok(Value::Int(l.lock().unwrap().len() as i128)),
                _ => {
                    // `.0` index access — records/tuples/lists share it (§7.3, §10)
                    if let Some(idx) = name
                        .strip_prefix("__idx_")
                        .and_then(|s| s.parse::<usize>().ok())
                    {
                        return Ok(l
                            .lock()
                            .unwrap()
                            .get(idx)
                            .cloned()
                            .unwrap_or(Value::Nothing));
                    }
                    Err(RuntimeError {
                        msg: format!("no field/method `{name}` on list"),
                        line: self.cur_line,
                    })
                }
            },
            Value::Map(m) => {
                // zapYaml.x and map.key read as field access (§19)
                let key = Value::str(name);
                if let Some((_, v)) = m.lock().unwrap().iter().find(|(k, _)| values_equal(k, &key)) {
                    return Ok(v.clone());
                }
                match name {
                    "len" => Ok(Value::Int(m.lock().unwrap().len() as i128)),
                    _ => Err(RuntimeError {
                        msg: format!("no field/method `{name}` on map"),
                        line: self.cur_line,
                    }),
                }
            }
            Value::Str(s) => match name {
                "len" => Ok(Value::Int(s.lock().unwrap().chars().count() as i128)),
                // zero-arg string methods read as properties (§14 style)
                "upper" | "lower" | "trim" => self.method_call(obj, name, vec![]),
                _ => Err(RuntimeError {
                    msg: format!("no field/method `{name}` on str"),
                    line: self.cur_line,
                }),
            },
            Value::Type(t) => Ok(Value::Func(Arc::new(FuncVal {
                name: format!("type.{}", t.name()),
                params: vec![],
                body_id: usize::MAX,
                env: vec![],
            }))),
            Value::Ui(el) => {
                if name == "add" {
                    // `win.add` — element factory; `.text(...)` etc. create children
                    return Ok(Value::Record {
                        ty: "ui.adder".into(),
                        fields: Arc::new(Mutex::new(vec![(
                            "__parent".to_string(),
                            obj.clone(),
                        )])),
                    });
                }
                match el.lock().unwrap().get(name) {
                    Some(v) => Ok(v),
                    None => Err(RuntimeError {
                        msg: format!("no property `{name}` on ui element"),
                        line: self.cur_line,
                    }),
                }
            }
            Value::Func(f) => {
                // method-style: fnType is the big one (§8.1)
                if name == "fnType" {
                    return Ok(Value::Func(Arc::new(FuncVal {
                        name: "fnType".into(),
                        params: vec!["T".into()],
                        body_id: usize::MAX,
                        env: vec![],
                    })));
                }
                let _ = f;
                Err(RuntimeError {
                    msg: format!("no method `{name}` on function"),
                    line: self.cur_line,
                })
            }
            Value::Channel(_) => Err(RuntimeError {
                msg: "no fields on channel".into(),
                line: self.cur_line,
            }),
            _ => Err(RuntimeError {
                msg: format!(
                    "cannot access field `{name}` on {}",
                    obj.type_repr().name()
                ),
                line: self.cur_line,
            }),
        }
    }

    fn eval_binary(&mut self, op: BinOp, l: &Expr, r: &Expr) -> Result<Value, RuntimeError> {
        // `and`/`or` short-circuit
        match op {
            BinOp::And => {
                let lv = self.eval(l)?;
                if !lv.truthy() {
                    return Ok(Value::Bool(false));
                }
                let rv = self.eval(r)?;
                return Ok(Value::Bool(rv.truthy()));
            }
            BinOp::Or => {
                let lv = self.eval(l)?;
                if lv.truthy() {
                    return Ok(Value::Bool(true));
                }
                let rv = self.eval(r)?;
                return Ok(Value::Bool(rv.truthy()));
            }
            _ => {}
        }
        let lv = self.eval(l)?;
        let rv = self.eval(r)?;
        lv.ensure_not_poison("binary operator")
            .and_then(|_| rv.ensure_not_poison("binary operator"))
            .map_err(|m| RuntimeError {
                msg: m,
                line: self.cur_line,
            })?;
        // type union: Type | Type → Union type value
        if op == BinOp::Or {
            if let (Value::Type(a), Value::Type(b)) = (&lv, &rv) {
                let mut vs = match a {
                    TypeRepr::Union(v) => v.clone(),
                    t => vec![t.clone()],
                };
                match b {
                    TypeRepr::Union(v) => vs.extend(v.iter().cloned()),
                    t => vs.push(t.clone()),
                }
                return Ok(Value::Type(TypeRepr::Union(vs)));
            }
        }
        match op {
            BinOp::Eq => Ok(Value::Bool(values_equal(&lv, &rv))),
            BinOp::Ne => Ok(Value::Bool(!values_equal(&lv, &rv))),
            BinOp::Lt | BinOp::Gt | BinOp::Le | BinOp::Ge => {
                use crate::value::as_num;
                match (as_num(&lv), as_num(&rv)) {
                    (Some(a), Some(b)) => {
                        let (a, b) = (a.as_f64(), b.as_f64());
                        let res = match op {
                            BinOp::Lt => a < b,
                            BinOp::Gt => a > b,
                            BinOp::Le => a <= b,
                            BinOp::Ge => a >= b,
                            _ => unreachable!(),
                        };
                        Ok(Value::Bool(res))
                    }
                    _ => match (&lv, &rv) {
                        (Value::Str(a), Value::Str(b)) => {
                            let (a, b) = (a.lock().unwrap().clone(), b.lock().unwrap().clone());
                            let res = match op {
                                BinOp::Lt => a < b,
                                BinOp::Gt => a > b,
                                BinOp::Le => a <= b,
                                BinOp::Ge => a >= b,
                                _ => unreachable!(),
                            };
                            Ok(Value::Bool(res))
                        }
                        _ => self.err(format!(
                            "cannot compare {} and {}",
                            lv.type_repr().name(),
                            rv.type_repr().name()
                        )),
                    },
                }
            }
            BinOp::Add => self.num_or_str_add(&lv, &rv, false),
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                self.num_binop(op, &lv, &rv)
            }
            BinOp::And | BinOp::Or => unreachable!(),
        }
    }

    fn num_or_str_add(&mut self, l: &Value, r: &Value, _sub: bool) -> Result<Value, RuntimeError> {
        use crate::value::as_num;
        if let (Value::Str(a), Value::Str(b)) = (l, r) {
            let mut s = a.lock().unwrap().clone();
            s.push_str(&b.lock().unwrap());
            return Ok(Value::str(s));
        }
        if let (Value::Str(a), b) = (l, r) {
            if !matches!(b, Value::Str(_)) {
                let mut s = a.lock().unwrap().clone();
                s.push_str(&b.display());
                return Ok(Value::str(s));
            }
        }
        if let (a, Value::Str(b)) = (l, r) {
            if !matches!(a, Value::Str(_)) {
                let mut s = a.display();
                s.push_str(&b.lock().unwrap());
                return Ok(Value::str(s));
            }
        }
        match (as_num(l), as_num(r)) {
            (Some(a), Some(b)) => match (a, b) {
                (Num::Int(x), Num::Int(y)) => Ok(Value::Int(x + y)),
                _ => Ok(Value::Float(a.as_f64() + b.as_f64())),
            },
            _ => self.err(format!(
                "cannot add {} and {}",
                l.type_repr().name(),
                r.type_repr().name()
            )),
        }
    }

    fn num_binop(&mut self, op: BinOp, l: &Value, r: &Value) -> Result<Value, RuntimeError> {
        use crate::value::as_num;
        let (Some(a), Some(b)) = (as_num(l), as_num(r)) else {
            return self.err(format!(
                "cannot apply operator to {} and {}",
                l.type_repr().name(),
                r.type_repr().name()
            ));
        };
        if let (Num::Int(x), Num::Int(y)) = (a, b) {
            let res = match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => {
                    if y == 0 {
                        return self.err("division by zero".into());
                    }
                    x / y
                }
                BinOp::Mod => {
                    if y == 0 {
                        return self.err("modulo by zero".into());
                    }
                    x % y
                }
                _ => unreachable!(),
            };
            return Ok(Value::Int(res));
        }
        let (a, b) = (a.as_f64(), b.as_f64());
        let res = match op {
            BinOp::Add => a + b,
            BinOp::Sub => a - b,
            BinOp::Mul => a * b,
            BinOp::Div => {
                if b == 0.0 {
                    return self.err("division by zero".into());
                }
                a / b
            }
            BinOp::Mod => a % b,
            _ => unreachable!(),
        };
        Ok(Value::Float(res))
    }

    // ---------- calls ----------

    fn eval_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<Value, RuntimeError> {
        // `readInto(x)` — read a token into an existing variable (§9.4)
        if let Expr::Ident(n) = callee {
            if n == "readInto" && args.len() == 1 {
                if let Expr::Ident(var) = &args[0] {
                    let v = self.builtin_read(vec![])?;
                    self.set(var, v)?;
                    return Ok(Value::Nothing);
                }
            }
        }
        // `read(int age)` — declare-and-read (§9.4): reads whitespace tokens
        // and declares each named var in the current scope.
        if let Expr::Ident(n) = callee {
            if n == "read" && args.iter().any(|a| matches!(a, Expr::DeclArg { .. })) {
                return self.eval_read_decl(args);
            }
        }
        // evaluate args first (DeclArg/NamedArg handled by callers that need them)
        let mut vals = Vec::new();
        let mut named: Vec<(String, Value)> = Vec::new();
        // route's pattern arg is literal — `{param}` segments are path
        // placeholders, not interpolation (§16.3)
        let is_route_pattern = matches!(callee, Expr::Ident(n) if n == "route")
            && args.first().is_some();
        for (i, a) in args.iter().enumerate() {
            match a {
                Expr::NamedArg { name, value } => {
                    named.push((name.clone(), self.eval(value)?));
                }
                Expr::DeclArg { .. } => {
                    return self.err("declare-and-read arg outside read()".into());
                }
                _ => {
                    if is_route_pattern && i == 0 {
                        self.pattern_depth += 1;
                        let v = self.eval(a);
                        self.pattern_depth -= 1;
                        vals.push(v?);
                    } else {
                        vals.push(self.eval(a)?);
                    }
                }
            }
        }
        self.eval_callee(callee, vals, named)
    }

    fn eval_read_decl(&mut self, args: &[Expr]) -> Result<Value, RuntimeError> {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).map_err(|e| {
            RuntimeError {
                msg: format!("read failed: {e}"),
                line: self.cur_line,
            }
        })?;
        let mut toks = line.split_whitespace();
        for a in args {
            if let Expr::DeclArg { ty, var } = a {
                let tok = toks.next().unwrap_or("");
                let v = match ty.as_str() {
                    "int" | "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" => {
                        Value::Int(tok.parse().unwrap_or_else(|_| {
                            eprintln!("zap: bad int input `{tok}` for `{var}`");
                            std::process::exit(1);
                        }))
                    }
                    "float" | "f32" | "f64" => Value::Float(tok.parse().unwrap_or(0.0)),
                    "str" => Value::str(tok),
                    "bool" => Value::Bool(tok == "true" || tok == "1"),
                    _ => Value::str(tok),
                };
                self.declare(var, v)?;
            }
        }
        Ok(Value::Nothing)
    }

    fn eval_callee(
        &mut self,
        callee: &Expr,
        args: Vec<Value>,
        named: Vec<(String, Value)>,
    ) -> Result<Value, RuntimeError> {
        match callee {
            Expr::Ident(name) => {
                // user function first — std is ambient but user code wins ties
                if let Some(v) = self.call_user_fn(name, args.clone())? {
                    return Ok(v);
                }
                // bare builtin? (read, assert, len, channel, ...)
                if let Some(v) = self.builtin_call(None, name, &args, &named)? {
                    return Ok(v);
                }
                // type constructors in expression position: List(int), Map(k,v),
                // Box(t1, t2, ...) — a heterogeneous product type (§10)
                if name == "List" || name == "Map" || name == "Box" {
                    let tys: Vec<TypeRepr> = args.iter().filter_map(value_to_type).collect();
                    let t = if name == "List" {
                        TypeRepr::List(Box::new(tys.first().cloned().unwrap_or(TypeRepr::Any)))
                    } else if name == "Map" {
                        TypeRepr::Map(
                            Box::new(tys.first().cloned().unwrap_or(TypeRepr::Any)),
                            Box::new(tys.get(1).cloned().unwrap_or(TypeRepr::Any)),
                        )
                    } else {
                        TypeRepr::Tuple(tys)
                    };
                    return Ok(Value::Type(t));
                }
                // type construction: `User(...)` where User is a type value
                if let Some(v) = self.get(name) {
                    if let Value::Type(t) = v {
                        return self.construct_type(&t, name, args, named);
                    }
                    if let Value::Func(f) = v {
                        return self.call_func_value(&f, args);
                    }
                }
                Err(RuntimeError {
                    msg: format!("cannot call `{name}`"),
                    line: self.cur_line,
                })
            }
            Expr::Field { obj, name } => {
                // ui element tree: win.add.text("hi") — detect before eval
                if let Expr::Field {
                    obj: add_obj,
                    name: add_name,
                } = obj.as_ref()
                {
                    if add_name == "add" {
                        let parent = self.eval(add_obj)?;
                        for a in &args {
                            self.escape_value(a);
                        }
                        let _pp = self
                            .enter_val(&parent, true, "ui element store")
                            .map_err(|m| RuntimeError { msg: m, line: self.cur_line })?;
                        return Ok(crate::uiruntime::UiRuntime::add_element(
                            &parent, name, &args,
                        ));
                    }
                }
                // module namespace: `mod.fn(...)` — resolve before evaluating
                // obj, since a pure namespace isn't a value.
                if let Expr::Ident(modname) = obj.as_ref() {
                    if self.prog.modules.contains_key(modname) {
                        if let Some(v) = self.module_fn(modname, name) {
                            if let Value::Func(f) = v {
                                return self.call_func_value(&f, args);
                            }
                        }
                        return Err(RuntimeError {
                            msg: format!("no function `{name}` in module `{modname}`"),
                            line: self.cur_line,
                        });
                    }
                    // qualified builtin-ish: print.nl, env.get, ui.window, etc.
                    if let Some(v) = self.namespaced_builtin(modname, name, args.clone())? {
                        return Ok(v);
                    }
                }
                let ov = self.eval(obj)?;
                // method call on value: list.push, map.len, str.len, x.fnType
                self.method_call(&ov, name, args)
            }
            _ => {
                let cv = self.eval(callee)?;
                match cv {
                    Value::Func(f) => self.call_func_value(&f, args),
                    Value::Type(t) => self.construct_type(&t, "type", args, named),
                    _ => Err(RuntimeError {
                        msg: format!("cannot call {}", cv.type_repr().name()),
                        line: self.cur_line,
                    }),
                }
            }
        }
    }

    fn call_user_fn(&mut self, name: &str, args: Vec<Value>) -> Result<Option<Value>, RuntimeError> {
        let ids = match self.prog.fn_map.get(name) {
            Some(ids) => ids.clone(),
            None => return Ok(None),
        };
        // prefer a function from the caller's own module
        let id = ids
            .iter()
            .copied()
            .find(|id| self.prog.fns.read().unwrap()[*id].module == self.cur_module)
            .or_else(|| ids.first().copied())
            .unwrap();
        Ok(Some(self.call_fn_id(id, args)?))
    }

    /// Re-enter a FuncVal from the native GUI bridge's click callback.
    pub fn call_gui_func(&mut self, f: &Arc<FuncVal>, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.call_func_value(f, args)
    }

    /// §16.3 route dispatch: match `url` against registered routes.
    /// Extensionless routes never match `.html` URLs; a route explicitly
    /// named with `.html` does. `{param}` segments bind positionally.
    /// Unmatched URLs are loud errors — silent 404s hide typos.
    fn nav_dispatch(&mut self, url: &str) -> Result<Option<Value>, RuntimeError> {
        // strip a query string; the router sees only the path
        let path = url.split('?').next().unwrap_or(url);
        let url_html = path.ends_with(".html");
        let segments = |s: &str| {
            s.trim_matches('/')
                .split('/')
                .filter(|p| !p.is_empty())
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
        };
        let url_segs = segments(path);
        // §19.1: navigation is a ui entry point — sweep cold state first
        self.prog.ui.lock().unwrap().sweep();
        let routes = self.prog.ui.lock().unwrap().routes.clone();
        for r in &routes {
            let pat_html = r.pattern.ends_with(".html");
            if url_html != pat_html {
                continue;
            }
            let pat_segs = segments(&r.pattern);
            if pat_segs.len() != url_segs.len() {
                continue;
            }
            let mut params: Vec<Value> = Vec::new();
            let mut ok = true;
            for (p, u) in pat_segs.iter().zip(url_segs.iter()) {
                if p.starts_with('{') && p.ends_with('}') {
                    params.push(Value::str(u.clone()));
                } else if p != u {
                    ok = false;
                    break;
                }
            }
            if !ok {
                continue;
            }
            self.prog.ui.lock().unwrap().navigations.push(path.to_string());
            if let Some(page) = &r.page {
                let v = self.call_func_value(page, params)?;
                return Ok(Some(v));
            }
            return Ok(None);
        }
        Err(RuntimeError {
            msg: format!("nav.go: no route matches `{path}` (routes are extensionless; a `.html` URL matches only routes declared with `.html`) — SPEC §16.3"),
            line: self.cur_line,
        })
    }

    fn call_func_value(&mut self, f: &FuncVal, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if f.body_id == usize::MAX {
            // builtin marker — dispatch by name
            return self.builtin_dispatch(&f.name, args);
        }
        let saved_module = self.cur_module.clone();
        if f.body_id < self.prog.fns.read().unwrap().len() {
            self.cur_module = self.prog.fns.read().unwrap()[f.body_id].module.clone();
        }
        // closure: env captured in FuncVal
        let saved = std::mem::replace(&mut self.scopes, f.env.clone());
        self.push_scope();
        let mut rest = Vec::new();
        for (i, pname) in f.params.iter().enumerate() {
            let v = if i < args.len() {
                args[i].clone()
            } else {
                Value::Poison {
                    kind: PoisonKind::MissingArg,
                    origin: format!("call to {}", f.name),
                    expected: pname.clone(),
                    actual: "<missing>".into(),
                }
            };
            self.declare(pname, v)?;
        }
        if args.len() > f.params.len() {
            rest = args[f.params.len()..].to_vec();
        }
        let _ = self.declare(
            "args",
            Value::Record {
                ty: "args".into(),
                fields: Arc::new(Mutex::new(vec![
                    ("count".into(), Value::Int(args.len() as i128)),
                    ("rest".into(), Value::List(Arc::new(Mutex::new(rest)))),
                ])),
            },
        );
        let fdef = self.prog.fns.read().unwrap()[f.body_id].clone();
        let body = fdef.body.clone();
        let r = self.exec_block(&body).and_then(|fl| match fl {
            Flow::Return(v) => Ok(v),
            _ => Ok(Value::Nothing),
        });
        self.scopes = saved;
        self.cur_module = saved_module;
        r
    }

    /// `.fnType(T)` — runtime boundary check (§8.1).
    fn fn_type_check(&mut self, v: &Value, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let Some(tv) = args.first() else {
            return self.err("fnType expects a type argument".into());
        };
        let Some(tr) = value_to_type(tv) else {
            return self.err("fnType argument must be a type value".into());
        };
        if v.satisfies(&tr) {
            Ok(v.clone())
        } else {
            Ok(Value::Poison {
                kind: PoisonKind::WrongReturnType,
                origin: "fnType".into(),
                expected: tr.name(),
                actual: v.type_repr().name(),
            })
        }
    }

    fn construct_type(
        &mut self,
        t: &TypeRepr,
        name: &str,
        args: Vec<Value>,
        named: Vec<(String, Value)>,
    ) -> Result<Value, RuntimeError> {
        match t {
            TypeRepr::Record { name: tname, fields } => {
                let mut out: Vec<(String, Value)> = Vec::new();
                for (fname, fty) in fields {
                    // named arg wins, else positional
                    let v = named
                        .iter()
                        .find(|(n, _)| n == fname)
                        .map(|(_, v)| v.clone())
                        .or_else(|| {
                            // positional index
                            fields
                                .iter()
                                .position(|(n, _)| n == fname)
                                .and_then(|i| args.get(i).cloned())
                        })
                        .unwrap_or_else(|| default_for(fty));
                    let _ = fty;
                    out.push((fname.clone(), v));
                }
                Ok(Value::Record {
                    ty: if tname.is_empty() {
                        name.to_string()
                    } else {
                        tname.clone()
                    },
                    fields: Arc::new(Mutex::new(out)),
                })
            }
            TypeRepr::List(_) => Ok(Value::List(Arc::new(Mutex::new(args)))),
            TypeRepr::Tuple(ts) => {
                // Box(t1, ..., tn)(v1, ..., vn) — positional, defaults fill gaps
                let mut out = Vec::new();
                for (i, ty) in ts.iter().enumerate() {
                    out.push(args.get(i).cloned().unwrap_or_else(|| default_for(ty)));
                }
                Ok(Value::Tuple(Arc::new(Mutex::new(out))))
            }
            TypeRepr::Map(_, _) => {
                // Map(k,v){} literal style or pairs
                let mut m = Vec::new();
                for a in args {
                    if let Value::Tuple(t) = a {
                        let t = t.lock().unwrap();
                        if t.len() == 2 {
                            m.push((t[0].clone(), t[1].clone()));
                        }
                    }
                }
                Ok(Value::Map(Arc::new(Mutex::new(m))))
            }
            _ => Err(RuntimeError {
                msg: format!("cannot construct {}", t.name()),
                line: self.cur_line,
            }),
        }
    }

    fn method_call(&mut self, obj: &Value, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // `.fnType(T)` works on ANY value — it is a boundary tool (§8.1)
        if name == "fnType" {
            return self.fn_type_check(obj, args);
        }
        // §15.1: mutating methods take a write access; the rest read.
        // map/filter hold the read across user callbacks — a mutating
        // callback on the same cell fails as exclusiveAccess.
        let write = matches!(
            (obj, name),
            (Value::List(_), "push" | "pop" | "reverse")
                | (Value::Map(_), "set")
                | (Value::Channel(_), "send" | "close")
        );
        let _p = self
            .enter_val(obj, write, &format!("method `.{name}`"))
            .map_err(|m| RuntimeError { msg: m, line: self.cur_line })?;
        match (obj, name) {
            (Value::List(l), "push") => {
                if let Some(v) = args.first() {
                    self.escape_value(v);
                    l.lock().unwrap().push(v.clone());
                }
                Ok(Value::Nothing)
            }
            (Value::List(l), "len") => Ok(Value::Int(l.lock().unwrap().len() as i128)),
            (Value::List(l), "pop") => Ok(l.lock().unwrap().pop().unwrap_or(Value::Nothing)),
            (Value::List(l), "first") => Ok(l.lock().unwrap().first().cloned().unwrap_or(Value::Nothing)),
            (Value::List(l), "last") => Ok(l.lock().unwrap().last().cloned().unwrap_or(Value::Nothing)),
            (Value::List(l), "at") => {
                let i = args.first().map(|v| crate::value::as_num(v).map(|n| n.as_f64() as i64).unwrap_or(0)).unwrap_or(0);
                Ok(l.lock().unwrap().get(i.max(0) as usize).cloned().unwrap_or(Value::Nothing))
            }
            (Value::List(l), "contains") => {
                let needle = args.first().cloned().unwrap_or(Value::Nothing);
                Ok(Value::Bool(l.lock().unwrap().iter().any(|v| values_equal(v, &needle))))
            }
            (Value::List(l), "join") => {
                let sep = args.first().map(|v| v.display()).unwrap_or_default();
                let parts: Vec<String> = l.lock().unwrap().iter().map(|v| v.display()).collect();
                Ok(Value::str(parts.join(&sep)))
            }
            (Value::List(l), "reverse") => {
                l.lock().unwrap().reverse();
                Ok(Value::Nothing)
            }
            (Value::List(l), "map") | (Value::Tuple(l), "map") => {
                let f = match args.first() {
                    Some(Value::Func(f)) => f.clone(),
                    _ => return self.err("map expects a fn value".into()),
                };
                let src: Vec<Value> = l.lock().unwrap().clone();
                let mut out = Vec::with_capacity(src.len());
                for item in src {
                    out.push(self.call_func_value(&f, vec![item])?);
                }
                Ok(Value::List(Arc::new(Mutex::new(out))))
            }
            (Value::List(l), "filter") | (Value::Tuple(l), "filter") => {
                let f = match args.first() {
                    Some(Value::Func(f)) => f.clone(),
                    _ => return self.err("filter expects a fn value".into()),
                };
                let src: Vec<Value> = l.lock().unwrap().clone();
                let mut out = Vec::new();
                for item in src {
                    let keep = self.call_func_value(&f, vec![item.clone()])?;
                    if keep.truthy() {
                        out.push(item);
                    }
                }
                Ok(Value::List(Arc::new(Mutex::new(out))))
            }
            (Value::Map(m), "len") => Ok(Value::Int(m.lock().unwrap().len() as i128)),
            (Value::Map(m), "get") => {
                let k = args.first().cloned().unwrap_or(Value::Nothing);
                Ok(m.lock().unwrap().iter().find(|(mk, _)| values_equal(mk, &k)).map(|(_, v)| v.clone()).unwrap_or(Value::Nothing))
            }
            (Value::Map(m), "set") => {
                if args.len() >= 2 {
                    let k = args[0].clone();
                    let v = args[1].clone();
                    self.escape_value(&k);
                    self.escape_value(&v);
                    let mut mm = m.lock().unwrap();
                    if let Some(e) = mm.iter_mut().find(|(mk, _)| values_equal(mk, &k)) {
                        e.1 = v;
                    } else {
                        mm.push((k, v));
                    }
                }
                Ok(Value::Nothing)
            }
            (Value::Map(m), "has") => {
                let k = args.first().cloned().unwrap_or(Value::Nothing);
                Ok(Value::Bool(m.lock().unwrap().iter().any(|(mk, _)| values_equal(mk, &k))))
            }
            (Value::Map(m), "keys") => {
                Ok(Value::List(Arc::new(Mutex::new(m.lock().unwrap().iter().map(|(k, _)| k.clone()).collect()))))
            }
            (Value::Map(m), "values") => {
                Ok(Value::List(Arc::new(Mutex::new(m.lock().unwrap().iter().map(|(_, v)| v.clone()).collect()))))
            }
            (Value::Str(s), "len") => Ok(Value::Int(s.lock().unwrap().chars().count() as i128)),
            (Value::Record { fields, .. }, "len") => Ok(Value::Int(fields.lock().unwrap().len() as i128)),
            (Value::Tuple(t), "len") => Ok(Value::Int(t.lock().unwrap().len() as i128)),
            (Value::Str(s), "upper") => Ok(Value::str(s.lock().unwrap().to_uppercase())),
            (Value::Str(s), "lower") => Ok(Value::str(s.lock().unwrap().to_lowercase())),
            (Value::Str(s), "trim") => Ok(Value::str(s.lock().unwrap().trim().to_string())),
            (Value::Str(s), "contains") => {
                let needle = args.first().map(|v| v.display()).unwrap_or_default();
                Ok(Value::Bool(s.lock().unwrap().contains(&needle)))
            }
            (Value::Str(s), "split") => {
                let sep = args.first().map(|v| v.display()).unwrap_or_else(|| ",".into());
                let parts: Vec<Value> = s.lock().unwrap().split(&sep).map(|p| Value::str(p)).collect();
                Ok(Value::List(Arc::new(Mutex::new(parts))))
            }
            (Value::Channel(ch), "send") => {
                if let Some(v) = args.first() {
                    self.escape_value(v);
                    ch.send(v.clone()).map_err(|m| RuntimeError {
                        msg: m,
                        line: self.cur_line,
                    })?;
                }
                Ok(Value::Nothing)
            }
            (Value::Channel(ch), "close") => {
                ch.close();
                Ok(Value::Nothing)
            }
            (Value::Record { ty, fields }, kind) if ty == "ui.adder" => {
                // `win.add` factory value — `.text(...)`, `.button(...)` create children
                let parent = fields
                    .lock().unwrap()
                    .iter()
                    .find(|(n, _)| n == "__parent")
                    .map(|(_, v)| v.clone())
                    .unwrap_or(Value::Nothing);
                for a in &args {
                    self.escape_value(a);
                }
                let _pp = self
                    .enter_val(&parent, true, "ui element store")
                    .map_err(|m| RuntimeError { msg: m, line: self.cur_line })?;
                Ok(crate::uiruntime::UiRuntime::add_element(&parent, kind, &args))
            }
            (Value::Channel(ch), "recv") => {
                Ok(ch.recv().unwrap_or(Value::Nothing))
            }
            _ => Err(RuntimeError {
                msg: format!(
                    "no method `{name}` on {}",
                    obj.type_repr().name()
                ),
                line: self.cur_line,
            }),
        }
    }

    // ---------- builtins ----------

    /// `print.nl(...)`, `read(...)`, etc. — flat verb-first (§9.4).
    /// `ns` is Some("print") etc. for namespaced calls.
    fn builtin_call(
        &mut self,
        ns: Option<&str>,
        name: &str,
        args: &[Value],
        _named: &[(String, Value)],
    ) -> Result<Option<Value>, RuntimeError> {
        let _ = ns;
        match name {
            "read" | "readInto" | "readDialogue" | "assert" | "typeof" | "len" | "str"
            | "int" | "float" | "weak" | "channel" | "exit" | "route" | "onClick" | "Err" => {
                Ok(Some(self.builtin_dispatch(name, args.to_vec())?))
            }
            _ => Ok(None),
        }
    }

    /// §9.5 arg parsing — one pass over cli_args.
    /// Returns (positionals, opts-as-ordered-pairs, raw).
    fn parse_cli_args(&self) -> (Vec<String>, Vec<(String, Value)>) {
        let mut pos = Vec::new();
        let mut opts: Vec<(String, Value)> = Vec::new();
        let mut literal = false;
        let a = &self.prog.cli_args;
        let mut i = 0;
        while i < a.len() {
            let t = &a[i];
            if literal {
                pos.push(t.clone());
                i += 1;
                continue;
            }
            if t == "--" {
                literal = true;
                i += 1;
                continue;
            }
            if !t.starts_with('-') || t == "-" {
                pos.push(t.clone());
                i += 1;
                continue;
            }
            // --name=value
            if let Some(rest) = t.strip_prefix("--") {
                if let Some(eq) = rest.find('=') {
                    opts.push((rest[..eq].to_string(), Value::str(rest[eq + 1..].to_string())));
                    i += 1;
                    continue;
                }
            }
            let name = t.strip_prefix("--").or_else(|| t.strip_prefix('-')).unwrap_or(t);
            if name.is_empty() {
                pos.push(t.clone());
                i += 1;
                continue;
            }
            // value form: next token not starting with '-' (or exactly "-")
            if i + 1 < a.len() {
                let nx = &a[i + 1];
                if !nx.starts_with('-') || nx == "-" {
                    opts.push((name.to_string(), Value::str(nx.clone())));
                    i += 2;
                    continue;
                }
            }
            opts.push((name.to_string(), Value::Bool(true)));
            i += 1;
        }
        (pos, opts)
    }

    fn args_list(&self) -> Value {
        Value::List(std::sync::Arc::new(std::sync::Mutex::new(
            self.prog.cli_args.iter().map(|s| Value::str(s.clone())).collect(),
        )))
    }

    fn args_positional(&self) -> Value {
        let (pos, _) = self.parse_cli_args();
        Value::List(std::sync::Arc::new(std::sync::Mutex::new(
            pos.into_iter().map(Value::str).collect(),
        )))
    }

    fn args_opts(&self, defaults: Option<&Value>) -> Value {
        let (_, opts) = self.parse_cli_args();
        // start from defaults map entries, parsed opts overlay (last wins)
        let mut entries: Vec<(Value, Value)> = Vec::new();
        if let Some(Value::Map(m)) = defaults {
            entries = m.lock().unwrap().clone();
        }
        for (k, v) in opts {
            let kv = Value::str(k);
            if let Some(e) = entries.iter_mut().find(|(ek, _)| values_equal(ek, &kv)) {
                e.1 = v;
            } else {
                entries.push((kv, v));
            }
        }
        Value::Map(std::sync::Arc::new(std::sync::Mutex::new(entries)))
    }

    fn namespaced_builtin(
        &mut self,
        ns: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Option<Value>, RuntimeError> {
        let full = format!("{ns}.{name}");
        match full.as_str() {
            "args.list" => return Ok(Some(self.args_list())),
            "args.opts" => return Ok(Some(self.args_opts(args.first()))),
            "args.positional" => return Ok(Some(self.args_positional())),
            "print.nl" => {
                let line = args.iter().map(|v| v.display()).collect::<Vec<_>>().join(" ");
                println!("{line}");
                Ok(Some(Value::Nothing))
            }
            "print.raw" => {
                for a in &args {
                    print!("{}", a.display());
                }
                Ok(Some(Value::Nothing))
            }
            "print.err" => {
                let line = args.iter().map(|v| v.display()).collect::<Vec<_>>().join(" ");
                eprintln!("{line}");
                Ok(Some(Value::Nothing))
            }
            "print.errNl" => {
                let line = args.iter().map(|v| v.display()).collect::<Vec<_>>().join(" ");
                eprintln!("{line}");
                Ok(Some(Value::Nothing))
            }
            "print.fmt" => {
                // args[0]'s string was already interpolated by eval_str —
                // {expr} in any string literal is the same machinery (§9.4)
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        print!(" ");
                    }
                    print!("{}", a.display());
                }
                println!();
                Ok(Some(Value::Nothing))
            }
            "env.get" => {
                let key = args.first().map(|v| v.display()).unwrap_or_default();
                match std::env::var(&key) {
                    Ok(v) => Ok(Some(Value::str(v))),
                    Err(_) => Ok(Some(Value::Nothing)),
                }
            }
            "env.set" => {
                if args.len() >= 2 {
                    let k = args[0].display();
                    let v = args[1].display();
                    // SAFETY: single-threaded interpreter; env mutation is process-global by design
                    unsafe { std::env::set_var(&k, &v) };
                }
                Ok(Some(Value::Nothing))
            }
            "clock.nowIso" => Ok(Some(Value::str(now_iso()))),
            "clock.now" => Ok(Some(Value::Float(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
            ))),
            "ui.window" => {
                for a in &args {
                    self.escape_value(a);
                }
                Ok(Some(self.prog.ui.lock().unwrap().window(&args)))
            }
            "ui.run" => {
                // ui.run drives the native renderer in-process through the
                // bridge: the UiElement tree is marshalled to the native
                // ZUiC ABI and click handlers re-enter this Vm (§16).
                let win = args
                    .first()
                    .and_then(|v| match v {
                        Value::Ui(e) => Some(e.clone()),
                        _ => None,
                    })
                    .or_else(|| self.prog.ui.lock().unwrap().windows.first().cloned());
                // §16.3 deep link: on web the loader passes the real URL
                // path as a leading argv entry — dispatch it as the initial
                // route before the first frame
                if let Some(initial) = self
                    .prog
                    .cli_args
                    .iter()
                    .find(|a| a.starts_with('/') && !a.starts_with("//"))
                {
                    let initial = initial.clone();
                    self.nav_dispatch(&initial)?;
                }
                #[cfg(any(unix, windows, target_os = "wasi"))]
                if let Some(w) = win {
                    let eff = self
                        .prog
                        .project
                        .as_ref()
                        .map(|p| p.config.efficient_redraw)
                        .unwrap_or(false);
                    let frame_ms = self
                        .prog
                        .project
                        .as_ref()
                        .map(|p| crate::config::frame_ms_of(&p.config.frame_rate))
                        .unwrap_or(0);
                    crate::guibridge::run(&w, self as *mut Vm, eff, frame_ms);
                    return Ok(Some(Value::Nothing));
                }
                // headless fallback only reachable without a window or on
                // targets with no native backend (tests)
                let mut ui = self.prog.ui.lock().unwrap();
                let mut any_dirty = false;
                for w in ui.windows.iter() {
                    let mut el = w.lock().unwrap();
                    if el.dirty {
                        el.dirty = false;
                        any_dirty = true;
                    }
                }
                if any_dirty {
                    ui.frame += 1;
                }
                drop(ui);
                Ok(Some(Value::Nothing))
            }
            "nav.go" => {
                // §16.3: dispatch the URL against registered routes —
                // extensionless routes do not match `.html` URLs and vice
                // versa; `{param}` segments bind like arguments
                let url = args.first().map(|v| v.display()).unwrap_or_default();
                self.nav_dispatch(&url)?;
                Ok(Some(Value::Nothing))
            }
            "thisShell.shellOut" => {
                let cmd = args.first().map(|v| v.display()).unwrap_or_default();
                match run_shell(&cmd) {
                    Ok(out) => Ok(Some(Value::str(out))),
                    Err(e) => Ok(Some(Value::Err(Arc::new(ErrVal {
                        msg: e,
                        origin: "thisShell.shellOut".into(),
                    })))),
                }
            }
            "math.abs" => Ok(args.first().map(|v| match v {
                Value::Int(i) => Value::Int(i.abs()),
                Value::Float(f) => Value::Float(f.abs()),
                _ => v.clone(),
            })),
            "math.min" | "math.max" => {
                let mut nums: Vec<f64> = args.iter().filter_map(|v| crate::value::as_num(v).map(|n| n.as_f64())).collect();
                if nums.is_empty() {
                    return Ok(Some(Value::Int(0)));
                }
                nums.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let v = if full == "math.min" { nums[0] } else { nums[nums.len()-1] };
                Ok(Some(Value::Float(v)))
            }
            "json.parse" => {
                let s = args.first().map(|v| v.display()).unwrap_or_default();
                Ok(Some(json_parse_min(&s)))
            }
            "json.stringify" => {
                let v = args.first().cloned().unwrap_or(Value::Nothing);
                Ok(Some(Value::str(json_stringify(&v))))
            }
            "exit" => {
                let code = args.first().map(|v| match v {
                    Value::Int(i) => *i as i32,
                    _ => 0,
                }).unwrap_or(0);
                std::process::exit(code);
            }
            _ => Ok(None),
        }
    }


    fn builtin_dispatch(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // bare builtins called as `name(...)`
        match name {
            "read" => self.builtin_read(args),
            "readInto" => self.builtin_read_into(args),
            "readDialogue" => self.builtin_read_dialogue(args),
            "assert" => {
                let cond = args.first().map(|v| v.truthy()).unwrap_or(false);
                if !cond {
                    let msg = args.get(1).map(|v| v.display()).unwrap_or_else(|| "assertion failed".into());
                    return Err(RuntimeError {
                        msg: format!("assert failed: {msg}"),
                        line: self.cur_line,
                    });
                }
                Ok(Value::Nothing)
            }
            "typeof" => Ok(args.first().map(|v| Value::str(v.type_repr().name())).unwrap_or(Value::str("nothing"))),
            "len" => Ok(match args.first() {
                Some(Value::Str(s)) => Value::Int(s.lock().unwrap().chars().count() as i128),
                Some(Value::List(l)) => Value::Int(l.lock().unwrap().len() as i128),
                Some(Value::Map(m)) => Value::Int(m.lock().unwrap().len() as i128),
                Some(Value::Tuple(t)) => Value::Int(t.lock().unwrap().len() as i128),
                Some(v) => Value::Int(v.display().len() as i128),
                None => Value::Int(0),
            }),
            "str" => Ok(Value::str(args.first().map(|v| v.display()).unwrap_or_default())),
            "int" => Ok(match args.first() {
                Some(Value::Int(i)) => Value::Int(*i),
                Some(Value::Float(f)) => Value::Int(*f as i128),
                Some(Value::Str(s)) => Value::Int(s.lock().unwrap().parse().unwrap_or(0)),
                _ => Value::Int(0),
            }),
            "float" => Ok(match args.first() {
                Some(Value::Float(f)) => Value::Float(*f),
                Some(Value::Int(i)) => Value::Float(*i as f64),
                Some(Value::Str(s)) => Value::Float(s.lock().unwrap().parse().unwrap_or(0.0)),
                _ => Value::Float(0.0),
            }),
            "Err" => {
                let msg = args.first().map(|v| v.display()).unwrap_or_else(|| "error".into());
                Ok(Value::Err(Arc::new(ErrVal {
                    msg,
                    origin: "Err".into(),
                })))
            }
            "weak" => {
                if let Some(v) = args.first() {
                    self.escape_value(v);
                }
                Ok(match args.first() {
                    Some(v) => Value::Weak(Arc::new(Mutex::new(v.clone()))),
                    None => Value::Nothing,
                })
            }
            "channel" => {
                // (tx, rx) share one queue — send on tx, receive on rx.
                // Real blocking MPMC channel (§14): send blocks on a full
                // bounded queue, recv blocks until a value or close.
                let ch = Arc::new(ChannelShared::new(0));
                Ok(Value::Tuple(Arc::new(Mutex::new(vec![
                    Value::Channel(ch.clone()),
                    Value::Channel(ch),
                ]))))
            }
            "ui.window" | "window" => {
                for a in &args {
                    self.escape_value(a);
                }
                Ok(self.prog.ui.lock().unwrap().window(&args))
            }
            "route" => {
                let path = args.first().map(|v| v.display()).unwrap_or_default();
                let page = args.iter().find_map(|v| match v {
                    Value::Func(f) => Some(f.clone()),
                    _ => None,
                });
                self.prog.ui.lock().unwrap().routes.push(crate::uiruntime::Route {
                    pattern: path,
                    page,
                });
                Ok(Value::Nothing)
            }
            "onClick" => {
                // onClick(btn) { ... } — args: [element, lambda]
                for a in &args {
                    self.escape_value(a);
                }
                let handler = args.iter().rev().find_map(|v| match v {
                    Value::Func(f) => Some(f.clone()),
                    _ => None,
                });
                let el = args.iter().find_map(|v| match v {
                    Value::Ui(e) => Some(e.clone()),
                    _ => None,
                });
                // write access on the element covers the handler store
                let _p = match args.iter().find(|v| matches!(v, Value::Ui(_))) {
                    Some(ev) => Some(
                        self.enter_val(ev, true, "onClick handler store")
                            .map_err(|m| RuntimeError { msg: m, line: self.cur_line })?,
                    ),
                    None => None,
                };
                if let (Some(e), Some(f)) = (el, handler) {
                    let mut b = e.lock().unwrap();
                    b.handlers.push(("click".to_string(), f));
                    b.dirty = true;
                }
                self.prog.ui.lock().unwrap().last_handler = args.iter().find_map(|v| match v {
                    Value::Func(f) => Some(f.clone()),
                    _ => None,
                });
                Ok(Value::Nothing)
            }
            "spawn" => Ok(Value::Nothing),
            _ => Err(RuntimeError {
                msg: format!("unknown builtin `{name}`"),
                line: self.cur_line,
            }),
        }
    }

    fn builtin_read(&mut self, _args: Vec<Value>) -> Result<Value, RuntimeError> {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map_err(|e| RuntimeError {
            msg: format!("read failed: {e}"),
            line: self.cur_line,
        })?;
        let tok = line.split_whitespace().next().unwrap_or("");
        // declare-and-read: `read(int age)` → returns value; caller declares.
        // Typed arg info isn't passed here; infer: try int, then float, then str.
        if let Ok(i) = tok.parse::<i128>() {
            Ok(Value::Int(i))
        } else if let Ok(f) = tok.parse::<f64>() {
            Ok(Value::Float(f))
        } else {
            Ok(Value::str(tok))
        }
    }

    fn builtin_read_into(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // readInto(x) — x is an lvalue; we can't assign through a Value.
        // v1: returns the read value; assignment form handled by parser later.
        let _ = args;
        self.builtin_read(vec![])
    }

    fn builtin_read_dialogue(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let prompt = args.first().map(|v| v.display()).unwrap_or_default();
        print!(">: {prompt}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        use std::io::BufRead;
        let stdin = std::io::stdin();
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map_err(|e| RuntimeError {
            msg: format!("read failed: {e}"),
            line: self.cur_line,
        })?;
        Ok(Value::str(line.trim_end().to_string()))
    }

}

fn primitive_type(name: &str) -> Option<TypeRepr> {
    Some(match name {
        "int" | "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" => TypeRepr::Int,
        "float" | "f32" | "f64" => TypeRepr::Float,
        "str" => TypeRepr::Str,
        "bool" => TypeRepr::Bool,
        "bytes" => TypeRepr::Bytes,
        _ => return None,
    })
}

/// A runtime value used where a type is expected → TypeRepr.
/// `Value::Type` is itself; a tuple of type values is a union (§7.6);
/// `(T, null)` is the optional pad (§7.5).
fn value_to_type(v: &Value) -> Option<TypeRepr> {
    match v {
        Value::Type(t) => Some(t.clone()),
        Value::Tuple(t) => {
            let t = t.lock().unwrap();
            if t.len() == 2 {
                if let (Some(inner), Value::Nothing) = (value_to_type(&t[0]), &t[1]) {
                    return Some(TypeRepr::Optional(Box::new(inner)));
                }
            }
            let mut vs = Vec::new();
            for e in t.iter() {
                vs.push(value_to_type(e)?);
            }
            Some(TypeRepr::Union(vs))
        }
        _ => None,
    }
}

fn default_for(t: &TypeRepr) -> Value {
    match t {
        TypeRepr::Int => Value::Int(0),
        TypeRepr::Float => Value::Float(0.0),
        TypeRepr::Str => Value::str(""),
        TypeRepr::Bool => Value::Bool(false),
        TypeRepr::List(_) => Value::List(Arc::new(Mutex::new(vec![]))),
        TypeRepr::Map(_, _) => Value::Map(Arc::new(Mutex::new(vec![]))),
        TypeRepr::Record { name, fields } => Value::Record {
            ty: name.clone(),
            fields: Arc::new(Mutex::new(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), default_for(t)))
                    .collect(),
            )),
        },
        _ => Value::Nothing,
    }
}

/// Bit width + signedness of a fixed-width integer type name, or None for
/// `int` (§7.1).
fn cast_width(ty: &TypeExpr) -> Option<(&'static str, u32, bool)> {
    let TypeExpr::Named(n) = ty else {
        return None;
    };
    match n.as_str() {
        "i8" => Some(("i8", 8, true)),
        "i16" => Some(("i16", 16, true)),
        "i32" => Some(("i32", 32, true)),
        "i64" => Some(("i64", 64, true)),
        "u8" => Some(("u8", 8, false)),
        "u16" => Some(("u16", 16, false)),
        "u32" => Some(("u32", 32, false)),
        "u64" => Some(("u64", 64, false)),
        _ => None,
    }
}

/// Truncate to a fixed width, two's complement — `as u8` on 300 is 44,
/// `as i8` on 200 is -56, `as u8` on -5 is 251. None width = no change.
fn trunc_int(i: i128, w: Option<(&'static str, u32, bool)>) -> i128 {
    match w {
        None => i,
        Some(("i8", ..)) => (i as i8) as i128,
        Some(("i16", ..)) => (i as i16) as i128,
        Some(("i32", ..)) => (i as i32) as i128,
        Some(("i64", ..)) => (i as i64) as i128,
        Some(("u8", ..)) => (i as u8) as i128,
        Some(("u16", ..)) => (i as u16) as i128,
        Some(("u32", ..)) => (i as u32) as i128,
        Some(("u64", ..)) => (i as u64) as i128,
        Some(_) => i,
    }
}

fn fuse_name(parts: &[String]) -> String {    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            out.push_str(&p.to_lowercase());
        } else {
            let mut c = p.chars();
            if let Some(f) = c.next() {
                out.push(f.to_ascii_uppercase());
                out.push_str(c.as_str());
            }
        }
    }
    out
}

fn run_shell(cmd: &str) -> Result<String, String> {
    // call flattening (§20): simple commands exec directly, no /bin/sh
    const META: &[char] = &[
        '|', '&', ';', '<', '>', '`', '$', '(', ')', '{', '}', '*', '?', '[', ']', '\'', '"',
        '\\', '~', '#', '!', '^',
    ];
    let mut argv = cmd.split_whitespace();
    let direct = !cmd.contains(META);
    let prog = if direct { argv.next() } else { None };
    let mut command = match prog {
        Some(p) => {
            let mut c = std::process::Command::new(p);
            c.args(argv);
            c
        }
        None => {
            let mut c = std::process::Command::new("/bin/sh");
            c.arg("-c").arg(cmd);
            c
        }
    };
    let out = command
        .output()
        .map_err(|e| format!("shellOut failed: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        Err(format!(
            "shellOut `{cmd}` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn now_iso() -> String {
    // civil-from-days (Howard Hinnant's algorithm)
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// ---------- minimal JSON (for json.parse/stringify) ----------

fn json_parse_min(s: &str) -> Value {
    let mut p = JsonP { b: s.as_bytes(), i: 0 };
    p.ws();
    p.value()
}

struct JsonP<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> JsonP<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn peek(&self) -> u8 {
        *self.b.get(self.i).unwrap_or(&0)
    }
    fn value(&mut self) -> Value {
        self.ws();
        match self.peek() {
            b'"' => self.string(),
            b'[' => self.array(),
            b'{' => self.object(),
            b't' => { self.i += 4; Value::Bool(true) }
            b'f' => { self.i += 5; Value::Bool(false) }
            b'n' => { self.i += 4; Value::Nothing }
            _ => self.number(),
        }
    }
    fn string(&mut self) -> Value {
        self.i += 1; // opening quote
        let mut out = String::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            self.i += 1;
            if c == b'"' {
                break;
            }
            if c == b'\\' && self.i < self.b.len() {
                let e = self.b[self.i];
                self.i += 1;
                match e {
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    b'u' => { self.i = (self.i + 4).min(self.b.len()); out.push('?'); }
                    _ => out.push(e as char),
                }
            } else {
                out.push(c as char);
            }
        }
        Value::str(out)
    }
    fn number(&mut self) -> Value {
        let start = self.i;
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
        {
            self.i += 1;
        }
        let t = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("0");
        if t.contains(['.', 'e', 'E']) {
            Value::Float(t.parse().unwrap_or(0.0))
        } else {
            Value::Int(t.parse().unwrap_or(0))
        }
    }
    fn array(&mut self) -> Value {
        self.i += 1;
        let mut items = Vec::new();
        loop {
            self.ws();
            if self.peek() == b']' { self.i += 1; break; }
            items.push(self.value());
            self.ws();
            if self.peek() == b',' { self.i += 1; continue; }
            if self.peek() == b']' { self.i += 1; break; }
            break;
        }
        Value::List(Arc::new(Mutex::new(items)))
    }
    fn object(&mut self) -> Value {
        self.i += 1;
        let mut m = Vec::new();
        loop {
            self.ws();
            if self.peek() == b'}' { self.i += 1; break; }
            let k = self.string();
            self.ws();
            if self.peek() == b':' { self.i += 1; }
            let v = self.value();
            m.push((k, v));
            self.ws();
            if self.peek() == b',' { self.i += 1; continue; }
            if self.peek() == b'}' { self.i += 1; break; }
            break;
        }
        Value::Map(Arc::new(Mutex::new(m)))
    }
}

fn json_stringify(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("\"{}\"", s.lock().unwrap().replace('"', "\\\"")),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Nothing => "null".into(),
        Value::List(l) => {
            let inner = l
                .lock().unwrap()
                .iter()
                .map(json_stringify)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        Value::Map(m) => {
            let inner = m
                .lock().unwrap()
                .iter()
                .map(|(k, v)| format!("{}:{}", json_stringify(k), json_stringify(v)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
        Value::Record { fields, .. } => {
            let inner = fields
                .lock().unwrap()
                .iter()
                .map(|(k, v)| format!("\"{k}\":{}", json_stringify(v)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
        _ => format!("\"{}\"", v.display()),
    }
}

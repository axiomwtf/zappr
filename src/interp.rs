//! Tree-walking evaluator — the semantics engine behind `zap run`
//! (in-memory build + execute, SPEC §18) and shared by `zap check`.
//! Native codegen (SPEC §4) consumes the same AST.

use crate::parser::{
    BinOp, Expr, ForPat, Item, MatchArm, Param, Program, Stmt, StrPart, TypeExpr, UnOp,
};
use crate::value::{values_equal, BuiltinFn, ChannelState, ErrVal, FuncVal, Num, PoisonKind, TypeRepr, Value};
use crate::value::Env as Scope;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

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

pub struct Vm {
    /// all function bodies across all loaded modules, flat
    fns: Vec<FnDef>,
    /// name → fn indices (a name may exist in several modules)
    fn_map: HashMap<String, Vec<usize>>,
    /// module namespace → module
    modules: HashMap<String, Module>,
    /// module of the function currently executing (for bare-name resolution)
    cur_module: String,
    /// global scope (module top-level decls live in their module scope)
    scopes: Scope,
    /// entry module name (main.znz → "main")
    pub entry: String,
    /// zapYaml vals from Zappr.yaml
    pub zap_yaml: Value,
    /// ui element registry for the (stub) ui runtime
    pub ui: crate::uiruntime::UiRuntime,
    /// line number tracking for errors
    cur_line: usize,
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
pub struct Module {
    pub name: String,
    /// top-level statements other than fns (rare — spec keeps logic in fns)
    pub fns: Vec<usize>,
}

impl Vm {
    pub fn new() -> Self {
        Vm {
            fns: Vec::new(),
            fn_map: HashMap::new(),
            modules: HashMap::new(),
            cur_module: "main".into(),
            scopes: vec![Rc::new(RefCell::new(HashMap::new()))],
            entry: "main".into(),
            zap_yaml: Value::Map(Rc::new(RefCell::new(Vec::new()))),
            ui: crate::uiruntime::UiRuntime::new(),
            cur_line: 0,
        }
    }

    // ---------- scope helpers ----------

    fn push_scope(&mut self) {
        self.scopes.push(Rc::new(RefCell::new(HashMap::new())));
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare(&mut self, name: &str, val: Value) -> Result<(), RuntimeError> {
        // no-shadowing (§6.3): name must not exist in any enclosing scope
        for s in self.scopes.iter().rev() {
            if s.borrow().contains_key(name) {
                return Err(RuntimeError {
                    msg: format!("redeclaration of `{name}` — names are declared exactly once (SPEC §6.3)"),
                    line: self.cur_line,
                });
            }
        }
        self.scopes
            .last()
            .unwrap()
            .borrow_mut()
            .insert(name.to_string(), val);
        Ok(())
    }

    fn set(&mut self, name: &str, val: Value) -> Result<(), RuntimeError> {
        for s in self.scopes.iter().rev() {
            if s.borrow().contains_key(name) {
                s.borrow_mut().insert(name.to_string(), val);
                return Ok(());
            }
        }
        Err(RuntimeError {
            msg: format!("reassignment of undeclared `{name}` — use `!{name} = ...` to declare"),
            line: self.cur_line,
        })
    }

    fn get(&self, name: &str) -> Option<Value> {
        for s in self.scopes.iter().rev() {
            if let Some(v) = s.borrow().get(name) {
                return Some(v.clone());
            }
        }
        None
    }

    // ---------- loading ----------

    /// Load a parsed module: register its functions under its namespace.
    pub fn load_module(&mut self, module_name: &str, prog: &Program) -> Result<(), RuntimeError> {
        let module_scope = vec![Rc::new(RefCell::new(HashMap::new()))];
        let mut fn_ids = Vec::new();
        for item in &prog.items {
            match item {
                Item::Fn {
                    name,
                    params,
                    body,
                    line,
                } => {
                    let _ = line;
                    let id = self.fns.len();
                    self.fns.push(FnDef {
                        name: name.clone(),
                        module: module_name.to_string(),
                        params: params
                            .iter()
                            .map(|p| (p.name.clone(), p.ty.clone()))
                            .collect(),
                        body: body.clone(),
                        env: module_scope.clone(),
                    });
                    self.fn_map.entry(name.clone()).or_default().push(id);
                    // qualified name resolves from anywhere
                    self.fn_map
                        .entry(format!("{module_name}.{name}"))
                        .or_default()
                        .push(id);
                    fn_ids.push(id);
                }
                Item::Use(_) => {}
            }
        }
        self.modules.insert(
            module_name.to_string(),
            Module {
                name: module_name.to_string(),
                fns: fn_ids,
            },
        );
        Ok(())
    }

    /// Resolve `use.foo.bar` path → module name (fused camelCase).
    pub fn resolve_use(&self, path: &[String]) -> Option<String> {
        // path segments fuse: http/auth/session → httpAuthSession
        let fused = fuse_name(path);
        if self.modules.contains_key(&fused) {
            return Some(fused);
        }
        // maybe path already is the fused name in one segment
        if path.len() == 1 && self.modules.contains_key(&path[0]) {
            return Some(path[0].clone());
        }
        None
    }

    /// `module.fn` → callable value
    pub fn module_fn(&self, module: &str, fname: &str) -> Option<Value> {
        let key = format!("{module}.{fname}");
        let ids = self.fn_map.get(&key)?;
        let id = *ids.first()?;
        let f = &self.fns[id];
        Some(Value::Func(Rc::new(FuncVal {
            name: format!("{module}.{fname}"),
            params: f.params.iter().map(|(n, _)| n.clone()).collect(),
            param_types: f
                .params
                .iter()
                .map(|(_, t)| t.as_ref().map(|_| TypeRepr::Any))
                .collect(),
            body_id: id,
            env: f.env.clone(),
            builtin: None,
        })))
    }

    // ---------- execution ----------

    /// Call a function by qualified or bare name (used by `zap test`, CLI cmds).
    pub fn call_named(&mut self, name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let ids = self
            .fn_map
            .get(name)
            .cloned()
            .ok_or_else(|| RuntimeError {
                msg: format!("no function `{name}`"),
                line: 0,
            })?;
        let id = ids
            .iter()
            .copied()
            .find(|id| self.fns[*id].module == self.cur_module)
            .or_else(|| ids.first().copied())
            .unwrap();
        self.call_fn_id(id, args)
    }

    pub fn run_main(&mut self) -> Result<Value, RuntimeError> {
        // find main() — prefer entry module's main, else any main
        let key = format!("{}.main", self.entry);
        let id = self
            .fn_map
            .get(&key)
            .and_then(|v| v.first().copied())
            .or_else(|| {
                self.fn_map
                    .get("main")
                    .and_then(|v| v.first().copied())
            })
            .ok_or_else(|| RuntimeError {
                msg: format!("no `main()` found (entry module `{}`)", self.entry),
                line: 0,
            })?;
        self.call_fn_id(id, Vec::new())
    }

    fn call_fn_id(&mut self, id: usize, args: Vec<Value>) -> Result<Value, RuntimeError> {
        // clone what we need; fns vec isn't mutated during call
        let (name, params, body, env, module) = {
            let f = &self.fns[id];
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
            fields: Rc::new(RefCell::new(vec![
                ("count".into(), Value::Int(args.len() as i128)),
                (
                    "rest".into(),
                    Value::List(Rc::new(RefCell::new(rest))),
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
                    Value::Tuple(t) => t.borrow().clone(),
                    Value::List(l) => l.borrow().clone(),
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
                let ov = self.get(obj).ok_or_else(|| RuntimeError {
                    msg: format!("unknown object `{obj}`"),
                    line: *line,
                })?;
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
                let mut idx = 0usize;
                loop {
                    let next = match &it {
                        Value::List(l) => {
                            let l = l.borrow();
                            if idx >= l.len() {
                                None
                            } else {
                                Some(l[idx].clone())
                            }
                        }
                        Value::Tuple(t) => {
                            let t = t.borrow();
                            if idx >= t.len() {
                                None
                            } else {
                                Some(t[idx].clone())
                            }
                        }
                        Value::Map(m) => {
                            let m = m.borrow();
                            if idx >= m.len() {
                                None
                            } else {
                                let (k, v) = m[idx].clone();
                                Some(Value::Tuple(Rc::new(RefCell::new(vec![k, v]))))
                            }
                        }
                        Value::Channel(ch) => {
                            let mut ch = ch.borrow_mut();
                            ch.queue.pop_front()
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
                                Value::Tuple(t) => t.borrow().clone(),
                                Value::List(l) => l.borrow().clone(),
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
                            let inner = match &v {
                                Value::Variant { inner, .. } => (**inner).clone(),
                                _ => v.clone(),
                            };
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
                // v1: structured concurrency semantics — children run to
                // completion at the join, decls publish upward. Parallel
                // execution lands with native codegen; the join guarantee
                // (no orphans) holds here by construction.
                self.push_scope();
                let r = self.exec_block(body);
                // publish decls upward
                let inner = self.scopes.pop().unwrap();
                for (k, v) in inner.borrow().iter() {
                    self.scopes
                        .last()
                        .unwrap()
                        .borrow_mut()
                        .insert(k.clone(), v.clone());
                }
                r.map(|f| match f {
                    Flow::Return(v) => Flow::Return(v),
                    _ => Flow::Normal,
                })
            }
            Stmt::Spawn(e) => {
                // v1: run eagerly — channel semantics still work because the
                // worker fills the queue before the consumer drains it.
                if let Expr::Call { .. } = e {
                    let _ = self.eval_call_spawn(e)?;
                } else {
                    self.eval(e)?;
                }
                Ok(Flow::Normal)
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
                Value::Variant { name: n, .. } => n == name,
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
                Value::Variant { name: n, .. } => n == name,
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
            TypeExpr::Any => TypeRepr::Any,
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
        match obj {
            Value::Ui(el) => {
                // mutate an element's property and it updates (§16.1)
                el.borrow_mut().set(field, val);
                Ok(())
            }
            Value::Record { fields, ty } => {
                // ui element records stored as Record? (legacy) — allow growth
                let mut f = fields.borrow_mut();
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
                let mut m = m.borrow_mut();
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
            Expr::Int(i) => Ok(Value::Int(*i as i128)),
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
                Ok(Value::Tuple(Rc::new(RefCell::new(vals))))
            }
            Expr::List(items) => {
                let mut vals = Vec::new();
                for i in items {
                    vals.push(self.eval(i)?);
                }
                Ok(Value::List(Rc::new(RefCell::new(vals))))
            }
            Expr::Map(entries) => {
                let mut vals = Vec::new();
                for (k, v) in entries {
                    vals.push((self.eval(k)?, self.eval(v)?));
                }
                Ok(Value::Map(Rc::new(RefCell::new(vals))))
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
                if v.satisfies(&tr) {
                    Ok(v)
                } else {
                    // numeric cast as u8 etc → truncate
                    if let TypeRepr::Int = tr {
                        return match v {
                            Value::Int(i) => Ok(Value::Int(i)),
                            Value::Float(f) => Ok(Value::Int(f as i128)),
                            _ => Ok(Value::Poison {
                                kind: PoisonKind::WrongReturnType,
                                origin: "as cast".into(),
                                expected: tr.name(),
                                actual: v.type_repr().name(),
                            }),
                        };
                    }
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
                    // register the lambda as a fn capturing current scopes
                    let id = self.fns.len();
                    self.fns.push(FnDef {
                        name: "<lambda>".into(),
                        module: self.cur_module.clone(),
                        params: vec![("event".into(), None)],
                        body: body.clone(),
                        env: self.scopes.clone(),
                    });
                    vals.push(Value::Func(Rc::new(FuncVal {
                        name: "<lambda>".into(),
                        params: vec!["event".into()],
                        param_types: vec![None],
                        body_id: id,
                        env: self.scopes.clone(),
                        builtin: None,
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
        if let Some(ids) = self.fn_map.get(name) {
            let id = ids
                .iter()
                .copied()
                .find(|id| self.fns[*id].module == self.cur_module)
                .or_else(|| ids.first().copied())
                .unwrap();
            let f = &self.fns[id];
            return Ok(Value::Func(Rc::new(FuncVal {
                name: name.to_string(),
                params: f.params.iter().map(|(n, _)| n.clone()).collect(),
                param_types: f
                    .params
                    .iter()
                    .map(|(_, t)| t.as_ref().map(|_| TypeRepr::Any))
                    .collect(),
                body_id: id,
                env: f.env.clone(),
                builtin: None,
            })));
        }
        // builtins / ambient namespaces
        match name {
            "print" | "read" | "readInto" | "readDialogue" | "assert" | "typeof" | "len" | "str" | "int" | "float" | "env" | "clock" | "nav" | "ui" | "route" | "onClick" | "weak" | "channel" | "spawn" | "parallel" | "thisShell" | "math" | "json" | "exit" => {
                return Ok(Value::Func(Rc::new(FuncVal {
                    name: name.to_string(),
                    params: vec![],
                    param_types: vec![],
                    body_id: usize::MAX,
                    env: vec![],
                    builtin: None, // resolved by name at call time
                })));
            }
            _ => {}
        }
        if name == "zapYaml" {
            return Ok(self.zap_yaml.clone());
        }
        self.get(name).ok_or_else(|| RuntimeError {
            msg: format!("unknown name `{name}`"),
            line: self.cur_line,
        })
    }

    fn eval_str(&mut self, parts: &[StrPart]) -> Result<Value, RuntimeError> {
        let mut out = String::new();
        for p in parts {
            match p {
                StrPart::Lit(s) => out.push_str(s),
                StrPart::Interp(e) => {
                    let v = self.eval(e)?;
                    out.push_str(&v.display());
                }
            }
        }
        Ok(Value::str(out))
    }

    fn field_get(&mut self, obj: &Value, name: &str) -> Result<Value, RuntimeError> {
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
                    let f = fields.borrow();
                    return Ok(f.get(idx).map(|(_, v)| v.clone()).unwrap_or(Value::Nothing));
                }
                let f = fields.borrow();
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
                    let t = t.borrow();
                    return Ok(t.get(idx).cloned().unwrap_or(Value::Nothing));
                }
                Err(RuntimeError {
                    msg: format!("no named field `{name}` on tuple"),
                    line: self.cur_line,
                })
            }
            Value::List(l) => match name {
                "len" => Ok(Value::Func(Rc::new(FuncVal {
                    name: "List.len".into(),
                    params: vec![],
                    param_types: vec![],
                    body_id: usize::MAX,
                    env: vec![],
                    builtin: Some(BuiltinFn::Len),
                }))),
                "push" => Ok(Value::Func(Rc::new(FuncVal {
                    name: "List.push".into(),
                    params: vec!["v".into()],
                    param_types: vec![],
                    body_id: usize::MAX,
                    env: vec![],
                    builtin: Some(BuiltinFn::Len), // marker; handled specially
                }))),
                _ => Err(RuntimeError {
                    msg: format!("no field/method `{name}` on list"),
                    line: self.cur_line,
                }),
            },
            Value::Map(m) => {
                // zapYaml.x and map.key read as field access (§19)
                let key = Value::str(name);
                if let Some((_, v)) = m.borrow().iter().find(|(k, _)| values_equal(k, &key)) {
                    return Ok(v.clone());
                }
                match name {
                    "len" => Ok(Value::Func(Rc::new(FuncVal {
                        name: "Map.len".into(),
                        params: vec![],
                        param_types: vec![],
                        body_id: usize::MAX,
                        env: vec![],
                        builtin: Some(BuiltinFn::Len),
                    }))),
                    _ => Err(RuntimeError {
                        msg: format!("no field/method `{name}` on map"),
                        line: self.cur_line,
                    }),
                }
            }
            Value::Str(s) => match name {
                "len" => Ok(Value::Int(s.borrow().chars().count() as i128)),
                _ => Err(RuntimeError {
                    msg: format!("no field/method `{name}` on str"),
                    line: self.cur_line,
                }),
            },
            Value::Type(t) => Ok(Value::Func(Rc::new(FuncVal {
                name: format!("type.{}", t.name()),
                params: vec![],
                param_types: vec![],
                body_id: usize::MAX,
                env: vec![],
                builtin: None,
            }))),
            Value::Ui(el) => {
                if name == "add" {
                    // `win.add` — element factory; `.text(...)` etc. create children
                    return Ok(Value::Record {
                        ty: "ui.adder".into(),
                        fields: Rc::new(RefCell::new(vec![(
                            "__parent".to_string(),
                            obj.clone(),
                        )])),
                    });
                }
                match el.borrow().get(name) {
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
                    return Ok(Value::Func(Rc::new(FuncVal {
                        name: "fnType".into(),
                        params: vec!["T".into()],
                        param_types: vec![],
                        body_id: usize::MAX,
                        env: vec![],
                        builtin: Some(BuiltinFn::PrintNl), // marker — real dispatch in eval_call_field
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
            Value::Ui(_) => Err(RuntimeError {
                msg: "no such field".into(),
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
                            let (a, b) = (a.borrow().clone(), b.borrow().clone());
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
            let mut s = a.borrow().clone();
            s.push_str(&b.borrow());
            return Ok(Value::str(s));
        }
        if let (Value::Str(a), b) = (l, r) {
            if !matches!(b, Value::Str(_)) {
                let mut s = a.borrow().clone();
                s.push_str(&b.display());
                return Ok(Value::str(s));
            }
        }
        if let (a, Value::Str(b)) = (l, r) {
            if !matches!(a, Value::Str(_)) {
                let mut s = a.display();
                s.push_str(&b.borrow());
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
        for a in args {
            match a {
                Expr::NamedArg { name, value } => {
                    named.push((name.clone(), self.eval(value)?));
                }
                Expr::DeclArg { .. } => {
                    return self.err("declare-and-read arg outside read()".into());
                }
                _ => vals.push(self.eval(a)?),
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

    fn eval_call_spawn(&mut self, e: &Expr) -> Result<Value, RuntimeError> {
        if let Expr::Call { callee, args } = e {
            let mut vals = Vec::new();
            for a in args {
                vals.push(self.eval(a)?);
            }
            return self.eval_callee(callee, vals, vec![]);
        }
        self.err("spawn expects a call".into())
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
                // type constructors in expression position: List(int), Map(k,v)
                if name == "List" || name == "Map" {
                    let tys: Vec<TypeRepr> = args.iter().filter_map(value_to_type).collect();
                    let t = if name == "List" {
                        TypeRepr::List(Box::new(tys.first().cloned().unwrap_or(TypeRepr::Any)))
                    } else {
                        TypeRepr::Map(
                            Box::new(tys.first().cloned().unwrap_or(TypeRepr::Any)),
                            Box::new(tys.get(1).cloned().unwrap_or(TypeRepr::Any)),
                        )
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
                        return Ok(crate::uiruntime::UiRuntime::add_element(
                            &parent, name, &args,
                        ));
                    }
                }
                // module namespace: `mod.fn(...)` — resolve before evaluating
                // obj, since a pure namespace isn't a value.
                if let Expr::Ident(modname) = obj.as_ref() {
                    if self.modules.contains_key(modname) {
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
        let ids = match self.fn_map.get(name) {
            Some(ids) => ids.clone(),
            None => return Ok(None),
        };
        // prefer a function from the caller's own module
        let id = ids
            .iter()
            .copied()
            .find(|id| self.fns[*id].module == self.cur_module)
            .or_else(|| ids.first().copied())
            .unwrap();
        Ok(Some(self.call_fn_id(id, args)?))
    }

    fn call_func_value(&mut self, f: &FuncVal, args: Vec<Value>) -> Result<Value, RuntimeError> {
        if f.body_id == usize::MAX {
            // builtin marker — dispatch by name
            return self.builtin_dispatch(&f.name, args);
        }
        let saved_module = self.cur_module.clone();
        if f.body_id < self.fns.len() {
            self.cur_module = self.fns[f.body_id].module.clone();
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
                fields: Rc::new(RefCell::new(vec![
                    ("count".into(), Value::Int(args.len() as i128)),
                    ("rest".into(), Value::List(Rc::new(RefCell::new(rest)))),
                ])),
            },
        );
        let fdef = &self.fns[f.body_id];
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
                    fields: Rc::new(RefCell::new(out)),
                })
            }
            TypeRepr::List(_) => Ok(Value::List(Rc::new(RefCell::new(args)))),
            TypeRepr::Map(_, _) => {
                // Map(k,v){} literal style or pairs
                let mut m = Vec::new();
                for a in args {
                    if let Value::Tuple(t) = a {
                        let t = t.borrow();
                        if t.len() == 2 {
                            m.push((t[0].clone(), t[1].clone()));
                        }
                    }
                }
                Ok(Value::Map(Rc::new(RefCell::new(m))))
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
        match (obj, name) {
            (Value::List(l), "push") => {
                if let Some(v) = args.first() {
                    l.borrow_mut().push(v.clone());
                }
                Ok(Value::Nothing)
            }
            (Value::List(l), "len") => Ok(Value::Int(l.borrow().len() as i128)),
            (Value::List(l), "pop") => Ok(l.borrow_mut().pop().unwrap_or(Value::Nothing)),
            (Value::List(l), "first") => Ok(l.borrow().first().cloned().unwrap_or(Value::Nothing)),
            (Value::List(l), "last") => Ok(l.borrow().last().cloned().unwrap_or(Value::Nothing)),
            (Value::List(l), "at") => {
                let i = args.first().map(|v| crate::value::as_num(v).map(|n| n.as_f64() as i64).unwrap_or(0)).unwrap_or(0);
                Ok(l.borrow().get(i.max(0) as usize).cloned().unwrap_or(Value::Nothing))
            }
            (Value::List(l), "contains") => {
                let needle = args.first().cloned().unwrap_or(Value::Nothing);
                Ok(Value::Bool(l.borrow().iter().any(|v| values_equal(v, &needle))))
            }
            (Value::List(l), "join") => {
                let sep = args.first().map(|v| v.display()).unwrap_or_default();
                let parts: Vec<String> = l.borrow().iter().map(|v| v.display()).collect();
                Ok(Value::str(parts.join(&sep)))
            }
            (Value::List(l), "reverse") => {
                l.borrow_mut().reverse();
                Ok(Value::Nothing)
            }
            (Value::List(l), "map") | (Value::Tuple(l), "map") => {
                let f = match args.first() {
                    Some(Value::Func(f)) => f.clone(),
                    _ => return self.err("map expects a fn value".into()),
                };
                let src: Vec<Value> = l.borrow().clone();
                let mut out = Vec::with_capacity(src.len());
                for item in src {
                    out.push(self.call_func_value(&f, vec![item])?);
                }
                Ok(Value::List(Rc::new(RefCell::new(out))))
            }
            (Value::List(l), "filter") | (Value::Tuple(l), "filter") => {
                let f = match args.first() {
                    Some(Value::Func(f)) => f.clone(),
                    _ => return self.err("filter expects a fn value".into()),
                };
                let src: Vec<Value> = l.borrow().clone();
                let mut out = Vec::new();
                for item in src {
                    let keep = self.call_func_value(&f, vec![item.clone()])?;
                    if keep.truthy() {
                        out.push(item);
                    }
                }
                Ok(Value::List(Rc::new(RefCell::new(out))))
            }
            (Value::Map(m), "len") => Ok(Value::Int(m.borrow().len() as i128)),
            (Value::Map(m), "get") => {
                let k = args.first().cloned().unwrap_or(Value::Nothing);
                Ok(m.borrow().iter().find(|(mk, _)| values_equal(mk, &k)).map(|(_, v)| v.clone()).unwrap_or(Value::Nothing))
            }
            (Value::Map(m), "set") => {
                if args.len() >= 2 {
                    let k = args[0].clone();
                    let v = args[1].clone();
                    let mut mm = m.borrow_mut();
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
                Ok(Value::Bool(m.borrow().iter().any(|(mk, _)| values_equal(mk, &k))))
            }
            (Value::Map(m), "keys") => {
                Ok(Value::List(Rc::new(RefCell::new(m.borrow().iter().map(|(k, _)| k.clone()).collect()))))
            }
            (Value::Map(m), "values") => {
                Ok(Value::List(Rc::new(RefCell::new(m.borrow().iter().map(|(_, v)| v.clone()).collect()))))
            }
            (Value::Str(s), "len") => Ok(Value::Int(s.borrow().chars().count() as i128)),
            (Value::Record { fields, .. }, "len") => Ok(Value::Int(fields.borrow().len() as i128)),
            (Value::Tuple(t), "len") => Ok(Value::Int(t.borrow().len() as i128)),
            (Value::Str(s), "upper") => Ok(Value::str(s.borrow().to_uppercase())),
            (Value::Str(s), "lower") => Ok(Value::str(s.borrow().to_lowercase())),
            (Value::Str(s), "trim") => Ok(Value::str(s.borrow().trim().to_string())),
            (Value::Str(s), "contains") => {
                let needle = args.first().map(|v| v.display()).unwrap_or_default();
                Ok(Value::Bool(s.borrow().contains(&needle)))
            }
            (Value::Str(s), "split") => {
                let sep = args.first().map(|v| v.display()).unwrap_or_else(|| ",".into());
                let parts: Vec<Value> = s.borrow().split(&sep).map(|p| Value::str(p)).collect();
                Ok(Value::List(Rc::new(RefCell::new(parts))))
            }
            (Value::Channel(ch), "send") => {
                if let Some(v) = args.first() {
                    ch.borrow_mut().queue.push_back(v.clone());
                }
                Ok(Value::Nothing)
            }
            (Value::Channel(ch), "close") => {
                ch.borrow_mut().closed = true;
                Ok(Value::Nothing)
            }
            (Value::Record { ty, fields }, kind) if ty == "ui.adder" => {
                // `win.add` factory value — `.text(...)`, `.button(...)` create children
                let parent = fields
                    .borrow()
                    .iter()
                    .find(|(n, _)| n == "__parent")
                    .map(|(_, v)| v.clone())
                    .unwrap_or(Value::Nothing);
                Ok(crate::uiruntime::UiRuntime::add_element(&parent, kind, &args))
            }
            (Value::Channel(ch), "recv") => {
                let mut ch = ch.borrow_mut();
                Ok(ch.queue.pop_front().unwrap_or(Value::Nothing))
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

    fn namespaced_builtin(
        &mut self,
        ns: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Option<Value>, RuntimeError> {
        let full = format!("{ns}.{name}");
        match full.as_str() {
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
            "ui.window" => Ok(Some(self.ui.window(&args))),
            "ui.run" => {
                // headless run: drive one dirty-check tick per window.
                // The native renderer takes over at codegen time; the DOM
                // bridge at wasm. Element semantics are shared (§16).
                for w in &self.ui.windows {
                    let mut el = w.borrow_mut();
                    if el.dirty {
                        el.dirty = false;
                        self.ui.frame += 1;
                    }
                }
                Ok(Some(Value::Nothing))
            }
            "nav.go" => {
                // headless run: record navigation
                let url = args.first().map(|v| v.display()).unwrap_or_default();
                self.ui.navigations.push(url);
                Ok(Some(Value::Nothing))
            }
            "thisShell.shellOut" => {
                let cmd = args.first().map(|v| v.display()).unwrap_or_default();
                match run_shell(&cmd) {
                    Ok(out) => Ok(Some(Value::str(out))),
                    Err(e) => Ok(Some(Value::Err(Rc::new(ErrVal {
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

    fn render_fmt(&mut self, fmt: &str, args: &[Value]) -> String {
        // {name} interpolation — positional/named by arg order; also {0},{1}
        let mut out = String::new();
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '{' {
                let mut name = String::new();
                for c2 in chars.by_ref() {
                    if c2 == '}' {
                        break;
                    }
                    name.push(c2);
                }
                // try positional index then named arg display order
                if let Ok(idx) = name.parse::<usize>() {
                    if let Some(v) = args.get(idx) {
                        out.push_str(&v.display());
                        continue;
                    }
                }
                // named: find arg whose... we don't have names here; use order of named args
                // fallback: search all args for one whose display matches? Just use next unused.
                out.push('{');
                out.push_str(&name);
                out.push('}');
            } else {
                out.push(c);
            }
        }
        out
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
                Some(Value::Str(s)) => Value::Int(s.borrow().chars().count() as i128),
                Some(Value::List(l)) => Value::Int(l.borrow().len() as i128),
                Some(Value::Map(m)) => Value::Int(m.borrow().len() as i128),
                Some(Value::Tuple(t)) => Value::Int(t.borrow().len() as i128),
                Some(v) => Value::Int(v.display().len() as i128),
                None => Value::Int(0),
            }),
            "str" => Ok(Value::str(args.first().map(|v| v.display()).unwrap_or_default())),
            "int" => Ok(match args.first() {
                Some(Value::Int(i)) => Value::Int(*i),
                Some(Value::Float(f)) => Value::Int(*f as i128),
                Some(Value::Str(s)) => Value::Int(s.borrow().parse().unwrap_or(0)),
                _ => Value::Int(0),
            }),
            "float" => Ok(match args.first() {
                Some(Value::Float(f)) => Value::Float(*f),
                Some(Value::Int(i)) => Value::Float(*i as f64),
                Some(Value::Str(s)) => Value::Float(s.borrow().parse().unwrap_or(0.0)),
                _ => Value::Float(0.0),
            }),
            "Err" => {
                let msg = args.first().map(|v| v.display()).unwrap_or_else(|| "error".into());
                Ok(Value::Err(Rc::new(ErrVal {
                    msg,
                    origin: "Err".into(),
                })))
            }
            "weak" => Ok(match args.first() {
                Some(v) => Value::Weak(Rc::new(RefCell::new(v.clone()))),
                None => Value::Nothing,
            }),
            "channel" => {
                // (tx, rx) share one queue — send on tx, receive on rx
                let st = Rc::new(RefCell::new(ChannelState {
                    queue: std::collections::VecDeque::new(),
                    closed: false,
                    cap: 0,
                }));
                Ok(Value::Tuple(Rc::new(RefCell::new(vec![
                    Value::Channel(st.clone()),
                    Value::Channel(st),
                ]))))
            }
            "ui.window" | "window" => Ok(self.ui.window(&args)),
            "route" => {
                let path = args.first().map(|v| v.display()).unwrap_or_default();
                self.ui.routes.push(path);
                Ok(Value::Nothing)
            }
            "onClick" => {
                // onClick(btn) { ... } — args: [element, lambda]
                let handler = args.iter().rev().find_map(|v| match v {
                    Value::Func(f) => Some(f.clone()),
                    _ => None,
                });
                let el = args.iter().find_map(|v| match v {
                    Value::Ui(e) => Some(e.clone()),
                    _ => None,
                });
                if let (Some(e), Some(f)) = (el, handler) {
                    e.borrow_mut().handlers.push(("click".to_string(), f));
                }
                self.ui.last_handler = args.iter().find_map(|v| match v {
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

    fn builtin_read(&mut self, args: Vec<Value>) -> Result<Value, RuntimeError> {
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
            let t = t.borrow();
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

fn f_caller_value(v: &Value) -> Result<Value, RuntimeError> {
    Ok(v.clone())
}

fn default_for(t: &TypeRepr) -> Value {
    match t {
        TypeRepr::Int => Value::Int(0),
        TypeRepr::Float => Value::Float(0.0),
        TypeRepr::Str => Value::str(""),
        TypeRepr::Bool => Value::Bool(false),
        TypeRepr::List(_) => Value::List(Rc::new(RefCell::new(vec![]))),
        TypeRepr::Map(_, _) => Value::Map(Rc::new(RefCell::new(vec![]))),
        TypeRepr::Record { name, fields } => Value::Record {
            ty: name.clone(),
            fields: Rc::new(RefCell::new(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), default_for(t)))
                    .collect(),
            )),
        },
        _ => Value::Nothing,
    }
}

fn fuse_name(parts: &[String]) -> String {
    let mut out = String::new();
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
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
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
        Value::List(Rc::new(RefCell::new(items)))
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
        Value::Map(Rc::new(RefCell::new(m)))
    }
}

fn json_stringify(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("\"{}\"", s.borrow().replace('"', "\\\"")),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Nothing => "null".into(),
        Value::List(l) => {
            let inner = l
                .borrow()
                .iter()
                .map(json_stringify)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        Value::Map(m) => {
            let inner = m
                .borrow()
                .iter()
                .map(|(k, v)| format!("{}:{}", json_stringify(k), json_stringify(v)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
        Value::Record { fields, .. } => {
            let inner = fields
                .borrow()
                .iter()
                .map(|(k, v)| format!("\"{k}\":{}", json_stringify(v)))
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{inner}}}")
        }
        _ => format!("\"{}\"", v.display()),
    }
}

//! Checker: names resolve, arity is soft-checked, poison paths linted,
//! naming law + no-shadowing enforced (SPEC §4, §5, §9).

use crate::parser::{Expr, ForPat, Item, MatchArm, Program, Stmt, StrPart, TypeExpr};
use std::collections::{HashMap, HashSet};

/// Names that always resolve without declaration.
const AMBIENT: &[&str] = &[
    "print", "read", "readInto", "readDialogue", "assert", "typeof", "len", "str", "int",
    "float", "env", "clock", "nav", "ui", "route", "onClick", "weak", "channel", "spawn",
    "parallel", "zapYaml", "thisShell", "math", "json", "exit", "List", "Map", "null", "true",
    "false", "int", "float", "str", "bool", "bytes", "Err", "poison", "any",
];

pub struct CheckOutcome {
    /// hard errors — build/check fails
    pub errors: Vec<String>,
    /// warnings — reported, never silent (§19.1 spirit)
    pub warnings: Vec<String>,
}

/// Cross-module symbol table built once per project check/build.
#[derive(Debug, Default)]
pub struct GlobalSyms {
    /// every resolvable name: bare fn names, `module.fn`, module namespaces, type names
    pub names: HashSet<String>,
    /// fn name (bare or qualified) → param names, for soft-arity warnings
    pub params: HashMap<String, Vec<String>>,
}

impl GlobalSyms {
    pub fn from_programs<'a>(
        programs: impl Iterator<Item = (&'a str, &'a Program)>,
    ) -> GlobalSyms {
        let mut g = GlobalSyms::default();
        for (module, prog) in programs {
            g.names.insert(module.to_string());
            for item in &prog.items {
                match item {
                    Item::Fn { name, params, .. } => {
                        g.names.insert(name.clone());
                        g.names.insert(format!("{module}.{name}"));
                        let plist: Vec<String> =
                            params.iter().map(|p| p.name.clone()).collect();
                        g.params.insert(name.clone(), plist.clone());
                        g.params.insert(format!("{module}.{name}"), plist);
                    }
                    Item::Use(path) => {
                        if let Some(last) = path.last() {
                            g.names.insert(last.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        g
    }
}

pub fn check_program(prog: &Program, module: &str, globals: &GlobalSyms) -> CheckOutcome {
    let mut c = Checker {
        errors: Vec::new(),
        warnings: Vec::new(),
        globals,
        scope: vec![HashSet::new()],
    };
    for item in &prog.items {
        if let Item::Fn { name, params, body, .. } = item {
            c.scope = vec![HashSet::new()];
            for p in params {
                c.declare(&p.name);
            }
            c.stmts(body, module, name);
        }
    }
    CheckOutcome {
        errors: c.errors,
        warnings: c.warnings,
    }
}

struct Checker<'a> {
    errors: Vec<String>,
    warnings: Vec<String>,
    globals: &'a GlobalSyms,
    /// lexical scopes; innermost last
    scope: Vec<HashSet<String>>,
}

impl Checker<'_> {
    fn declare(&mut self, name: &str) -> bool {
        // no-shadowing law (§5.x): a decl in an inner scope that hides an
        // outer name is an error, not a warning
        for (i, s) in self.scope.iter().enumerate().rev() {
            if s.contains(name) {
                if i == self.scope.len() - 1 {
                    self.errors.push(format!(
                        "duplicate declaration `{name}` in same scope"
                    ));
                } else {
                    self.errors.push(format!(
                        "shadowing: `{name}` hides an outer binding — rename it (§5)"
                    ));
                }
                return false;
            }
        }
        self.scope.last_mut().unwrap().insert(name.to_string());
        true
    }

    fn lookup(&self, name: &str) -> bool {
        self.scope.iter().rev().any(|s| s.contains(name))
            || self.globals.names.contains(name)
            || AMBIENT.contains(&name)
    }

    fn stmts(&mut self, body: &[Stmt], module: &str, fname: &str) {
        for s in body {
            self.stmt(s, module, fname);
        }
    }

    fn stmt(&mut self, s: &Stmt, module: &str, fname: &str) {
        match s {
            Stmt::Decl { name, init, .. } => {
                self.expr(init, module, fname);
                self.declare(name);
            }
            Stmt::DeclTuple { names, init, .. } => {
                self.expr(init, module, fname);
                for n in names {
                    self.declare(n);
                }
            }
            Stmt::Reassign { target, value, .. } => {
                if !self.lookup(target) {
                    self.errors.push(format!("`{target}` is not declared — use `!{target} =` to declare"));
                }
                if let Some(v) = value {
                    self.expr(v, module, fname);
                }
            }
            Stmt::ReassignField { obj, value, .. } => {
                if !self.lookup(obj) {
                    self.errors.push(format!("`{obj}` is not declared"));
                }
                self.expr(value, module, fname);
            }
            Stmt::Expr(e) => self.expr(e, module, fname),
            Stmt::If {
                cond,
                bind,
                then_body,
                else_body,
                else_bind,
                ..
            } => {
                self.expr(cond, module, fname);
                self.scope.push(HashSet::new());
                if let Some(b) = bind {
                    self.declare(b);
                }
                self.stmts(then_body, module, fname);
                self.scope.pop();
                if let Some(eb) = else_body {
                    self.scope.push(HashSet::new());
                    if let Some(b) = else_bind {
                        self.declare(b);
                    }
                    self.stmts(eb, module, fname);
                    self.scope.pop();
                }
            }
            Stmt::ForIn { pat, iter, body, .. } => {
                self.expr(iter, module, fname);
                self.scope.push(HashSet::new());
                match pat {
                    ForPat::Single(n) => {
                        self.declare(n);
                    }
                    ForPat::Tuple(ns) => {
                        for n in ns {
                            self.declare(n);
                        }
                    }
                }
                self.stmts(body, module, fname);
                self.scope.pop();
            }
            Stmt::ForCond { cond, body, .. } => {
                self.expr(cond, module, fname);
                self.scope.push(HashSet::new());
                self.stmts(body, module, fname);
                self.scope.pop();
            }
            Stmt::ForInfinite { body, .. } => {
                self.scope.push(HashSet::new());
                self.stmts(body, module, fname);
                self.scope.pop();
            }
            Stmt::ForCStyle { init, cond, post, body, .. } => {
                self.scope.push(HashSet::new());
                for s in init {
                    self.stmt(s, module, fname);
                }
                self.expr(cond, module, fname);
                for s in post {
                    self.stmt(s, module, fname);
                }
                self.scope.push(HashSet::new());
                self.stmts(body, module, fname);
                self.scope.pop();
                self.scope.pop();
            }
            Stmt::Match { expr, arms, .. } => {
                self.expr(expr, module, fname);
                for arm in arms {
                    self.match_arm(arm, module, fname);
                }
            }
            Stmt::Return(e) => self.expr(e, module, fname),
            Stmt::Parallel { body, .. } => {
                // parallel body shares the enclosing scope for decls (§14:
                // declarations publish upward) — check in a child scope so
                // names still resolve, but don't double-declare
                self.scope.push(HashSet::new());
                self.stmts(body, module, fname);
                self.scope.pop();
            }
            Stmt::Spawn(e) => self.expr(e, module, fname),
            Stmt::Break | Stmt::Continue | Stmt::Use(_) => {}
        }
    }

    fn match_arm(&mut self, arm: &MatchArm, module: &str, fname: &str) {
        self.scope.push(HashSet::new());
        if let Some(b) = &arm.bind {
            self.declare(b);
        }
        self.stmts(&arm.body, module, fname);
        self.scope.pop();
    }

    fn expr(&mut self, e: &Expr, module: &str, fname: &str) {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Null => {}
            Expr::Str(parts) => {
                for p in parts {
                    if let StrPart::Interp(ie) = p {
                        self.expr(ie, module, fname);
                    }
                }
            }
            Expr::Ident(name) => {
                if !self.lookup(name) {
                    self.errors.push(format!(
                        "unknown name `{name}` in {module}.{fname} — not a local, fn, type, module, or builtin"
                    ));
                }
            }
            Expr::Tuple(es) | Expr::List(es) => {
                for e in es {
                    self.expr(e, module, fname);
                }
            }
            Expr::Map(kvs) => {
                for (k, v) in kvs {
                    self.expr(k, module, fname);
                    self.expr(v, module, fname);
                }
            }
            Expr::Call { callee, args } => {
                self.expr(callee, module, fname);
                for a in args {
                    self.expr(a, module, fname);
                }
                self.arity_check(callee, args, module, fname);
            }
            Expr::Field { obj, .. } => self.expr(obj, module, fname),
            Expr::Unary { expr, .. } => self.expr(expr, module, fname),
            Expr::Binary { l, r, .. } => {
                self.expr(l, module, fname);
                self.expr(r, module, fname);
            }
            Expr::Cast { expr, .. } => self.expr(expr, module, fname),
            Expr::CountOf(inner) => self.expr(inner, module, fname),
            Expr::DeclArg { var, .. } => {
                self.declare(var);
            }
            Expr::NamedArg { value, .. } => self.expr(value, module, fname),
            Expr::RecordType { fields } => {
                for (_, t) in fields {
                    self.type_expr(t);
                }
            }
            Expr::Callback { call, body } => {
                self.expr(call, module, fname);
                self.scope.push(HashSet::new());
                self.stmts(body, module, fname);
                self.scope.pop();
            }
        }
    }

    fn type_expr(&mut self, t: &TypeExpr) {
        match t {
            TypeExpr::Named(n) => {
                if !self.lookup(n)
                    && !["int", "float", "str", "bool", "bytes", "null", "any", "Err", "List", "Map"].contains(&n.as_str())
                {
                    self.warnings.push(format!("type `{n}` is not a known type value"));
                }
            }
            TypeExpr::App { name, args } => {
                if !["List", "Map"].contains(&name.as_str()) && !self.lookup(name) {
                    self.warnings.push(format!("type `{name}` is not a known type value"));
                }
                for a in args {
                    self.type_expr(a);
                }
            }
            TypeExpr::Tuple(ts) | TypeExpr::Union(ts) => {
                for t in ts {
                    self.type_expr(t);
                }
            }
            TypeExpr::Ref(n) => {
                if !self.lookup(n) {
                    self.warnings.push(format!("type `{n}` is not a known type value"));
                }
            }
            TypeExpr::Null | TypeExpr::Any => {}
        }
    }

    /// Soft arity (§9): too many args → `args.rest`; too few → missingArg
    /// poison at runtime. Checker warns on missing args only.
    fn arity_check(&mut self, callee: &Expr, args: &[Expr], module: &str, fname: &str) {
        let name = match callee {
            Expr::Ident(n) => Some(n.clone()),
            Expr::Field { obj, name } => {
                if let Expr::Ident(m) = obj.as_ref() {
                    Some(format!("{m}.{name}"))
                } else {
                    None
                }
            }
            _ => None,
        };
        let Some(name) = name else { return };
        // user fn?
        let params = self
            .globals
            .params
            .get(&name)
            .or_else(|| self.globals.params.get(name.rsplit('.').next().unwrap_or(&name)));
        if let Some(params) = params {
            let has_args_param = params.iter().any(|p| p == "args");
            let named = args.iter().any(|a| matches!(a, Expr::NamedArg { .. }));
            if !named && !has_args_param && args.len() < params.len() {
                self.warnings.push(format!(
                    "{module}.{fname}: call to `{name}` supplies {} arg(s) but it declares {} — missing args become missingArg poison at runtime (§9)",
                    args.len(),
                    params.len()
                ));
            }
        }
    }
}

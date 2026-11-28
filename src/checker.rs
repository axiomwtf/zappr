//! Checker: names resolve, arity is soft-checked, poison paths linted,
//! naming law + no-shadowing enforced (SPEC §4, §5, §9), and the
//! exclusivity law (§15.1): moves are compile-time and local, visible
//! borrows are checked per-function.

use crate::parser::{BinOp, Expr, ForPat, Item, MatchArm, Program, Stmt, StrPart, TypeExpr};
use std::collections::{HashMap, HashSet};

/// Names that always resolve without declaration.
const AMBIENT: &[&str] = &[
    "print", "read", "readInto", "readDialogue", "assert", "typeof", "len", "str", "int",
    "float", "env", "clock", "nav", "ui", "route", "onClick", "weak", "channel", "spawn",
    "parallel", "zapYaml", "thisShell", "math", "json", "exit", "List", "Map", "null", "true",
    "false", "int", "float", "str", "bool", "bytes", "Err", "poison", "any", "args",
];

/// Bare builtins whose arguments are only read — never moved (§15.1).
const BORROW_FNS: &[&str] = &[
    "len", "str", "int", "float", "typeof", "assert", "weak", "exit", "read", "readDialogue",
    "readInto", "Err", "channel", "route", "print",
];

/// `ns.fn` pairs whose arguments are only read.
const BORROW_NS: &[&str] = &[
    "env.get", "env.set", "nav.go", "thisShell.shellOut",
];

const BORROW_NS_PREFIX: &[&str] = &["print.", "env.", "clock.", "math.", "json.", "args."];

/// Method names that mutate their receiver — a write access (§15.1).
const MUTATING_METHODS: &[&str] = &[
    "push", "pop", "reverse", "set", "insert", "remove", "delete", "clear", "send", "close",
    "sort",
];

/// Scalar types whose values copy instead of move (§15.1, §7.1).
const SCALAR_TYS: &[&str] = &[
    "int", "i8", "i16", "i32", "i64", "u8", "u16", "u32", "u64", "float", "f32", "f64", "bool",
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

/// An active borrow of a binding's storage — iteration holds one for the
/// loop body so mutation of the iterated collection is an error (§15.1).
struct Borrow {
    root: String,
    origin: String,
}

/// Statically known type of a binding's value — feeds §7.6 match
/// exhaustiveness. Inference is strictly local: constructor calls,
/// union definitions, fnType narrowing and same-module fn return sets.
#[derive(Clone)]
enum VarTy {
    Unknown,
    /// `!s = Circle(...)` — s is that record/variant type
    Named(String),
    /// the value may be any of these variants (inferred fn return set)
    Union(Vec<String>),
}

#[derive(Clone)]
struct Binding {
    copyable: bool,
    ty: VarTy,
}

struct Checker<'a> {
    errors: Vec<String>,
    warnings: Vec<String>,
    globals: &'a GlobalSyms,
    /// lexical scopes; innermost last — name → binding
    scope: Vec<HashMap<String, Binding>>,
    /// bindings moved out of — dead until re-declared (§15.1)
    moved: HashSet<String>,
    /// live borrows of binding storage (iteration)
    borrows: Vec<Borrow>,
    /// names visible to an enclosing lambda/parallel body — shared, so
    /// they cannot be moved out of (§15.1 escape hatch)
    captured: HashSet<String>,
    /// known union type definitions: `!Shape = Circle | Rect` (§7.6)
    union_defs: HashMap<String, Vec<String>>,
    /// inferred return variant sets for this module's fns (same module
    /// only — no cross-module fixpoint, §4 fast-by-construction)
    ret_tys: HashMap<String, Vec<String>>,
    /// module-level `!Name = ...` declarations (§7.6) — visible to every
    /// fn in the module
    top_decls: HashMap<String, VarTy>,
}

pub fn check_program(prog: &Program, module: &str, globals: &GlobalSyms) -> CheckOutcome {
    // per-fn return variant sets (§7.6): every `return` in the fn must be
    // a constructor call on a PascalCase type for the set to be known
    let mut ret_tys: HashMap<String, Vec<String>> = HashMap::new();
    for item in &prog.items {
        if let Item::Fn { name, body, .. } = item {
            if let Some(vs) = fn_return_variants(body) {
                ret_tys.insert(name.clone(), vs);
            }
        }
    }
    let mut c = Checker {
        errors: Vec::new(),
        warnings: Vec::new(),
        globals,
        scope: vec![HashMap::new()],
        moved: HashSet::new(),
        borrows: Vec::new(),
        captured: HashSet::new(),
        union_defs: HashMap::new(),
        ret_tys,
        top_decls: HashMap::new(),
    };
    // module-level declarations first: union defs and types every fn sees
    for item in &prog.items {
        if let Item::TopDecl(decls) = item {
            for d in decls {
                match d {
                    Stmt::Decl { name, init, .. } => {
                        if let Some(vs) = union_of_init(init) {
                            c.union_defs.insert(name.clone(), vs);
                        }
                        c.top_decls.insert(name.clone(), decl_var_ty(init));
                    }
                    Stmt::DeclTuple { names, .. } => {
                        for n in names {
                            c.top_decls.insert(n.clone(), VarTy::Unknown);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    for item in &prog.items {
        if let Item::Fn { name, params, body, .. } = item {
            c.scope = vec![HashMap::new()];
            c.moved.clear();
            c.borrows.clear();
            c.captured.clear();
            for p in params {
                let copyable = p
                    .ty
                    .as_ref()
                    .map(|t| scalar_ty(t))
                    .unwrap_or(false);
                c.declare(&p.name, copyable, VarTy::Unknown);
            }
            c.stmts(body, module, name);
        }
    }
    CheckOutcome {
        errors: c.errors,
        warnings: c.warnings,
    }
}

/// Inferred variant set of a fn's returns (§7.6): every `return` in the
/// body — through if/match arms — must be a constructor call on a
/// PascalCase type. Any other return shape (or a bare fallthrough) leaves
/// the set unknown; checking then stays at runtime.
fn fn_return_variants(body: &[Stmt]) -> Option<Vec<String>> {
    fn go(stmts: &[Stmt], vs: &mut Vec<String>) -> bool {
        for s in stmts {
            match s {
                Stmt::Return(e) => match e {
                    Expr::Call { callee, .. } => match callee.as_ref() {
                        Expr::Ident(n) if is_type_name(n) => {
                            vs.push(n.clone());
                        }
                        _ => return false,
                    },
                    _ => return false,
                },
                Stmt::If { then_body, else_body, .. } => {
                    if !go(then_body, vs) {
                        return false;
                    }
                    if let Some(eb) = else_body {
                        if !go(eb, vs) {
                            return false;
                        }
                    }
                }
                Stmt::Match { arms, .. } => {
                    for a in arms {
                        if !go(&a.body, vs) {
                            return false;
                        }
                    }
                }
                _ => {}
            }
        }
        true
    }
    let mut vs = Vec::new();
    if go(body, &mut vs) && !vs.is_empty() {
        Some(vs)
    } else {
        None
    }
}

/// Is this identifier a type name? (PascalCase — the naming law, §5)
fn is_type_name(n: &str) -> bool {
    n.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Top-level names a parallel body statement publishes upward (§14) —
/// mirrors the runtime's publish rule: only `!x = ...` / `!(a, b) = ...`
/// statements of the block's own top level publish.
fn parallel_publishes(stmt: &Stmt) -> Vec<String> {
    match stmt {
        Stmt::Decl { name, .. } => vec![name.clone()],
        Stmt::DeclTuple { names, .. } => names.clone(),
        _ => Vec::new(),
    }
}

/// `A | B | C` chain of PascalCase idents — a union type definition (§7.6).
fn union_of_init(e: &Expr) -> Option<Vec<String>> {
    fn go(e: &Expr, vs: &mut Vec<String>) -> bool {
        match e {
            Expr::Binary { op: BinOp::Or, l, r } => go(l, vs) && go(r, vs),
            Expr::Ident(n) if is_type_name(n) => {
                vs.push(n.clone());
                true
            }
            _ => false,
        }
    }
    let mut vs = Vec::new();
    if go(e, &mut vs) {
        Some(vs)
    } else {
        None
    }
}

/// Static type of a declaration whose init is a plain constructor call.
fn decl_var_ty(init: &Expr) -> VarTy {
    if let Expr::Call { callee, .. } = init {
        if let Expr::Ident(n) = callee.as_ref() {
            if is_type_name(n) {
                return VarTy::Named(n.clone());
            }
        }
    }
    VarTy::Unknown
}

/// `if x.fnType(T) as v` — the then-bind holds a value of type T (§8.2).
fn fntype_narrow(cond: &Expr) -> VarTy {
    if let Expr::Call { callee, args } = cond {
        if let Expr::Field { name, .. } = callee.as_ref() {
            if name == "fnType" {
                if let Some(Expr::Ident(n)) = args.first() {
                    return VarTy::Named(n.clone());
                }
            }
        }
    }
    VarTy::Unknown
}

fn scalar_ty(t: &TypeExpr) -> bool {
    match t {
        TypeExpr::Named(n) => SCALAR_TYS.contains(&n.as_str()),
        _ => false,
    }
}

/// Root binding an expression reads through field/cast chains — used to
/// tell whether an access touches a borrowed binding's storage.
fn root_ident(e: &Expr) -> Option<String> {
    match e {
        Expr::Ident(n) => Some(n.clone()),
        Expr::Field { obj, .. } => root_ident(obj),
        Expr::Cast { expr, .. } => root_ident(expr),
        Expr::CountOf(inner) => root_ident(inner),
        _ => None,
    }
}

impl Checker<'_> {
    fn declare(&mut self, name: &str, copyable: bool, ty: VarTy) -> bool {
        // no-shadowing law (§5.x): a decl in an inner scope that hides an
        // outer name is an error, not a warning
        for (i, s) in self.scope.iter().enumerate().rev() {
            if s.contains_key(name) {
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
        self.scope
            .last_mut()
            .unwrap()
            .insert(name.to_string(), Binding { copyable, ty });
        true
    }

    fn lookup(&self, name: &str) -> bool {
        self.scope.iter().rev().any(|s| s.contains_key(name))
            || self.top_decls.contains_key(name)
            || self.globals.names.contains(name)
            || AMBIENT.contains(&name)
    }

    /// Is `name` a local binding (not a global fn/type/ambient)?
    fn is_local(&self, name: &str) -> bool {
        self.scope.iter().rev().any(|s| s.contains_key(name))
    }

    fn copyable(&self, name: &str) -> bool {
        self.scope
            .iter()
            .rev()
            .find_map(|s| s.get(name).map(|b| b.copyable))
            .unwrap_or(false)
    }

    /// Statically known type of a local binding (§7.6)
    fn var_ty(&self, name: &str) -> VarTy {
        self.scope
            .iter()
            .rev()
            .find_map(|s| s.get(name).map(|b| b.ty.clone()))
            .or_else(|| self.top_decls.get(name).cloned())
            .unwrap_or(VarTy::Unknown)
    }

    /// Does this expression evaluate to a copyable scalar? (§15.1 —
    /// scalars copy; everything else moves)
    fn copyable_expr(&self, e: &Expr) -> bool {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Null => true,
            Expr::Ident(n) => self.is_local(n) && self.copyable(n),
            Expr::Unary { expr, .. } => self.copyable_expr(expr),
            Expr::Cast { expr, ty } => scalar_ty(ty) || self.copyable_expr(expr),
            _ => false,
        }
    }

    /// Borrow-position use of a name: dead-binding check only.
    fn borrow_ident(&mut self, name: &str, module: &str, fname: &str) {
        if !self.lookup(name) {
            self.errors.push(format!(
                "unknown name `{name}` in {module}.{fname} — not a local, fn, type, module, or builtin"
            ));
            return;
        }
        if self.moved.contains(name) {
            self.errors.push(format!(
                "use of moved value `{name}` in {module}.{fname} — it was moved on a previous use (§15.1)"
            ));
        }
    }

    /// Move-position use of a name: dead check, borrow check, then kill
    /// the binding unless the value copies (§15.1).
    fn move_ident(&mut self, name: &str, module: &str, fname: &str) {
        if !self.is_local(name) {
            // globals, fns, types, ambient names are not movable bindings
            if !self.lookup(name) {
                self.errors.push(format!(
                    "unknown name `{name}` in {module}.{fname} — not a local, fn, type, module, or builtin"
                ));
            }
            return;
        }
        if self.moved.contains(name) {
            self.errors.push(format!(
                "use of moved value `{name}` in {module}.{fname} — it was moved on a previous use (§15.1)"
            ));
            return;
        }
        if let Some(b) = self.borrows.iter().find(|b| b.root == *name) {
            self.errors.push(format!(
                "cannot move `{name}` in {module}.{fname} — its storage is borrowed by {} (§15.1)",
                b.origin
            ));
            return;
        }
        if self.copyable(name) || self.captured.contains(name) {
            return;
        }
        self.moved.insert(name.to_string());
    }

    /// Run `f` with every currently-visible local treated as captured —
    /// lambda bodies and parallel blocks share the enclosing scope, so
    /// outer bindings cannot be moved out of them (§15.1).
    fn with_captured<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let mut added = Vec::new();
        for s in &self.scope {
            for n in s.keys() {
                if self.captured.insert(n.clone()) {
                    added.push(n.clone());
                }
            }
        }
        let r = f(self);
        for n in added {
            self.captured.remove(&n);
        }
        r
    }

    /// Do call arguments move? Read-only builtins borrow; everything else
    /// consumes (§15.1, §9.2).
    fn args_move(callee: &Expr) -> bool {
        match callee {
            Expr::Ident(n) => {
                if BORROW_FNS.contains(&n.as_str()) || n == "List" || n == "Map" {
                    false
                } else {
                    true
                }
            }
            Expr::Field { obj, name } => {
                if let Expr::Ident(ns) = obj.as_ref() {
                    let full = format!("{ns}.{name}");
                    if BORROW_NS.contains(&full.as_str())
                        || BORROW_NS_PREFIX.iter().any(|p| full.starts_with(p))
                    {
                        return false;
                    }
                }
                true
            }
            _ => true,
        }
    }

    /// Is this a write to a binding borrowed by an active iteration?
    fn borrow_conflict(&mut self, root: &str, what: &str, module: &str, fname: &str) {
        if let Some(b) = self.borrows.iter().find(|b| b.root == *root) {
            self.errors.push(format!(
                "exclusivity: {what} on `{root}` in {module}.{fname} while it is borrowed by {} — at most one writer or readers, never both (§15.1)",
                b.origin
            ));
        }
    }

    fn stmts(&mut self, body: &[Stmt], module: &str, fname: &str) {
        for s in body {
            self.stmt(s, module, fname);
        }
    }

    fn stmt(&mut self, s: &Stmt, module: &str, fname: &str) {
        match s {
            Stmt::Decl {
                name,
                ty,
                init,
                ..
            } => {
                self.walk(init, true, module, fname);
                // `!Shape = Circle | Rect` — union type definition (§7.6)
                if let Some(vs) = union_of_init(init) {
                    self.union_defs.insert(name.clone(), vs);
                }
                let copyable = ty
                    .as_ref()
                    .map(|t| scalar_ty(t))
                    .unwrap_or(false)
                    || self.copyable_expr(init);
                // `!s = pickShape()` — s holds the callee's variant set (§7.6)
                let vty = if union_of_init(init).is_some() {
                    VarTy::Unknown // a union def itself is a type value
                } else if let Expr::Call { callee, .. } = init {
                    match callee.as_ref() {
                        Expr::Ident(n) if is_type_name(n) => VarTy::Named(n.clone()),
                        Expr::Ident(n) => match self.ret_tys.get(n) {
                            Some(vs) => VarTy::Union(vs.clone()),
                            None => VarTy::Unknown,
                        },
                        _ => VarTy::Unknown,
                    }
                } else {
                    VarTy::Unknown
                };
                self.declare(name, copyable, vty);
            }
            Stmt::DeclTuple { names, init, .. } => {
                self.walk(init, true, module, fname);
                for n in names {
                    self.declare(n, false, VarTy::Unknown);
                }
            }
            Stmt::Reassign {
                target, op, value, ..
            } => {
                if !self.lookup(target) {
                    self.errors.push(format!("`{target}` is not declared — use `!{target} =` to declare"));
                }
                if let Some(v) = value {
                    self.walk(v, true, module, fname);
                }
                // any reassignment (set/inc/dec) writes the binding —
                // conflicts with an active iteration borrow, and a moved
                // binding is re-initialized (§15.1)
                self.moved.remove(target);
                if self.borrows.iter().any(|b| b.root == *target) {
                    self.borrow_conflict(target, "reassignment", module, fname);
                }
                let _ = op;
            }
            Stmt::ReassignField { obj, value, .. } => {
                if !self.lookup(obj) {
                    self.errors.push(format!("`{obj}` is not declared"));
                }
                self.walk(value, true, module, fname);
                self.borrow_conflict(obj, "field write", module, fname);
                if self.moved.contains(obj) {
                    self.errors.push(format!(
                        "use of moved value `{obj}` in {module}.{fname} — it was moved on a previous use (§15.1)"
                    ));
                }
            }
            Stmt::Expr(e) => self.walk(e, false, module, fname),
            Stmt::If {
                cond,
                bind,
                then_body,
                else_body,
                else_bind,
                ..
            } => {
                // `if e as u` moves the bound value; a bare condition borrows
                let bound = bind.is_some() || else_bind.is_some();
                self.walk(cond, bound, module, fname);
                self.scope.push(HashMap::new());
                if let Some(b) = bind {
                    let cp = self.copyable_expr(cond);
                    let vty = fntype_narrow(cond);
                    self.declare(b, cp, vty);
                }
                self.stmts(then_body, module, fname);
                self.scope.pop();
                if let Some(eb) = else_body {
                    self.scope.push(HashMap::new());
                    if let Some(b) = else_bind {
                        let cp = self.copyable_expr(cond);
                        self.declare(b, cp, VarTy::Unknown);
                    }
                    self.stmts(eb, module, fname);
                    self.scope.pop();
                }
            }
            Stmt::ForIn { pat, iter, body, .. } => {
                self.walk(iter, false, module, fname);
                // iteration borrows the collection's storage for the body —
                // mutating it inside is an exclusivity error (§15.1)
                let root = root_ident(iter);
                if let Some(r) = &root {
                    self.borrows.push(Borrow {
                        root: r.clone(),
                        origin: format!("for..in `{r}`"),
                    });
                }
                self.scope.push(HashMap::new());
                match pat {
                    ForPat::Single(n) => {
                        self.declare(n, false, VarTy::Unknown);
                    }
                    ForPat::Tuple(ns) => {
                        for n in ns {
                            self.declare(n, false, VarTy::Unknown);
                        }
                    }
                }
                self.stmts(body, module, fname);
                self.scope.pop();
                if root.is_some() {
                    self.borrows.pop();
                }
            }
            Stmt::ForCond { cond, body, .. } => {
                self.walk(cond, false, module, fname);
                self.scope.push(HashMap::new());
                self.stmts(body, module, fname);
                self.scope.pop();
            }
            Stmt::ForInfinite { body, .. } => {
                self.scope.push(HashMap::new());
                self.stmts(body, module, fname);
                self.scope.pop();
            }
            Stmt::ForCStyle { init, cond, post, body, .. } => {
                self.scope.push(HashMap::new());
                for s in init {
                    self.stmt(s, module, fname);
                }
                self.walk(cond, false, module, fname);
                for s in post {
                    self.stmt(s, module, fname);
                }
                self.scope.push(HashMap::new());
                self.stmts(body, module, fname);
                self.scope.pop();
                self.scope.pop();
            }
            Stmt::Match { expr, arms, .. } => {
                self.walk(expr, false, module, fname);
                // compile-time exhaustiveness (§7.6) when the scrutinee's
                // variants are statically known
                if let Some(vs) = self.scrutinee_variants(expr) {
                    let mut missing: Vec<String> = vs;
                    let has_else = arms.iter().any(|a| a.ty.is_none());
                    for a in arms {
                        if let Some(t) = &a.ty {
                            for cov in self.arm_covers(t, 0) {
                                missing.retain(|m| *m != cov);
                            }
                        }
                    }
                    if !has_else && !missing.is_empty() {
                        self.errors.push(format!(
                            "match is not exhaustive: missing variant(s) {} — add arm(s) or an `else` (§7.6)",
                            missing.join(", ")
                        ));
                    }
                }
                for arm in arms {
                    self.match_arm(arm, module, fname);
                }
            }
            Stmt::Return(e) => self.walk(e, true, module, fname),
            Stmt::Parallel { body, .. } => {
                // parallel children share the enclosing scope — outer names
                // are captured, not movable (§14, §15.1)
                // §14 scoping: the block's top-level `!` declarations publish
                // upward — a name already in the enclosing scope is a compile
                // error, and the names exist after the join.
                for stmt in body {
                    for name in parallel_publishes(stmt) {
                        if self.lookup(&name) {
                            self.errors.push(format!(
                                "parallel block publishes `{name}`, which already exists in the enclosing scope — parallel declarations join upward, they do not shadow (§14)"
                            ));
                        }
                    }
                }
                self.with_captured(|c| {
                    c.scope.push(HashMap::new());
                    c.stmts(body, module, fname);
                    c.scope.pop();
                });
                // publish upward: the block's names live on after the join
                for stmt in body {
                    for name in parallel_publishes(stmt) {
                        if !self.scope.last().unwrap().contains_key(&name) {
                            self.scope.last_mut().unwrap().insert(
                                name,
                                Binding {
                                    copyable: false,
                                    ty: VarTy::Unknown,
                                },
                            );
                        }
                    }
                }
            }
            Stmt::Spawn(e) => self.walk(e, true, module, fname),
            Stmt::Break | Stmt::Continue | Stmt::Use(_) => {}
        }
    }

    fn match_arm(&mut self, arm: &MatchArm, module: &str, fname: &str) {
        self.scope.push(HashMap::new());
        if let Some(b) = &arm.bind {
            let ty = match &arm.ty {
                Some(TypeExpr::Named(n)) => VarTy::Named(n.clone()),
                _ => VarTy::Unknown,
            };
            self.declare(b, false, ty);
        }
        self.stmts(&arm.body, module, fname);
        self.scope.pop();
    }

    /// Variants the scrutinee may hold, when statically known (§7.6).
    fn scrutinee_variants(&self, e: &Expr) -> Option<Vec<String>> {
        // direct call: the callee's inferred return variant set
        if let Expr::Call { callee, .. } = e {
            if let Expr::Ident(n) = callee.as_ref() {
                if let Some(vs) = self.ret_tys.get(n) {
                    return Some(vs.clone());
                }
            }
        }
        let root = root_ident(e)?;
        match self.var_ty(&root) {
            VarTy::Union(vs) => Some(vs),
            VarTy::Named(n) => Some(vec![n]),
            VarTy::Unknown => None,
        }
    }

    /// Variants an arm's type expression covers — unions resolve through
    /// known union definitions, depth-limited against self-referential defs.
    fn arm_covers(&self, t: &TypeExpr, depth: usize) -> Vec<String> {
        if depth > 16 {
            return Vec::new();
        }
        match t {
            TypeExpr::Named(n) => match self.union_defs.get(n) {
                Some(vs) => vs.clone(),
                None => vec![n.clone()],
            },
            TypeExpr::Union(vs) => vs
                .iter()
                .flat_map(|t| self.arm_covers(t, depth + 1))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// `move_here`: this expression's root value flows into a new owner.
    /// Only matters at Ident leaves — every other position decides for
    /// itself (§15.1).
    fn walk(&mut self, e: &Expr, move_here: bool, module: &str, fname: &str) {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Null => {}
            Expr::Str(parts) => {
                for p in parts {
                    if let StrPart::Interp(ie) = p {
                        self.walk(ie, false, module, fname);
                    }
                }
            }
            Expr::Ident(name) => {
                if move_here {
                    self.move_ident(name, module, fname);
                } else {
                    self.borrow_ident(name, module, fname);
                }
            }
            Expr::Tuple(es) | Expr::List(es) => {
                for e in es {
                    self.walk(e, true, module, fname);
                }
            }
            Expr::Map(kvs) => {
                for (k, v) in kvs {
                    self.walk(k, true, module, fname);
                    self.walk(v, true, module, fname);
                }
            }
            Expr::Call { callee, args } => {
                self.walk(callee, false, module, fname);
                let move_args = Self::args_move(callee);
                for a in args {
                    self.walk(a, move_args, module, fname);
                }
                self.readinto_check(callee, args, module, fname);
                self.method_borrow_check(callee, module, fname);
                self.arity_check(callee, args, module, fname);
            }
            Expr::Field { obj, .. } => self.walk(obj, false, module, fname),
            Expr::Unary { expr, .. } => self.walk(expr, false, module, fname),
            Expr::Binary { l, r, .. } => {
                self.walk(l, false, module, fname);
                self.walk(r, false, module, fname);
            }
            Expr::Cast { expr, .. } => self.walk(expr, false, module, fname),
            Expr::CountOf(inner) => self.walk(inner, false, module, fname),
            Expr::DeclArg { ty, var } => {
                self.declare(
                    var,
                    scalar_ty(&TypeExpr::Named(ty.clone())),
                    VarTy::Unknown,
                );
            }
            Expr::NamedArg { value, .. } => self.walk(value, true, module, fname),
            Expr::RecordType { fields } => {
                for (_, t) in fields {
                    self.type_expr(t);
                }
            }
            Expr::Callback { call, body } => {
                if let Expr::Call { callee, args } = call.as_ref() {
                    self.walk(callee, false, module, fname);
                    let move_args = Self::args_move(callee);
                    for a in args {
                        self.walk(a, move_args, module, fname);
                    }
                    self.readinto_check(callee, args, module, fname);
                    self.method_borrow_check(callee, module, fname);
                    self.arity_check(callee, args, module, fname);
                    self.with_captured(|c| {
                        c.scope.push(HashMap::new());
                        c.stmts(body, module, fname);
                        c.scope.pop();
                    });
                } else {
                    self.walk(call, false, module, fname);
                }
            }
        }
    }

    /// `readInto(x)` writes the binding — a borrowed root conflicts, and a
    /// moved binding is re-initialized (§9.4, §15.1).
    fn readinto_check(&mut self, callee: &Expr, args: &[Expr], module: &str, fname: &str) {
        if let Expr::Ident(n) = callee {
            if n == "readInto" {
                if let Some(Expr::Ident(var)) = args.first() {
                    if self.borrows.iter().any(|b| b.root == *var) {
                        self.borrow_conflict(var, "readInto", module, fname);
                    }
                    self.moved.remove(var);
                }
            }
        }
    }

    /// `recv.mutatingMethod(...)` while `recv`'s root is iteration-borrowed
    /// is an exclusivity error (§15.1).
    fn method_borrow_check(&mut self, callee: &Expr, module: &str, fname: &str) {
        if let Expr::Field { obj, name } = callee {
            if !MUTATING_METHODS.contains(&name.as_str()) {
                return;
            }
            if let Some(root) = root_ident(obj) {
                if self.borrows.iter().any(|b| b.root == root) {
                    self.borrow_conflict(&root, &format!("method `.{name}`"), module, fname);
                }
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

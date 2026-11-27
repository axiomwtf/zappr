//! C11 backend: transpiles the Zappr AST to C and compiles with `cc`
//! (SPEC §20). One tagged-value runtime (zaprt) is emitted ahead of the
//! translated functions; the result links statically against libc/pthread.
//!
//! Semantic mapping (mirrors src/interp.rs):
//! - every binding is a heap `ZRef` cell inside a `ZEnv` scope chain —
//!   closures capture the chain pointer, `!` defines, `/` walks and sets
//! - missing args bind `missingArg` poison; surplus lands in `args.rest`
//! - `parallel` publishes decls upward and runs sequentially here
//!   (documented deviation); `spawn` runs on a real pthread
//! - UI is the headless element model shared with uiruntime.rs

use crate::parser::{Expr, ForPat, Item, MatchArm, Param, Program, Stmt, StrPart, TypeExpr};
use crate::project::Project;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

/// Names that are builtin callables or namespaces (not user fns).
const BUILTIN_ROOTS: &[&str] = &[
    "print", "read", "readInto", "readDialogue", "assert", "typeof", "len", "str", "int",
    "float", "env", "clock", "nav", "ui", "math", "json", "thisShell", "route", "onClick",
    "weak", "channel", "spawn", "exit", "List", "Map", "Err", "parallel",
];

/// Primitive type names — evaluate to `Z_TYPE` values in expression position.
const PRIM_TYPES: &[&str] = &[
    "int", "float", "str", "bool", "bytes", "null", "any", "Err", "poison", "fn",
];

pub struct CGen<'a> {
    modules: Vec<(&'a str, &'a Program)>,
    reachable: &'a HashSet<String>,
    /// module name → (fused use-path → resolved module name)
    uses: HashMap<String, HashMap<String, String>>,
    module_names: HashSet<String>,
    /// bare fn name → qualified names (for call resolution)
    bare_fns: HashMap<String, Vec<String>>,
    /// qualified fn name → param names (table + named-arg binding)
    fn_params: HashMap<String, Vec<String>>,
    /// pre-scanned lambda count (ids assigned in emit order)
    lambda_count: usize,
    /// emit-order lambda id counter
    lambda_seq: usize,
    tmp_n: usize,
    loop_n: usize,
    /// loop break/continue label stack
    loop_labels: Vec<(String, String)>,
    /// declared names in the fn currently being emitted
    locals: HashSet<String>,
    cur_module: String,
    cur_fn_q: String,
    errors: Vec<String>,
    lambda_protos: String,
    lambda_defs: String,
}

/// Entry: emit full C source for the reachable set.
pub fn emit(project: &Project, reachable: &HashSet<String>) -> Result<String, String> {
    let modules: Vec<(&str, &Program)> = project
        .modules
        .iter()
        .map(|(m, _, p)| (m.as_str(), p))
        .collect();
    let module_names: HashSet<String> = modules.iter().map(|(m, _)| m.to_string()).collect();
    let mut g = CGen {
        modules,
        reachable,
        uses: HashMap::new(),
        module_names,
        bare_fns: HashMap::new(),
        fn_params: HashMap::new(),
        lambda_count: 0,
        lambda_seq: 0,
        tmp_n: 0,
        loop_n: 0,
        loop_labels: Vec::new(),
        locals: HashSet::new(),
        cur_module: String::new(),
        cur_fn_q: String::new(),
        errors: Vec::new(),
        lambda_protos: String::new(),
        lambda_defs: String::new(),
    };
    g.collect_symbols();
    g.prescan_lambdas();
    let src = g.generate(project);
    if !g.errors.is_empty() {
        return Err(format!("codegen:\n  {}", g.errors.join("\n  ")));
    }
    Ok(src)
}

impl<'a> CGen<'a> {
    fn collect_symbols(&mut self) {
        for (m, prog) in &self.modules {
            let mut m_uses = HashMap::new();
            for item in &prog.items {
                match item {
                    Item::Use(path) => {
                        let fused = fuse(path);
                        // the fused path IS the module name (§12.1)
                        if self.module_names.contains(&fused) {
                            m_uses.insert(fused.clone(), fused.clone());
                        }
                    }
                    Item::Fn { name, params, .. } => {
                        let q = format!("{m}.{name}");
                        self.bare_fns.entry(name.clone()).or_default().push(q.clone());
                        self.fn_params.insert(
                            q.clone(),
                            params.iter().map(|p| p.name.clone()).collect(),
                        );
                    }
                    _ => {}
                }
            }
            self.uses.insert(m.to_string(), m_uses);
        }
    }

    /// Count `Expr::Callback` occurrences in emit order so lambda ids in the
    /// fn table match the definitions emitted later.
    fn prescan_lambdas(&mut self) {
        for (m, prog) in &self.modules {
            for item in &prog.items {
                if let Item::Fn { name, body, .. } = item {
                    let q = format!("{m}.{name}");
                    if self.reachable.contains(&q) {
                        count_callbacks(body, &mut self.lambda_count);
                    }
                }
            }
        }
    }

    // ---------- driver ----------

    fn generate(&mut self, project: &Project) -> String {
        let mut s = String::with_capacity(1 << 20);
        s.push_str(RUNTIME_C);
        s.push_str("\n// ===== generated =====\n\n");
        // prototypes for user fns + lambdas
        for (m, prog) in &self.modules {
            for item in &prog.items {
                if let Item::Fn { name, .. } = item {
                    let q = format!("{m}.{name}");
                    if self.reachable.contains(&q) {
                        let _ = writeln!(s, "static ZVal {}(ZEnv *cap, ZVal *args, int nargs, int supplied);", mangled(&q));
                    }
                }
            }
        }
        for i in 0..self.lambda_count {
            let _ = writeln!(s, "static ZVal zf_lambda{i}(ZEnv *cap, ZVal *args, int nargs, int supplied);");
        }
        // fn table
        s.push_str("ZFnEnt z_fns[] = {\n");
        for (m, prog) in &self.modules {
            for item in &prog.items {
                if let Item::Fn { name, params, .. } = item {
                    let q = format!("{m}.{name}");
                    if !self.reachable.contains(&q) {
                        continue;
                    }
                    let plist: Vec<String> = params.iter().map(|p| format!("\"{}\"", cstr(&p.name))).collect();
                    let _ = writeln!(
                        s,
                        "  {{\"{q}\", {}, (const char*[]){{{}}}, {}}},",
                        mangled(&q),
                        plist.join(","),
                        params.len()
                    );
                }
            }
        }
        for i in 0..self.lambda_count {
            let _ = writeln!(s, "  {{\"lambda{i}\", zf_lambda{i}, 0, 0}},");
        }
        s.push_str("  {0,0,0,0}\n};\n\n");
        // bodies
        let mods: Vec<(&str, &Program)> = self.modules.clone();
        for (m, prog) in mods {
            for item in &prog.items {
                if let Item::Fn { name, params, body, .. } = item {
                    let q = format!("{m}.{name}");
                    if !self.reachable.contains(&q) {
                        continue;
                    }
                    self.cur_module = m.to_string();
                    self.cur_fn_q = q.clone();
                    self.emit_fn(&mut s, &q, params, body);
                }
            }
        }
        s.push_str(&self.lambda_defs);
        s.push_str(&zap_init(project));
        s.push_str(&MAIN_C);
        s
    }

    fn emit_fn(&mut self, s: &mut String, qname: &str, params: &[Param], body: &[Stmt]) {
        let fname = mangled(qname);
        self.locals.clear();
        self.loop_labels.clear();
        self.tmp_n = 0;
        self.loop_n = 0;
        collect_decl_names(body, &mut self.locals);
        for p in params {
            self.locals.insert(p.name.clone());
        }
        self.locals.insert("args".into());

        let _ = writeln!(s, "\nstatic ZVal {fname}(ZEnv *cap, ZVal *args, int nargs, int supplied) {{");
        s.push_str("  ZEnv *env = z_env_new(cap ? cap : z_global);\n");
        s.push_str("  ZVal z_ret = ZNOTHING;\n");
        // param binding with soft arity (§9)
        for (i, p) in params.iter().enumerate() {
            let _ = writeln!(
                s,
                "  z_env_define(env, \"{}\", ({} < nargs) ? args[{}] : z_poisonv(\"missingArg\", \"call to {}\", \"{}\", \"<missing>\"));",
                cstr(&p.name), i, i, cstr(qname), cstr(&p.name)
            );
        }
        // `args` magic record {count, rest}
        let _ = writeln!(
            s,
            "  {{ ZVec *_r = z_vec_new(); for (int _i = {}; _i < nargs; _i++) z_vec_push(_r, args[_i]); z_env_define(env, \"args\", z_args_record(supplied, _r, {})); }}",
            params.len(), params.len()
        );
        for st in body {
            self.emit_stmt(s, st, 1);
        }
        let _ = writeln!(s, "z_done: return z_ret; }}\n");
    }

    fn ind(&self, s: &mut String, n: usize) {
        for _ in 0..n {
            s.push_str("  ");
        }
    }

    fn tmp(&mut self, prefix: &str) -> String {
        self.tmp_n += 1;
        format!("_{prefix}{}", self.tmp_n)
    }

    // ---------- statements ----------

    fn emit_stmt(&mut self, s: &mut String, st: &Stmt, ind: usize) {
        match st {
            Stmt::Decl { name, init, .. } => {
                let e = self.emit_expr(init);
                self.ind(s, ind);
                let _ = writeln!(s, "z_env_define(env, \"{}\", {e});", cstr(name));
            }
            Stmt::DeclTuple { names, init, .. } => {
                let e = self.emit_expr(init);
                self.ind(s, ind);
                let _ = writeln!(s, "{{ ZVal _t = {e};");
                self.ind(s, ind + 1);
                for (i, n) in names.iter().enumerate() {
                    let _ = writeln!(
                        s,
                        "z_env_define(env, \"{}\", z_index(_t, {}));",
                        cstr(n), i
                    );
                }
                self.ind(s, ind);
                s.push_str("}\n");
            }
            Stmt::Reassign { target, op, value, .. } => {
                self.ind(s, ind);
                match op {
                    crate::parser::ReassignOp::Set => {
                        let e = value.as_ref().map(|v| self.emit_expr(v)).unwrap_or_else(|| "ZNOTHING".into());
                        let _ = writeln!(s, "z_env_set(env, \"{}\", {e});", cstr(target));
                    }
                    crate::parser::ReassignOp::Inc | crate::parser::ReassignOp::Dec => {
                        let d = if matches!(op, crate::parser::ReassignOp::Inc) { 1 } else { -1 };
                        let _ = writeln!(
                            s,
                            "z_env_set(env, \"{}\", z_add(z_env_get(env, \"{}\"), z_int({d}LL)));",
                            cstr(target), cstr(target)
                        );
                    }
                }
            }
            Stmt::ReassignField { obj, field, value, .. } => {
                let ov = format!("z_env_get(env, \"{}\")", cstr(obj));
                let e = self.emit_expr(value);
                self.ind(s, ind);
                let _ = writeln!(s, "z_field_set({ov}, \"{}\", {e});", cstr(field));
            }
            Stmt::Expr(e) => {
                if let Expr::Callback { call, body } = e {
                    self.emit_callback_stmt(s, call, body, ind);
                } else {
                    let ex = self.emit_expr(e);
                    self.ind(s, ind);
                    let _ = writeln!(s, "(void)({ex});");
                }
            }
            Stmt::If {
                cond, bind, then_body, else_body, else_bind, ..
            } => {
                let cv = self.tmp("c");
                let ce = self.emit_expr(cond);
                self.ind(s, ind);
                let _ = writeln!(s, "{{ ZVal {cv} = {ce};");
                self.ind(s, ind + 1);
                let _ = writeln!(s, "if (z_truthy({cv})) {{");
                self.ind(s, ind + 2);
                s.push_str("ZEnv *env_p = env; ZEnv *env = z_env_new(env_p);\n");
                self.ind(s, ind + 2);
                if let Some(b) = bind {
                    let _ = writeln!(s, "z_env_define(env, \"{}\", {cv});", cstr(b));
                }
                for st in then_body {
                    self.emit_stmt(s, st, ind + 2);
                }
                self.ind(s, ind + 1);
                s.push_str("}\n");
                if let Some(eb) = else_body {
                    self.ind(s, ind + 1);
                    s.push_str("else {\n");
                    self.ind(s, ind + 2);
                    s.push_str("ZEnv *env_p = env; ZEnv *env = z_env_new(env_p);\n");
                    self.ind(s, ind + 2);
                    if let Some(b) = else_bind {
                        let _ = writeln!(s, "z_env_define(env, \"{}\", {cv});", cstr(b));
                    }
                    for st in eb {
                        self.emit_stmt(s, st, ind + 2);
                    }
                    self.ind(s, ind + 1);
                    s.push_str("}\n");
                }
                self.ind(s, ind);
                s.push_str("}\n");
            }
            Stmt::ForIn { pat, iter, body, .. } => self.emit_for_in(s, pat, iter, body, ind),
            Stmt::ForCond { cond, body, .. } => {
                let ce = self.emit_expr(cond);
                self.loop_n += 1;
                let (bl, cl) = (format!("zb{}", self.loop_n), format!("zc{}", self.loop_n));
                self.loop_labels.push((bl.clone(), cl.clone()));
                self.ind(s, ind);
                let _ = writeln!(s, "while (z_truthy({ce})) {{");
                self.ind(s, ind + 1);
                s.push_str("ZEnv *env_p = env; ZEnv *env = z_env_new(env_p);\n");
                for st in body {
                    self.emit_stmt(s, st, ind + 1);
                }
                self.ind(s, ind + 1);
                let _ = writeln!(s, "{cl}: ;");
                self.ind(s, ind);
                s.push_str("}\n");
                self.ind(s, ind);
                let _ = writeln!(s, "{bl}: ;");
                self.loop_labels.pop();
            }
            Stmt::ForInfinite { body, .. } => {
                self.loop_n += 1;
                let (bl, cl) = (format!("zb{}", self.loop_n), format!("zc{}", self.loop_n));
                self.loop_labels.push((bl.clone(), cl.clone()));
                self.ind(s, ind);
                s.push_str("for (;;) {\n");
                self.ind(s, ind + 1);
                s.push_str("ZEnv *env_p = env; ZEnv *env = z_env_new(env_p);\n");
                for st in body {
                    self.emit_stmt(s, st, ind + 1);
                }
                self.ind(s, ind + 1);
                let _ = writeln!(s, "{cl}: ;");
                self.ind(s, ind);
                s.push_str("}\n");
                self.ind(s, ind);
                let _ = writeln!(s, "{bl}: ;");
                self.loop_labels.pop();
            }
            Stmt::ForCStyle { init, cond, post, body, .. } => {
                self.loop_n += 1;
                let (bl, cl) = (format!("zb{}", self.loop_n), format!("zc{}", self.loop_n));
                self.loop_labels.push((bl.clone(), cl.clone()));
                self.ind(s, ind);
                s.push_str("{\n");
                self.ind(s, ind + 1);
                s.push_str("ZEnv *env_p = env; ZEnv *env = z_env_new(env_p);\n");
                for st in init {
                    self.emit_stmt(s, st, ind + 1);
                }
                let ce = self.emit_expr(cond);
                self.ind(s, ind + 1);
                let _ = writeln!(s, "while (z_truthy({ce})) {{");
                self.ind(s, ind + 2);
                s.push_str("ZEnv *env_p2 = env; ZEnv *env = z_env_new(env_p2);\n");
                for st in body {
                    self.emit_stmt(s, st, ind + 2);
                }
                self.ind(s, ind + 2);
                let _ = writeln!(s, "{cl}: ;");
                // C-style continue must still run post
                for st in post {
                    self.emit_stmt(s, st, ind + 2);
                }
                self.ind(s, ind + 1);
                s.push_str("}\n");
                self.ind(s, ind + 1);
                let _ = writeln!(s, "{bl}: ;");
                self.ind(s, ind);
                s.push_str("}\n");
                self.loop_labels.pop();
            }
            Stmt::Match { expr, arms, .. } => {
                let mv = self.tmp("m");
                let me = self.emit_expr(expr);
                self.ind(s, ind);
                let _ = writeln!(s, "{{ ZVal {mv} = {me}; ZVal _mb = ZNOTHING;");
                for (ai, arm) in arms.iter().enumerate() {
                    self.emit_match_arm(s, &mv, arm, ind + 1, ai == 0, ai + 1 == arms.len());
                }
                self.ind(s, ind);
                s.push_str("}\n");
            }
            Stmt::Return(e) => {
                let ex = self.emit_expr(e);
                self.ind(s, ind);
                let _ = writeln!(s, "{{ z_ret = {ex}; goto z_done; }}");
            }
            Stmt::Parallel { body, .. } => {
                // sequential here; decls publish upward (§14) — emit inline
                for st in body {
                    self.emit_stmt(s, st, ind);
                }
            }
            Stmt::Spawn(e) => {
                let (callable, args_c, n) = self.emit_spawn_target(e);
                self.ind(s, ind);
                let _ = writeln!(s, "z_spawn({callable}, {args_c}, {n}, env);");
            }
            Stmt::Break => {
                if let Some((bl, _)) = self.loop_labels.last() {
                    self.ind(s, ind);
                    let _ = writeln!(s, "goto {bl};");
                } else {
                    self.errors.push(format!("`break` outside a loop in {}", self.cur_fn_q));
                }
            }
            Stmt::Continue => {
                if let Some((_, cl)) = self.loop_labels.last() {
                    self.ind(s, ind);
                    let _ = writeln!(s, "goto {cl};");
                } else {
                    self.errors.push(format!("`continue` outside a loop in {}", self.cur_fn_q));
                }
            }
            Stmt::Use(_) => {}
        }
    }

    fn emit_match_arm(
        &mut self,
        s: &mut String,
        mv: &str,
        arm: &MatchArm,
        ind: usize,
        first: bool,
        last: bool,
    ) {
        // build condition
        let cond = match &arm.ty {
            None => None, // else arm
            Some(ty) => {
                let tv = self.emit_typeval(ty);
                Some(format!("z_match({mv}, {tv}, &_mb)"))
            }
        };
        let kw = if first {
            if cond.is_some() { "if" } else { "if (1)" }
        } else {
            "else if"
        };
        self.ind(s, ind);
        match (&cond, last) {
            (Some(c), _) => {
                let _ = writeln!(s, "{kw} ({c}) {{");
            }
            (None, true) => {
                let _ = writeln!(s, "else {{");
            }
            (None, false) => {
                // non-final else arm — treat as catch-all anyway
                let _ = writeln!(s, "else {{");
            }
        }
        self.ind(s, ind + 1);
        s.push_str("ZEnv *env_p = env; ZEnv *env = z_env_new(env_p);\n");
        if let Some(b) = &arm.bind {
            self.ind(s, ind + 1);
            let _ = writeln!(s, "z_env_define(env, \"{}\", _mb);", cstr(b));
        }
        for st in &arm.body {
            self.emit_stmt(s, st, ind + 1);
        }
        self.ind(s, ind);
        s.push_str("}\n");
    }

    fn emit_for_in(&mut self, s: &mut String, pat: &ForPat, iter: &Expr, body: &[Stmt], ind: usize) {
        let iv = self.tmp("it");
        let ie = self.emit_expr(iter);
        self.ind(s, ind);
        let _ = writeln!(s, "{{ ZVal {iv} = {ie};");
        self.ind(s, ind + 1);

        // each branch: (if-cond, setup lines, for-header, bind value expr)
        // bind value may reference _i / _rv
        let branches: Vec<(String, Vec<String>, String, String)> = vec![
            (
                format!("{iv}.tag == Z_LIST || {iv}.tag == Z_TUPLE"),
                vec![format!("ZVec *_v = {iv}.u.vec;")],
                "for (size_t _i = 0; _i < _v->n; _i++) {".into(),
                "_v->v[_i]".into(),
            ),
            (
                format!("{iv}.tag == Z_MAP"),
                vec![format!("ZZMap *_m = {iv}.u.map;")],
                "for (size_t _i = 0; _i < _m->n; _i++) {".into(),
                "__MAP__".into(),
            ),
            (
                format!("{iv}.tag == Z_STR"),
                vec![
                    format!("const char *_s = {iv}.u.s;"),
                    "for (size_t _i = 0; _s[_i]; _i++) {".into(),
                ][..1].to_vec(),
                "for (size_t _i = 0; _s[_i]; _i++) {".into(),
                "z_strn(_s + _i, 1)".into(),
            ),
            (
                format!("{iv}.tag == Z_CHAN"),
                vec![],
                "for (;;) {\nZVal _rv = z_chan_recv({}); if (_rv.tag == Z_NOTHING) break;".replace("{}", &iv),
                "_rv".into(),
            ),
            (
                format!("{iv}.tag == Z_INT || {iv}.tag == Z_FLOAT"),
                vec![format!("long long _n = (long long)z_num({iv});")],
                "for (long long _i = 0; _i < _n; _i++) {".into(),
                "z_int(_i)".into(),
            ),
        ];

        let mut first = true;
        for (cond, setup, header, bindval) in branches {
            self.loop_n += 1;
            let (bl, cl) = (format!("zb{}", self.loop_n), format!("zc{}", self.loop_n));
            self.ind(s, ind + 1);
            let kw = if first { "if" } else { "else if" };
            let _ = writeln!(s, "{kw} ({cond}) {{");
            first = false;
            for line in &setup {
                self.ind(s, ind + 2);
                let _ = writeln!(s, "{line}");
            }
            self.ind(s, ind + 2);
            let _ = writeln!(s, "{header}");
            self.ind(s, ind + 3);
            s.push_str("ZEnv *env_p = env; ZEnv *env = z_env_new(env_p);\n");
            // map branch binds k/v directly
            if bindval == "__MAP__" {
                match pat {
                    ForPat::Tuple(names) => {
                        for (i, n) in names.iter().enumerate() {
                            let idx = if i == 0 { "k" } else { "v" };
                            let _ = writeln!(
                                s,
                                "z_env_define(env, \"{}\", _m->p[_i].{});",
                                cstr(n), idx
                            );
                        }
                    }
                    ForPat::Single(n) => {
                        let _ = writeln!(s, "z_env_define(env, \"{}\", _m->p[_i].v);", cstr(n));
                    }
                }
            } else {
                self.emit_for_bind(s, pat, &bindval, ind + 3);
            }
            self.loop_labels.push((bl.clone(), cl.clone()));
            for st in body {
                self.emit_stmt(s, st, ind + 3);
            }
            self.loop_labels.pop();
            self.ind(s, ind + 3);
            let _ = writeln!(s, "{cl}: ;");
            self.ind(s, ind + 2);
            s.push_str("}\n");
            self.ind(s, ind + 2);
            let _ = writeln!(s, "{bl}: ;");
            self.ind(s, ind + 1);
            s.push_str("}\n");
        }
        // fallback
        self.ind(s, ind + 1);
        let _ = writeln!(s, "else {{ z_die(\"for-in over non-iterable value\"); }}");
        self.ind(s, ind);
        s.push_str("}\n");
    }

    fn emit_for_bind(&mut self, s: &mut String, pat: &ForPat, valexpr: &str, ind: usize) {
        match pat {
            ForPat::Single(n) => {
                let _ = writeln!(s, "z_env_define(env, \"{}\", {valexpr});", cstr(n));
            }
            ForPat::Tuple(names) => {
                self.ind(s, ind);
                let _ = writeln!(s, "{{ ZVal _pv = {valexpr};");
                self.ind(s, ind + 1);
                for (i, n) in names.iter().enumerate() {
                    let _ = writeln!(
                        s,
                        "z_env_define(env, \"{}\", z_index(_pv, {}));",
                        cstr(n), i
                    );
                }
                self.ind(s, ind);
                s.push_str("}\n");
            }
        }
    }

    // ---------- expressions ----------

    fn emit_expr(&mut self, e: &Expr) -> String {
        match e {
            Expr::Int(n) => format!("z_int({n}LL)"),
            Expr::Float(f) => {
                let mut s = f.to_string();
                if !s.contains('.') && !s.contains('e') && !s.contains("inf") && !s.contains("NaN") {
                    s.push_str(".0");
                }
                format!("z_float({s})")
            }
            Expr::Bool(b) => format!("z_bool({})", if *b { 1 } else { 0 }),
            Expr::Null => "ZNOTHING".into(),
            Expr::Str(parts) => self.emit_str(parts),
            Expr::Ident(name) => self.emit_ident(name),
            Expr::Tuple(items) => {
                let inner: Vec<String> = items.iter().map(|i| self.emit_expr(i)).collect();
                format!("z_tuple((ZVal[]){{{}}}, {})", inner.join(","), inner.len())
            }
            Expr::List(items) => {
                let inner: Vec<String> = items.iter().map(|i| self.emit_expr(i)).collect();
                format!("z_listv((ZVal[]){{{}}}, {})", inner.join(","), inner.len())
            }
            Expr::Map(kvs) => {
                let inner: Vec<String> = kvs
                    .iter()
                    .map(|(k, v)| format!("{{{}, {}}}", self.emit_expr(k), self.emit_expr(v)))
                    .collect();
                format!("z_mapv((ZPair[]){{{}}}, {})", inner.join(","), inner.len())
            }
            Expr::Call { callee, args } => self.emit_call(callee, args),
            Expr::Field { obj, name } => {
                let ov = self.emit_expr(obj);
                format!("z_field({ov}, \"{}\")", cstr(name))
            }
            Expr::Unary { op, expr } => {
                let ev = self.emit_expr(expr);
                match op {
                    crate::parser::UnOp::Neg => format!("z_neg({ev})"),
                    crate::parser::UnOp::Not => format!("z_bool(!z_truthy({ev}))"),
                }
            }
            Expr::Binary { op, l, r } => {
                use crate::parser::BinOp::*;
                let lv = self.emit_expr(l);
                let rv = self.emit_expr(r);
                match op {
                    Add => format!("z_add({lv}, {rv})"),
                    Sub => format!("z_sub({lv}, {rv})"),
                    Mul => format!("z_mul({lv}, {rv})"),
                    Div => format!("z_div({lv}, {rv})"),
                    Mod => format!("z_mod({lv}, {rv})"),
                    Eq => format!("z_eqv({lv}, {rv})"),
                    Ne => format!("z_ne({lv}, {rv})"),
                    Lt => format!("z_lt({lv}, {rv})"),
                    Gt => format!("z_gt({lv}, {rv})"),
                    Le => format!("z_le({lv}, {rv})"),
                    Ge => format!("z_ge({lv}, {rv})"),
                    And => format!("z_bool(z_truthy({lv}) && z_truthy({rv}))"),
                    Or => format!("z_bool(z_truthy({lv}) || z_truthy({rv}))"),
                }
            }
            Expr::Cast { expr, ty } => {
                let ev = self.emit_expr(expr);
                let tv = self.emit_typeval(ty);
                format!("z_cast({ev}, {tv})")
            }
            Expr::CountOf(inner) => self.emit_expr(inner),
            Expr::DeclArg { ty, var } => {
                // read(int age) — declare + read + pass value (§9.4)
                format!(
                    "({{ ZVal _rd = z_read_parse(\"{}\"); z_env_define(env, \"{}\", _rd); _rd; }})",
                    cstr(ty), cstr(var)
                )
            }
            Expr::NamedArg { value, .. } => self.emit_expr(value),
            Expr::RecordType { fields } => {
                let inner: Vec<String> = fields
                    .iter()
                    .map(|(n, t)| format!("{{z_str(\"{}\"), {}}}", cstr(n), self.emit_typeval(t)))
                    .collect();
                format!(
                    "z_type_rec((ZPair[]){{{}}}, {}, NULL)",
                    inner.join(","),
                    inner.len()
                )
            }
            Expr::Callback { call, body } => {
                // only valid as a call's trailing block — emit as call with
                // the lambda appended
                if let Expr::Call { callee, args } = call.as_ref() {
                    let lid = self.lambda_count;
                    self.lambda_count += 1;
                    self.emit_lambda_def(lid, body);
                    let mut args2 = args.clone();
                    args2.push(Expr::Callback {
                        call: Box::new(Expr::Call {
                            callee: callee.clone(),
                            args: vec![],
                        }),
                        body: vec![],
                    });
                    // simpler: mark lambda id — handled by emit_call_lambda
                    let _ = args2;
                    self.emit_call_with_lambda(callee, args, lid)
                } else {
                    "z_errv(\"callback on non-call\", \"expr\")".into()
                }
            }
        }
    }

    fn emit_str(&mut self, parts: &[StrPart]) -> String {
        let mut acc: Option<String> = None;
        for p in parts {
            let piece = match p {
                StrPart::Lit(s) => format!("z_str(\"{}\")", cstr(s)),
                StrPart::Interp(e) => self.emit_expr(e),
            };
            acc = Some(match acc {
                None => piece,
                Some(a) => format!("z_str_catv({a}, {piece})"),
            });
        }
        acc.unwrap_or_else(|| "z_str(\"\")".into())
    }

    fn emit_ident(&mut self, name: &str) -> String {
        // local binding / param / args record
        if self.locals.contains(name) {
            return format!("z_env_get(env, \"{}\")", cstr(name));
        }
        // user fn as first-class value
        if let Some(q) = self.resolve_fn(name) {
            return format!("z_fnval(\"{}\", NULL)", q);
        }
        // primitive type names are type values (§7.4)
        if PRIM_TYPES.contains(&name) {
            return format!("z_type_prim(\"{}\")", name);
        }
        // bare List/Map are generic type constructors
        if name == "List" || name == "Map" {
            return format!("z_type_prim(\"{}\")", name);
        }
        // builtin namespace root as a value → callable by name
        if BUILTIN_ROOTS.contains(&name) {
            return format!("z_fnval(\"{}\", NULL)", name);
        }
        // globals (zapYaml, thisShell) and anything else → env lookup;
        // unknown names yield an Err value at runtime
        format!("z_env_get(env, \"{}\")", cstr(name))
    }

    /// Resolve a bare fn name to its qualified name (same module first).
    fn resolve_fn(&self, name: &str) -> Option<String> {
        let cands = self.bare_fns.get(name)?;
        if cands.len() == 1 {
            return Some(cands[0].clone());
        }
        let prefix = format!("{}.", self.cur_module);
        cands.iter().find(|c| c.starts_with(&prefix)).cloned().or_else(|| cands.first().cloned())
    }

    fn dotted_path(e: &Expr) -> Option<Vec<String>> {
        match e {
            Expr::Ident(n) => Some(vec![n.clone()]),
            Expr::Field { obj, name } => {
                let mut p = Self::dotted_path(obj)?;
                p.push(name.clone());
                Some(p)
            }
            _ => None,
        }
    }

    // ---------- calls ----------

    fn split_args(&mut self, args: &[Expr]) -> (Vec<String>, Option<String>) {
        let mut pos = Vec::new();
        let mut named: Vec<(String, String)> = Vec::new();
        for a in args {
            match a {
                Expr::NamedArg { name, value } => {
                    named.push((name.clone(), self.emit_expr(value)));
                }
                _ => pos.push(self.emit_expr(a)),
            }
        }
        let named_c = if named.is_empty() {
            None
        } else {
            let inner: Vec<String> = named
                .iter()
                .map(|(n, v)| format!("{{z_str(\"{}\"), {v}}}", cstr(n)))
                .collect();
            Some(format!(
                "z_named((ZPair[]){{{}}}, {})",
                inner.join(","),
                named.len()
            ))
        };
        (pos, named_c)
    }

    fn emit_call(&mut self, callee: &Expr, args: &[Expr]) -> String {
        let (pos, named_c) = self.split_args(args);
        let pos_c = format!("(ZVal[]){{{}}}", pos.join(","));
        let npos = pos.len();
        let named = named_c.unwrap_or_else(|| "ZNOTHING".into());

        // dotted callee: ns.fn(), obj.method(), mod.fn()
        if let Some(segs) = Self::dotted_path(callee) {
            if segs.len() >= 2 {
                let root = &segs[0];
                if self.module_names.contains(root) {
                    // module.fn — qualified call
                    let q = format!("{}.{}", root, segs[1]);
                    if segs.len() == 2 {
                        return format!(
                            "z_invoke(z_fnval(\"{}\", NULL), {pos_c}, {npos}, {named}, env)",
                            q
                        );
                    }
                    // mod.fn.rest(...) → call fn then method on result
                    let inner = format!(
                        "z_invoke(z_fnval(\"{}\", NULL), (ZVal[]){{}}, 0, ZNOTHING, env)",
                        q
                    );
                    let path = segs[2..].join(".");
                    return format!("z_method({inner}, \"{}\", {pos_c}, {npos})", cstr(&path));
                }
                if BUILTIN_ROOTS.contains(&root.as_str()) {
                    let dotted = segs.join(".");
                    return format!(
                        "z_invoke(z_fnval(\"{}\", NULL), {pos_c}, {npos}, {named}, env)",
                        dotted
                    );
                }
                // obj.method(...) — root is a value expression
                let rootval = self.emit_ident_or_expr(root, callee);
                let path = segs[1..].join(".");
                return format!("z_method({rootval}, \"{}\", {pos_c}, {npos}, env)", cstr(&path));
            }
            // single-segment dotted → plain ident handling below
        }

        // callee resolution: local > user fn > type/builtin > env value
        match callee {
            Expr::Ident(name) => {
                if self.locals.contains(name) {
                    return format!(
                        "z_invoke(z_env_get(env, \"{}\"), {pos_c}, {npos}, {named}, env)",
                        cstr(name)
                    );
                }
                if let Some(q) = self.resolve_fn(name) {
                    return format!(
                        "z_invoke(z_fnval(\"{}\", NULL), {pos_c}, {npos}, {named}, env)",
                        q
                    );
                }
                if name == "List" || name == "Map" {
                    // generic type constructor call: List(int) → Z_TYPE "List(int)"
                    return format!(
                        "z_type_generic(\"{}\", {pos_c}, {npos})",
                        name
                    );
                }
                if PRIM_TYPES.contains(&name.as_str()) {
                    return format!(
                        "z_invoke({} , {pos_c}, {npos}, {named}, env)",
                        self.emit_ident(name)
                    );
                }
                if BUILTIN_ROOTS.contains(&name.as_str()) {
                    return format!(
                        "z_invoke(z_fnval(\"{}\", NULL), {pos_c}, {npos}, {named}, env)",
                        name
                    );
                }
                return format!(
                    "z_invoke(z_env_get(env, \"{}\"), {pos_c}, {npos}, {named}, env)",
                    cstr(name)
                );
            }
            Expr::Field { obj, name } => {
                // method call on a computed root: "abc".upper(), f().len()
                let root = self.emit_expr(obj);
                format!(
                    "z_method({root}, \"{}\", {pos_c}, {npos}, env)",
                    cstr(name)
                )
            }
            _ => {
                let cv = self.emit_expr(callee);
                format!("z_invoke({cv}, {pos_c}, {npos}, {named}, env)")
            }
        }
    }

    /// Root of a dotted path: ident → local/fn/env resolution; else expr.
    fn emit_ident_or_expr(&mut self, name: &str, _whole: &Expr) -> String {
        if self.locals.contains(name) {
            return format!("z_env_get(env, \"{}\")", cstr(name));
        }
        if let Some(q) = self.resolve_fn(name) {
            return format!("z_fnval(\"{}\", NULL)", q);
        }
        if PRIM_TYPES.contains(&name) || name == "List" || name == "Map" {
            return format!("z_type_prim(\"{}\")", name);
        }
        if BUILTIN_ROOTS.contains(&name) {
            return format!("z_fnval(\"{}\", NULL)", name);
        }
        format!("z_env_get(env, \"{}\")", cstr(name))
    }

    fn emit_call_with_lambda(&mut self, callee: &Expr, args: &[Expr], lid: usize) -> String {
        // append lambda fnval as last arg
        let (pos, named_c) = self.split_args(args);
        let mut pos = pos;
        pos.push(format!("z_fnval(\"lambda{lid}\", env)"));
        let pos_c = format!("(ZVal[]){{{}}}", pos.join(","));
        let npos = pos.len();
        let named = named_c.unwrap_or_else(|| "ZNOTHING".into());
        // reuse callable resolution by delegating to a synthesized call
        match callee {
            Expr::Ident(name) if self.locals.contains(name) => format!(
                "z_invoke(z_env_get(env, \"{}\"), {pos_c}, {npos}, {named}, env)",
                cstr(name)
            ),
            Expr::Ident(name) => {
                if let Some(q) = self.resolve_fn(name) {
                    format!("z_invoke(z_fnval(\"{q}\", NULL), {pos_c}, {npos}, {named}, env)")
                } else if BUILTIN_ROOTS.contains(&name.as_str()) {
                    format!("z_invoke(z_fnval(\"{name}\", NULL), {pos_c}, {npos}, {named}, env)")
                } else {
                    format!("z_invoke(z_env_get(env, \"{}\"), {pos_c}, {npos}, {named}, env)", cstr(name))
                }
            }
            _ => {
                if let Some(segs) = Self::dotted_path(callee) {
                    if segs.len() >= 2 {
                        let root = &segs[0];
                        if self.module_names.contains(root) {
                            let q = format!("{}.{}", root, segs[1]);
                            if segs.len() == 2 {
                                return format!("z_invoke(z_fnval(\"{q}\", NULL), {pos_c}, {npos}, {named}, env)");
                            }
                        }
                        if BUILTIN_ROOTS.contains(&root.as_str()) {
                            let dotted = segs.join(".");
                            return format!("z_invoke(z_fnval(\"{dotted}\", NULL), {pos_c}, {npos}, {named}, env)");
                        }
                        let rootval = self.emit_ident_or_expr(root, callee);
                        let path = segs[1..].join(".");
                        // method + lambda: append lambda to method args too
                        return format!("z_method({rootval}, \"{}\", {pos_c}, {npos}, env)", cstr(&path));
                    }
                }
                let cv = self.emit_expr(callee);
                format!("z_invoke({cv}, {pos_c}, {npos}, {named}, env)")
            }
        }
    }

    fn emit_callback_stmt(&mut self, s: &mut String, call: &Expr, body: &[Stmt], ind: usize) {
        if let Expr::Call { callee, args } = call {
            let lid = self.lambda_seq;
            self.lambda_seq += 1;
            self.emit_lambda_def(lid, body);
            let ex = self.emit_call_with_lambda(callee, args, lid);
            self.ind(s, ind);
            let _ = writeln!(s, "(void)({ex});");
        } else {
            self.errors.push(format!("callback on non-call in {}", self.cur_fn_q));
        }
    }

    fn emit_spawn_target(&mut self, e: &Expr) -> (String, String, usize) {
        if let Expr::Call { callee, args } = e {
            let (pos, _) = self.split_args(args);
            let pos_c = format!("(ZVal[]){{{}}}", pos.join(","));
            let n = pos.len();
            // callable: resolve like emit_call's callee part
            let callable = match callee.as_ref() {
                Expr::Ident(name) => {
                    if let Some(q) = self.resolve_fn(name) {
                        format!("z_fnval(\"{q}\", NULL)")
                    } else if BUILTIN_ROOTS.contains(&name.as_str()) {
                        format!("z_fnval(\"{name}\", NULL)")
                    } else {
                        format!("z_env_get(env, \"{}\")", cstr(name))
                    }
                }
                _ => {
                    if let Some(segs) = Self::dotted_path(callee) {
                        if segs.len() == 2 && self.module_names.contains(&segs[0]) {
                            format!("z_fnval(\"{}.{}\", NULL)", segs[0], segs[1])
                        } else if BUILTIN_ROOTS.contains(&segs[0].as_str()) {
                            format!("z_fnval(\"{}\", NULL)", segs.join("."))
                        } else {
                            self.emit_expr(callee)
                        }
                    } else {
                        self.emit_expr(callee)
                    }
                }
            };
            (callable, pos_c, n)
        } else {
            // spawn f — fn value expression
            (self.emit_expr(e), "(ZVal[]){0}".into(), 0)
        }
    }

    // ---------- lambdas ----------

    fn emit_lambda_def(&mut self, lid: usize, body: &[Stmt]) {
        // lambdaN(env) { event param; body }
        let fname = format!("zf_lambda{lid}");
        let mut s = String::new();
        let _ = writeln!(s, "\nstatic ZVal {fname}(ZEnv *cap, ZVal *args, int nargs, int supplied) {{");
        s.push_str("  ZEnv *env = z_env_new(cap ? cap : z_global);\n");
        s.push_str("  ZVal z_ret = ZNOTHING;\n");
        s.push_str("  z_env_define(env, \"event\", (0 < nargs) ? args[0] : ZNOTHING);\n");
        s.push_str("  { ZVec *_r = z_vec_new(); for (int _i = 1; _i < nargs; _i++) z_vec_push(_r, args[_i]); z_env_define(env, \"args\", z_args_record(supplied, _r, 1)); }\n");
        let saved_locals = std::mem::take(&mut self.locals);
        let saved_labels = std::mem::take(&mut self.loop_labels);
        let saved_tmp = self.tmp_n;
        let saved_loop = self.loop_n;
        let saved_cur = self.cur_fn_q.clone();
        self.locals = ["event".to_string(), "args".to_string()].into_iter().collect();
        collect_decl_names(body, &mut self.locals);
        self.loop_labels = Vec::new();
        self.tmp_n = 0;
        self.loop_n = 0;
        self.cur_fn_q = format!("lambda{lid}");
        for st in body {
            self.emit_stmt(&mut s, st, 1);
        }
        let _ = writeln!(s, "z_done: return z_ret; }}\n");
        self.lambda_defs.push_str(&s);
        self.locals = saved_locals;
        self.loop_labels = saved_labels;
        self.tmp_n = saved_tmp;
        self.loop_n = saved_loop;
        self.cur_fn_q = saved_cur;
    }

    // ---------- types ----------

    fn emit_typeval(&mut self, t: &TypeExpr) -> String {
        match t {
            TypeExpr::Named(n) => {
                if PRIM_TYPES.contains(&n.as_str()) || n == "List" || n == "Map" {
                    format!("z_type_prim(\"{}\")", n)
                } else {
                    format!("z_env_get(env, \"{}\")", cstr(n))
                }
            }
            TypeExpr::App { name, args } => {
                let inner: Vec<String> = args.iter().map(|a| self.emit_typeval(a)).collect();
                format!(
                    "z_type_generic(\"{}\", (ZVal[]){{{}}}, {})",
                    name,
                    inner.join(","),
                    inner.len()
                )
            }
            TypeExpr::Tuple(ts) => {
                let inner: Vec<String> = ts.iter().map(|a| self.emit_typeval(a)).collect();
                format!("z_type_tuple((ZVal[]){{{}}}, {})", inner.join(","), inner.len())
            }
            TypeExpr::Union(vs) => {
                let inner: Vec<String> = vs.iter().map(|a| self.emit_typeval(a)).collect();
                format!("z_type_union((ZVal[]){{{}}}, {})", inner.join(","), inner.len())
            }
            TypeExpr::Ref(n) => format!("z_type_ref(\"{}\")", cstr(n)),
            TypeExpr::Null => "z_type_prim(\"null\")".into(),
            TypeExpr::Any => "z_type_prim(\"any\")".into(),
        }
    }
}

// ---------- free helpers ----------

/// `mod.fn` → `zf_mod__fn`; `lambda3` → `zf_lambda3` handled separately.
fn mangled(q: &str) -> String {
    let mut out = String::from("zf_");
    for c in q.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    out
}

/// Escape a string for a C string literal.
fn cstr(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'"' => out.push_str("\\\""),
            b'\n' => out.push_str("\\n"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            0x20..=0x7e => out.push(b as char),
            _ => {
                let _ = write!(out, "\\{:03o}", b);
            }
        }
    }
    out
}

/// Fuse use-path segments: ["gui","widgets"] → "guiWidgets" (§12.1).
fn fuse(parts: &[String]) -> String {
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i == 0 {
            out.push_str(&p.to_lowercase());
        } else if let Some(f) = p.chars().next() {
            out.push(f.to_ascii_uppercase());
            out.push_str(&p[f.len_utf8()..]);
        }
    }
    out
}

/// Pre-scan: count Expr::Callback nodes in traversal order (matches emit).
fn count_callbacks(body: &[Stmt], n: &mut usize) {
    for s in body {
        count_callbacks_stmt(s, n);
    }
}

fn count_callbacks_stmt(s: &Stmt, n: &mut usize) {
    match s {
        Stmt::Decl { init, .. } => count_callbacks_expr(init, n),
        Stmt::DeclTuple { init, .. } => count_callbacks_expr(init, n),
        Stmt::Reassign { value, .. } => {
            if let Some(v) = value {
                count_callbacks_expr(v, n);
            }
        }
        Stmt::ReassignField { value, .. } => count_callbacks_expr(value, n),
        Stmt::Expr(e) => count_callbacks_expr(e, n),
        Stmt::If { cond, then_body, else_body, .. } => {
            count_callbacks_expr(cond, n);
            for s in then_body {
                count_callbacks_stmt(s, n);
            }
            if let Some(eb) = else_body {
                for s in eb {
                    count_callbacks_stmt(s, n);
                }
            }
        }
        Stmt::ForIn { iter, body, .. }
        | Stmt::ForCond { cond: iter, body, .. } => {
            count_callbacks_expr(iter, n);
            for s in body {
                count_callbacks_stmt(s, n);
            }
        }
        Stmt::ForInfinite { body, .. } => {
            for s in body {
                count_callbacks_stmt(s, n);
            }
        }
        Stmt::ForCStyle { init, cond, post, body, .. } => {
            for s in init.iter().chain(post.iter()) {
                count_callbacks_stmt(s, n);
            }
            count_callbacks_expr(cond, n);
            for s in body {
                count_callbacks_stmt(s, n);
            }
        }
        Stmt::Match { expr, arms, .. } => {
            count_callbacks_expr(expr, n);
            for a in arms {
                for s in &a.body {
                    count_callbacks_stmt(s, n);
                }
            }
        }
        Stmt::Return(e) | Stmt::Spawn(e) => count_callbacks_expr(e, n),
        Stmt::Parallel { body, .. } => {
            for s in body {
                count_callbacks_stmt(s, n);
            }
        }
        Stmt::Break | Stmt::Continue | Stmt::Use(_) => {}
    }
}

fn count_callbacks_expr(e: &Expr, n: &mut usize) {
    match e {
        Expr::Callback { call, body } => {
            *n += 1;
            count_callbacks_expr(call, n);
            for s in body {
                count_callbacks_stmt(s, n);
            }
        }
        Expr::Call { callee, args } => {
            count_callbacks_expr(callee, n);
            for a in args {
                count_callbacks_expr(a, n);
            }
        }
        Expr::Str(parts) => {
            for p in parts {
                if let StrPart::Interp(ie) = p {
                    count_callbacks_expr(ie, n);
                }
            }
        }
        Expr::Tuple(es) | Expr::List(es) => {
            for e in es {
                count_callbacks_expr(e, n);
            }
        }
        Expr::Map(kvs) => {
            for (k, v) in kvs {
                count_callbacks_expr(k, n);
                count_callbacks_expr(v, n);
            }
        }
        Expr::Field { obj, .. } => count_callbacks_expr(obj, n),
        Expr::Unary { expr, .. } => count_callbacks_expr(expr, n),
        Expr::Binary { l, r, .. } => {
            count_callbacks_expr(l, n);
            count_callbacks_expr(r, n);
        }
        Expr::Cast { expr, .. } | Expr::CountOf(expr) => count_callbacks_expr(expr, n),
        Expr::DeclArg { .. } | Expr::RecordType { .. } => {}
        Expr::NamedArg { value, .. } => count_callbacks_expr(value, n),
        _ => {}
    }
}

/// Collect names declared anywhere in a fn body (for call-vs-local).
fn collect_decl_names(body: &[Stmt], out: &mut HashSet<String>) {
    for s in body {
        match s {
            Stmt::Decl { name, .. } => {
                out.insert(name.clone());
            }
            Stmt::DeclTuple { names, .. } => {
                for n in names {
                    out.insert(n.clone());
                }
            }
            Stmt::If { then_body, else_body, .. } => {
                collect_decl_names(then_body, out);
                if let Some(eb) = else_body {
                    collect_decl_names(eb, out);
                }
            }
            Stmt::ForIn { pat, body, .. } => {
                match pat {
                    ForPat::Single(n) => {
                        out.insert(n.clone());
                    }
                    ForPat::Tuple(ns) => {
                        for n in ns {
                            out.insert(n.clone());
                        }
                    }
                }
                collect_decl_names(body, out);
            }
            Stmt::ForCond { body, .. }
            | Stmt::ForInfinite { body, .. }
            | Stmt::Parallel { body, .. } => collect_decl_names(body, out),
            Stmt::ForCStyle { init, body, .. } => {
                collect_decl_names(init, out);
                collect_decl_names(body, out);
            }
            Stmt::Match { arms, .. } => {
                for a in arms {
                    collect_decl_names(&a.body, out);
                }
            }
            _ => {}
        }
    }
}

const RUNTIME_C: &str = include_str!("zaprt.c");

const MAIN_C: &str = r#"
// ===== entry =====
int main(int argc, char **argv) {
  (void)argc; (void)argv;
  z_global = z_env_new(NULL);
  z_env_define(z_global, "thisShell", z_shell_value());
  z_zap_init();
  ZVal _r = z_call_qname("main.main", NULL, 0, z_global);
  (void)_r;
  fflush(stdout);
  return 0;
}
"#;

/// Emit zapYaml map literal + any cli registration from config.vals.
fn zap_init(project: &Project) -> String {
    let mut s = String::new();
    s.push_str("static void z_zap_init(void) {\n");
    s.push_str("// zapYaml from Zappr.yaml val:\n");
    if !project.config.vals.is_empty() {
        let expr = yval_c(&crate::config::YVal::Map(project.config.vals.clone()));
        s.push_str(&format!(
            "  z_env_define(z_global, \"zapYaml\", {expr});\n"
        ));
    } else {
        s.push_str("  z_env_define(z_global, \"zapYaml\", z_map_new());\n");
    }
    s.push_str("}\n");
    s
}

fn yval_c(v: &crate::config::YVal) -> String {
    use crate::config::YVal::*;
    match v {
        Str(s) => format!("z_str(\"{}\")", cstr(s)),
        Int(i) => format!("z_int({i}LL)"),
        Float(f) => format!("z_float({f})"),
        Bool(b) => format!("z_bool({})", if *b { 1 } else { 0 }),
        Null => "ZNOTHING".into(),
        Seq(items) => {
            let inner: Vec<String> = items.iter().map(yval_c).collect();
            format!("z_listv((ZVal[]){{{}}}, {})", inner.join(","), inner.len())
        }
        Map(m) => {
            let inner: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{{z_str(\"{}\"), {}}}", cstr(k), yval_c(v)))
                .collect();
            format!("z_mapv((ZPair[]){{{}}}, {})", inner.join(","), inner.len())
        }
    }
}

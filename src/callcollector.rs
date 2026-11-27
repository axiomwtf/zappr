//! Call collector: transitive closure of calls — the whole inclusion story
//! (SPEC §12). A module's code enters the binary iff it is reachable from
//! `main` through calls; `use` only opens a namespace, it doesn't include.

use crate::parser::{Expr, Item, Program, Stmt, StrPart};
use std::collections::{HashMap, HashSet, VecDeque};

/// Qualified name of every fn in the program set: `module.name`.
#[derive(Debug, Default)]
pub struct CallGraph {
    /// caller qualified name → callee names it may invoke
    pub edges: HashMap<String, HashSet<String>>,
    /// all fn qualified names
    pub fns: HashSet<String>,
    /// module → its fn names
    pub by_module: HashMap<String, Vec<String>>,
}

impl CallGraph {
    pub fn build<'a>(programs: impl Iterator<Item = (&'a str, &'a Program)>) -> CallGraph {
        let programs: Vec<(&'a str, &'a Program)> = programs.collect();
        let mut g = CallGraph::default();
        for &(module, prog) in &programs {
            g.by_module.entry(module.to_string()).or_default();
            for item in &prog.items {
                if let Item::Fn { name, .. } = item {
                    let q = format!("{module}.{name}");
                    g.fns.insert(q.clone());
                    g.by_module.get_mut(module).unwrap().push(name.clone());
                }
            }
        }
        for &(module, prog) in &programs {
            for item in &prog.items {
                if let Item::Fn { name, body, .. } = item {
                    let q = format!("{module}.{name}");
                    let mut callees = HashSet::new();
                    for s in body {
                        collect_stmt_calls(s, module, &mut callees);
                    }
                    g.edges.insert(q, callees);
                }
            }
        }
        g
    }

    /// Transitive closure from `main.main` — the included set (§12.2).
    pub fn reachable_from_main(&self) -> HashSet<String> {
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        for key in ["main.main", "main"] {
            if self.fns.contains(key) {
                queue.push_back(key.to_string());
            }
        }
        // bare `main` fallback
        if seen.is_empty() && self.fns.iter().any(|f| f.ends_with(".main")) {
            for f in &self.fns {
                if f.ends_with(".main") {
                    queue.push_back(f.clone());
                }
            }
        }
        while let Some(q) = queue.pop_front() {
            if !seen.insert(q.clone()) {
                continue;
            }
            if let Some(callees) = self.edges.get(&q) {
                for c in callees {
                    // resolve bare callee names within the caller's module first
                    let resolved = if self.fns.contains(c) {
                        Some(c.clone())
                    } else {
                        let caller_module = q.rsplit_once('.').map(|(m, _)| m).unwrap_or("");
                        let cand = format!("{caller_module}.{c}");
                        if self.fns.contains(&cand) {
                            Some(cand)
                        } else {
                            // global bare-name fallback: unique fn with that name
                            let matches: Vec<_> = self
                                .fns
                                .iter()
                                .filter(|f| f.rsplit('.').next() == Some(c.as_str()))
                                .cloned()
                                .collect();
                            if matches.len() == 1 {
                                Some(matches[0].clone())
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(r) = resolved {
                        if !seen.contains(&r) {
                            queue.push_back(r);
                        }
                    }
                }
            }
        }
        seen
    }

    /// Modules with at least one reachable fn — these get compiled (§12.2).
    pub fn included_modules(&self) -> HashSet<String> {
        let reach = self.reachable_from_main();
        reach
            .iter()
            .filter_map(|q| q.rsplit_once('.').map(|(m, _)| m.to_string()))
            .collect()
    }
}

fn collect_stmt_calls(s: &Stmt, module: &str, out: &mut HashSet<String>) {
    match s {
        Stmt::Decl { init, .. } => collect_expr_calls(init, module, out),
        Stmt::DeclTuple { init, .. } => collect_expr_calls(init, module, out),
        Stmt::Reassign { value, .. } => {
            if let Some(v) = value {
                collect_expr_calls(v, module, out);
            }
        }
        Stmt::ReassignField { value, .. } => collect_expr_calls(value, module, out),
        Stmt::Expr(e) => collect_expr_calls(e, module, out),
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            collect_expr_calls(cond, module, out);
            for s in then_body {
                collect_stmt_calls(s, module, out);
            }
            if let Some(eb) = else_body {
                for s in eb {
                    collect_stmt_calls(s, module, out);
                }
            }
        }
        Stmt::ForIn { iter, body, .. }
        | Stmt::ForCond { cond: iter, body, .. } => {
            collect_expr_calls(iter, module, out);
            for s in body {
                collect_stmt_calls(s, module, out);
            }
        }
        Stmt::ForInfinite { body, .. } => {
            for s in body {
                collect_stmt_calls(s, module, out);
            }
        }
        Stmt::ForCStyle {
            init,
            cond,
            post,
            body,
            ..
        } => {
            for s in init.iter().chain(post.iter()) {
                collect_stmt_calls(s, module, out);
            }
            collect_expr_calls(cond, module, out);
            for s in body {
                collect_stmt_calls(s, module, out);
            }
        }
        Stmt::Match { expr, arms, .. } => {
            collect_expr_calls(expr, module, out);
            for a in arms {
                for s in &a.body {
                    collect_stmt_calls(s, module, out);
                }
            }
        }
        Stmt::Return(e) => collect_expr_calls(e, module, out),
        Stmt::Parallel { body, .. } => {
            for s in body {
                collect_stmt_calls(s, module, out);
            }
        }
        Stmt::Spawn(e) => collect_expr_calls(e, module, out),
        Stmt::Break | Stmt::Continue | Stmt::Use(_) => {}
    }
}

fn collect_expr_calls(e: &Expr, module: &str, out: &mut HashSet<String>) {
    match e {
        Expr::Str(parts) => {
            for p in parts {
                if let StrPart::Interp(ie) = p {
                    collect_expr_calls(ie, module, out);
                }
            }
        }
        Expr::Tuple(es) | Expr::List(es) => {
            for e in es {
                collect_expr_calls(e, module, out);
            }
        }
        Expr::Map(kvs) => {
            for (k, v) in kvs {
                collect_expr_calls(k, module, out);
                collect_expr_calls(v, module, out);
            }
        }
        Expr::Call { callee, args } => {
            // callee ident or field path `ns.fn` → record the leaf name;
            // resolution happens in reachable_from_main
            match callee.as_ref() {
                Expr::Ident(n) => {
                    out.insert(n.clone());
                }
                Expr::Field { name, .. } => {
                    out.insert(name.clone());
                }
                _ => collect_expr_calls(callee, module, out),
            }
            for a in args {
                collect_expr_calls(a, module, out);
            }
        }
        Expr::Ident(n) => {
            // fn-as-value: passing `double` to map() still calls it (§12.2)
            out.insert(n.clone());
        }
        Expr::Field { obj, .. } => collect_expr_calls(obj, module, out),
        Expr::Unary { expr, .. } => collect_expr_calls(expr, module, out),
        Expr::Binary { l, r, .. } => {
            collect_expr_calls(l, module, out);
            collect_expr_calls(r, module, out);
        }
        Expr::Cast { expr, .. } => collect_expr_calls(expr, module, out),
        Expr::CountOf(inner) => collect_expr_calls(inner, module, out),
        Expr::DeclArg { .. } => {}
        Expr::NamedArg { value, .. } => collect_expr_calls(value, module, out),
        Expr::RecordType { .. } => {}
        Expr::Callback { call, body } => {
            collect_expr_calls(call, module, out);
            for s in body {
                collect_stmt_calls(s, module, out);
            }
        }
        _ => {}
    }
    let _ = module;
}

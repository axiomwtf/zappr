//! Parser: tokens to AST (SPEC §4).

use crate::lexer::{Tok, Token};

#[derive(Debug, Clone)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub enum Item {
    /// `name(a, b) { ... }`
    Fn {
        name: String,
        params: Vec<Param>,
        body: Vec<Stmt>,
        line: usize,
    },
    /// top-level `use.foo.bar`
    Use(Vec<String>),
    /// top-level `!Name = expr` declarations — record type definitions,
    /// union defs, consts (SPEC §7.6 shows them at module top level)
    TopDecl(Vec<Stmt>),
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    /// optional type annotation `name: Type`
    pub ty: Option<TypeExpr>,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `!name [: Type] = expr`
    Decl {
        name: String,
        ty: Option<TypeExpr>,
        init: Expr,
        line: usize,
    },
    /// `!(a, b) = expr` — tuple destructure declaration
    DeclTuple {
        names: Vec<String>,
        init: Expr,
        line: usize,
    },
    /// `/name = expr`, `/name+`, `/name-`
    Reassign {
        target: String,
        op: ReassignOp,
        value: Option<Expr>,
        line: usize,
    },
    /// `/obj.field = expr` etc. — member reassignment
    ReassignField {
        obj: String,
        field: String,
        value: Expr,
        line: usize,
    },
    Expr(Expr),
    If {
        cond: Expr,
        bind: Option<String>,
        then_body: Vec<Stmt>,
        else_body: Option<Vec<Stmt>>,
        else_bind: Option<String>,
        line: usize,
    },
    ForIn {
        pat: ForPat,
        iter: Expr,
        body: Vec<Stmt>,
        line: usize,
    },
    ForCond {
        cond: Expr,
        body: Vec<Stmt>,
        line: usize,
    },
    ForInfinite {
        body: Vec<Stmt>,
        line: usize,
    },
    ForCStyle {
        init: Vec<Stmt>,
        cond: Expr,
        post: Vec<Stmt>,
        body: Vec<Stmt>,
        line: usize,
    },
    Match {
        expr: Expr,
        arms: Vec<MatchArm>,
        line: usize,
    },
    Return(Expr),
    /// `parallel { ... }`
    Parallel {
        body: Vec<Stmt>,
        line: usize,
    },
    /// `spawn f(args)`
    Spawn(Expr),
    Break,
    Continue,
    /// `use.foo.bar` — opens a namespace (SPEC §12.1)
    Use(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReassignOp {
    Set,
    Inc,
    Dec,
}

#[derive(Debug, Clone)]
pub enum ForPat {
    Single(String),
    Tuple(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    /// type expression or `else`
    pub ty: Option<TypeExpr>,
    /// `as name` binding
    pub bind: Option<String>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64),
    Float(f64),
    Str(Vec<StrPart>),
    Bool(bool),
    Null,
    Ident(String),
    Tuple(Vec<Expr>),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    Field {
        obj: Box<Expr>,
        name: String,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        l: Box<Expr>,
        r: Box<Expr>,
    },
    /// `expr as Type` cast
    Cast {
        expr: Box<Expr>,
        ty: TypeExpr,
    },
    /// `$(expr)` — count-loop marker inside `for i in $(n)`
    CountOf(Box<Expr>),
    /// `read(int age)` — declare-and-read argument (§9.4)
    DeclArg { ty: String, var: String },
    /// `User(name: "ada")` — named argument in a construction call
    NamedArg { name: String, value: Box<Expr> },
    /// `(name: str, age: int)` — record type literal, a type value (§7.6)
    RecordType { fields: Vec<(String, TypeExpr)> },
    /// `f(x) { ... }` — call with trailing callback block (onClick etc.)
    Callback { call: Box<Expr>, body: Vec<Stmt> },
}

#[derive(Debug, Clone)]
pub enum StrPart {
    Lit(String),
    Interp(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    And,
    Or,
}

/// Type expressions — types are values (SPEC §7.4).
#[derive(Debug, Clone)]
pub enum TypeExpr {
    Named(String),
    /// `List(int)`, `Map(str, User)` — type application, generics are calls
    App { name: String, args: Vec<TypeExpr> },
    Tuple(Vec<TypeExpr>),
    Union(Vec<TypeExpr>),
    Ref(String), // &str
    Null,
    Any, // inferred / wildcard
}

#[derive(Debug, Clone)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// inside an `if` condition: `as name` is a binding, not a cast
    in_if_cond: bool,
}

impl Parser {
    pub fn new(toks: Vec<Token>) -> Self {
        Parser {
            toks,
            pos: 0,
            in_if_cond: false,
        }
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].tok
    }

    fn peek_tok(&self) -> &Token {
        &self.toks[self.pos.min(self.toks.len() - 1)]
    }

    fn peek_n(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }

    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, ParseError> {
        let t = self.peek_tok();
        Err(ParseError {
            line: t.line,
            col: t.col,
            msg: msg.into(),
        })
    }

    fn at_line_start(&self) -> bool {
        // true if the next token is on a later line than the previous token
        if self.pos == 0 {
            return true;
        }
        let prev = &self.toks[self.pos - 1];
        let cur = self.peek_tok();
        cur.line > prev.line
    }

    fn expect(&mut self, tok: &Tok, what: &str) -> Result<(), ParseError> {
        if self.peek() == tok {
            self.bump();
            Ok(())
        } else {
            self.err(format!("expected {what}, found {:?}", self.peek()))
        }
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == tok {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Statements end at newline or `;`. Expressions may continue on the
    /// next line only while an operator/delimiter demands it.
    fn stmt_boundary(&self) -> bool {
        match self.peek() {
            Tok::Eof | Tok::RBrace | Tok::Semi => true,
            _ => self.at_line_start(),
        }
    }

    pub fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut items = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            // skip stray semicolons/newlines
            if self.eat(&Tok::Semi) {
                continue;
            }
            if matches!(self.peek(), Tok::KwUse) {
                // top-level use statement
                if let Stmt::Use(path) = self.parse_stmt()? {
                    items.push(Item::Use(path));
                    continue;
                }
            }
            // top-level `!name = ...` declarations (§7.6)
            if matches!(self.peek(), Tok::Bang) {
                let mut decls = Vec::new();
                while !matches!(self.peek(), Tok::Eof) {
                    if self.eat(&Tok::Semi) {
                        continue;
                    }
                    if !matches!(self.peek(), Tok::Bang) {
                        break;
                    }
                    match self.parse_stmt()? {
                        s @ Stmt::Decl { .. } | s @ Stmt::DeclTuple { .. } => {
                            decls.push(s)
                        }
                        _ => {
                            return self
                                .err("expected a declaration after `!` at top level")
                        }
                    }
                    self.eat(&Tok::Semi);
                }
                items.push(Item::TopDecl(decls));
                continue;
            }
            items.push(self.parse_fn()?);
        }
        Ok(Program { items })
    }

    fn parse_fn(&mut self) -> Result<Item, ParseError> {
        let name_tok = self.bump();
        let name = match name_tok.tok.clone() {
            Tok::Ident(s) => s,
            Tok::KwMain => "main".to_string(),
            _ => {
                return Err(ParseError {
                    line: name_tok.line,
                    col: name_tok.col,
                    msg: format!("expected function name, found {:?}", name_tok.tok),
                })
            }
        };
        self.expect(&Tok::LParen, "`(` after function name")?;
        let mut params = Vec::new();
        while !self.eat(&Tok::RParen) {
            let ptok = self.bump();
            let Tok::Ident(pname) = ptok.tok.clone() else {
                return Err(ParseError {
                    line: ptok.line,
                    col: ptok.col,
                    msg: format!("expected parameter name, found {:?}", ptok.tok),
                });
            };
            // optional `: Type` annotation
            let mut ty = None;
            if self.eat(&Tok::Colon) {
                ty = Some(self.parse_type()?);
            }
            params.push(Param {
                name: pname,
                ty,
            });
            if !self.eat(&Tok::Comma) {
                self.expect(&Tok::RParen, "`,` or `)` in parameter list")?;
                break;
            }
        }
        let body = self.parse_block()?;
        Ok(Item::Fn {
            name,
            params,
            body,
            line: name_tok.line,
        })
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, ParseError> {
        self.expect(&Tok::LBrace, "`{`")?;
        let mut body = Vec::new();
        while !self.eat(&Tok::RBrace) {
            if matches!(self.peek(), Tok::Eof) {
                return self.err("unterminated block — expected `}`");
            }
            if self.eat(&Tok::Semi) {
                continue;
            }
            body.push(self.parse_stmt()?);
            // consume optional semicolon / newline handled by stmt_boundary
            self.eat(&Tok::Semi);
        }
        Ok(body)
    }

    pub fn parse_stmts_until_eof(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut body = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            if self.eat(&Tok::Semi) {
                continue;
            }
            body.push(self.parse_stmt()?);
            self.eat(&Tok::Semi);
        }
        Ok(body)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        // `val.field = expr` — the §19.2 build-privilege write, no `/`
        if matches!(self.peek(), Tok::Ident(s) if s == "val")
            && matches!(self.peek_n(1), Tok::Dot)
            && matches!(self.peek_n(2), Tok::Ident(_))
            && matches!(self.peek_n(3), Tok::Assign)
        {
            self.bump(); // val
            self.bump(); // .
            let ftok = self.bump();
            let Tok::Ident(field) = ftok.tok.clone() else {
                unreachable!("checked above")
            };
            self.bump(); // =
            let value = self.parse_expr()?;
            return Ok(Stmt::ReassignField {
                obj: "val".into(),
                field,
                value,
                line: ftok.line,
            });
        }
        let tok = self.peek_tok().clone();
        match &tok.tok {
            Tok::KwReturn => {
                self.bump();
                if self.stmt_boundary() {
                    return Ok(Stmt::Return(Expr::Null));
                }
                let e = self.parse_expr()?;
                Ok(Stmt::Return(e))
            }
            Tok::KwIf => self.parse_if(),
            Tok::KwFor => self.parse_for(),
            Tok::KwMatch => self.parse_match(),
            Tok::KwParallel => {
                self.bump();
                let body = self.parse_block()?;
                Ok(Stmt::Parallel {
                    body,
                    line: tok.line,
                })
            }
            Tok::KwSpawn => {
                self.bump();
                let e = self.parse_expr()?;
                Ok(Stmt::Spawn(e))
            }
            Tok::KwBreak => {
                self.bump();
                Ok(Stmt::Break)
            }
            Tok::KwContinue => {
                self.bump();
                Ok(Stmt::Continue)
            }
            Tok::KwUse => {
                self.bump();
                let mut path = Vec::new();
                // use.foo.bar — fused namespace name segments; stops at newline
                loop {
                    if self.at_line_start() && !path.is_empty() {
                        break;
                    }
                    match self.peek() {
                        Tok::Dot => {
                            self.bump();
                        }
                        Tok::Ident(s) => {
                            path.push(s.clone());
                            self.bump();
                        }
                        _ => break,
                    }
                }
                if path.is_empty() {
                    return self.err("expected namespace path after `use`");
                }
                Ok(Stmt::Use(path))
            }
            Tok::Bang => self.parse_decl_or_expr(),
            Tok::Slash => self.parse_reassign(),
            _ => {
                let e = self.parse_expr()?;
                Ok(Stmt::Expr(e))
            }
        }
    }

    /// `!name [: Type] = expr` or `!(a, b, ...) = expr` — declaration.
    fn parse_decl_or_expr(&mut self) -> Result<Stmt, ParseError> {
        let bang = self.bump(); // !
        match self.peek() {
            Tok::Ident(name) => {
                let name = name.clone();
                self.bump();
                // optional type annotation
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                // expect = (or += style? spec: !x = 5)
                if !self.eat(&Tok::Assign) {
                    return Err(ParseError {
                        line: bang.line,
                        col: bang.col,
                        msg: "expected `=` after declaration".into(),
                    });
                }
                let init = self.parse_expr()?;
                Ok(Stmt::Decl {
                    name,
                    ty,
                    init,
                    line: bang.line,
                })
            }
            Tok::LParen => {
                // !(a, b) = expr — tuple destructure declaration
                self.bump();
                let mut names = Vec::new();
                while !self.eat(&Tok::RParen) {
                    let t = self.bump();
                    match t.tok {
                        Tok::Ident(s) => names.push(s),
                        _ => {
                            return Err(ParseError {
                                line: t.line,
                                col: t.col,
                                msg: "expected identifier in `!(...)` destructure".into(),
                            })
                        }
                    }
                    self.eat(&Tok::Comma);
                }
                if !self.eat(&Tok::Assign) {
                    return Err(ParseError {
                        line: bang.line,
                        col: bang.col,
                        msg: "expected `=` after `!(...)`".into(),
                    });
                }
                let init = self.parse_expr()?;
                Ok(Stmt::DeclTuple {
                    names,
                    init,
                    line: bang.line,
                })
            }
            _ => {
                // unary not expression: !expr — but `!` as statement start with
                // non-ident is unusual; treat as expression statement.
                let inner = self.parse_unary_rest()?;
                Ok(Stmt::Expr(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(inner),
                }))
            }
        }
    }

    fn parse_reassign(&mut self) -> Result<Stmt, ParseError> {
        let slash = self.bump(); // /
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.bump();
                // /x+ /x- increments
                if matches!(self.peek(), Tok::Plus | Tok::Minus) {
                    let inc = matches!(self.peek(), Tok::Plus);
                    self.bump();
                    // optional `= expr`? spec: /x+ is complete. But allow /x+ = e? no.
                    return Ok(Stmt::Reassign {
                        target: name,
                        op: if inc {
                            ReassignOp::Inc
                        } else {
                            ReassignOp::Dec
                        },
                        value: None,
                        line: slash.line,
                    });
                }
                // field reassign: /obj.field = expr
                if matches!(self.peek(), Tok::Dot) {
                    self.bump();
                    let ftok = self.bump();
                    let Tok::Ident(field) = ftok.tok.clone() else {
                        return Err(ParseError {
                            line: ftok.line,
                            col: ftok.col,
                            msg: "expected field name after `.`".into(),
                        });
                    };
                    if !self.eat(&Tok::Assign) {
                        return Err(ParseError {
                            line: slash.line,
                            col: slash.col,
                            msg: "expected `=` after `/name.field`".into(),
                        });
                    }
                    let value = self.parse_expr()?;
                    return Ok(Stmt::ReassignField {
                        obj: name,
                        field,
                        value,
                        line: slash.line,
                    });
                }
                if !self.eat(&Tok::Assign) {
                    return Err(ParseError {
                        line: slash.line,
                        col: slash.col,
                        msg: "expected `=` after `/name`".into(),
                    });
                }
                let value = self.parse_expr()?;
                Ok(Stmt::Reassign {
                    target: name,
                    op: ReassignOp::Set,
                    value: Some(value),
                    line: slash.line,
                })
            }
            _ => self.err("expected identifier after `/`"),
        }
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        let iftok = self.bump(); // if
        self.in_if_cond = true;
        let cond = self.parse_expr();
        self.in_if_cond = false;
        let cond = cond?;
        // `if expr as name {` — fnType narrowing bind
        let bind = if matches!(self.peek(), Tok::KwAs) {
            self.bump();
            let btok = self.bump();
            match btok.tok {
                Tok::Ident(s) => Some(s),
                _ => {
                    return Err(ParseError {
                        line: btok.line,
                        col: btok.col,
                        msg: "expected binding name after `as`".into(),
                    })
                }
            }
        } else {
            None
        };
        let then_body = self.parse_block()?;
        let mut else_body = None;
        let mut else_bind = None;
        if matches!(self.peek(), Tok::KwElse) {
            self.bump();
            if matches!(self.peek(), Tok::KwAs) {
                self.bump();
                let btok = self.bump();
                match btok.tok {
                    Tok::Ident(s) => else_bind = Some(s),
                    _ => {
                        return Err(ParseError {
                            line: btok.line,
                            col: btok.col,
                            msg: "expected binding name after `else as`".into(),
                        })
                    }
                }
            }
            if matches!(self.peek(), Tok::KwIf) {
                // else if — treat as nested if statement
                let nested = self.parse_if()?;
                else_body = Some(vec![nested]);
            } else {
                else_body = Some(self.parse_block()?);
            }
        }
        Ok(Stmt::If {
            cond,
            bind,
            then_body,
            else_body,
            else_bind,
            line: iftok.line,
        })
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        let fortok = self.bump(); // for
        match self.peek().clone() {
            // for { ... } infinite
            Tok::LBrace => {
                let body = self.parse_block()?;
                Ok(Stmt::ForInfinite {
                    body,
                    line: fortok.line,
                })
            }
            // for i in $(expr) { }
            // for (x, y) in z { }
            Tok::LParen | Tok::Ident(_) | Tok::Dollar => {
                // could be: for cond { } | for i in $(n) | for (a,b) in it | for (!i=0;...)
                if matches!(self.peek(), Tok::LParen) && self.looks_like_cstyle() {
                    return self.parse_for_cstyle(fortok.line);
                }
                if matches!(self.peek(), Tok::Dollar) {
                    // for $ ... not valid; count form is `for i in $(n)`
                    return self.err("unexpected `$` after `for`");
                }
                // try `for cond {` vs `for pat in iter {`
                // peek ahead for `in` keyword before `{` at depth 0
                if self.scan_for_in() {
                    return self.parse_for_in(fortok.line);
                }
                // while form
                let cond = self.parse_expr()?;
                let body = self.parse_block()?;
                Ok(Stmt::ForCond {
                    cond,
                    body,
                    line: fortok.line,
                })
            }
            _ => self.err("expected `for` loop form"),
        }
    }

    /// Look ahead: does this `for` header contain `in` before the body `{`?
    fn scan_for_in(&self) -> bool {
        let mut i = self.pos;
        let mut depth = 0i32;
        while i < self.toks.len() {
            match &self.toks[i].tok {
                Tok::KwIn if depth == 0 => return true,
                Tok::LBrace if depth == 0 => return false,
                Tok::LParen | Tok::LBracket => depth += 1,
                Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket | Tok::RBrace => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                }
                Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    /// `for (!i = 0; i < n; /i+) { }` — header obeys the !/ / law.
    fn looks_like_cstyle(&self) -> bool {
        // `for (` followed by `!` or `/` or ident then `;` before matching `)`
        if !matches!(self.peek(), Tok::LParen) {
            return false;
        }
        // scan inside parens for `;` at depth 1
        let mut i = self.pos + 1;
        let mut depth = 1i32;
        while i < self.toks.len() {
            match &self.toks[i].tok {
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket | Tok::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        return false;
                    }
                }
                Tok::Semi if depth == 1 => return true,
                Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_for_cstyle(&mut self, line: usize) -> Result<Stmt, ParseError> {
        self.expect(&Tok::LParen, "`(`")?;
        // init: declarations/reassignments separated by ; — parse as stmts
        let mut init = Vec::new();
        while !self.eat(&Tok::Semi) {
            if matches!(self.peek(), Tok::Eof | Tok::RParen) {
                return self.err("unterminated C-style for header");
            }
            init.push(self.parse_stmt()?);
        }
        let cond = self.parse_expr()?;
        self.expect(&Tok::Semi, "`;` in for header")?;
        let mut post = Vec::new();
        while !self.eat(&Tok::RParen) {
            if matches!(self.peek(), Tok::Eof) {
                return self.err("unterminated C-style for header");
            }
            post.push(self.parse_stmt()?);
        }
        let body = self.parse_block()?;
        Ok(Stmt::ForCStyle {
            init,
            cond,
            post,
            body,
            line,
        })
    }

    /// `for i in $(expr)` or `for (x, y) in iter`
    fn parse_for_in(&mut self, line: usize) -> Result<Stmt, ParseError> {
        let pat = if matches!(self.peek(), Tok::LParen) {
            self.bump();
            let mut names = Vec::new();
            while !self.eat(&Tok::RParen) {
                let t = self.bump();
                match t.tok {
                    Tok::Ident(s) => names.push(s),
                    _ => {
                        return Err(ParseError {
                            line: t.line,
                            col: t.col,
                            msg: "expected identifier in destructure pattern".into(),
                        })
                    }
                }
                self.eat(&Tok::Comma);
            }
            ForPat::Tuple(names)
        } else {
            let t = self.bump();
            match t.tok {
                Tok::Ident(s) => ForPat::Single(s),
                _ => {
                    return Err(ParseError {
                        line: t.line,
                        col: t.col,
                        msg: "expected loop variable".into(),
                    })
                }
            }
        };
        self.expect(&Tok::KwIn, "`in`")?;
        let iter = if self.eat(&Tok::Dollar) {
            // $(expr) count-loop
            self.expect(&Tok::LParen, "`(` after `$`")?;
            let e = self.parse_expr()?;
            self.expect(&Tok::RParen, "`)`")?;
            Expr::CountOf(Box::new(e))
        } else {
            self.parse_expr()?
        };
        let body = self.parse_block()?;
        Ok(Stmt::ForIn {
            pat,
            iter,
            body,
            line,
        })
    }

    fn parse_match(&mut self) -> Result<Stmt, ParseError> {
        let mtok = self.bump(); // match
        let expr = self.parse_expr()?;
        self.expect(&Tok::LBrace, "`{` after match expression")?;
        let mut arms = Vec::new();
        while !self.eat(&Tok::RBrace) {
            if matches!(self.peek(), Tok::Eof) {
                return self.err("unterminated match — expected `}`");
            }
            // arm: TypeExpr [as name] { body }  |  else [as name] { body }
            let (ty, bind) = if matches!(self.peek(), Tok::KwElse) {
                self.bump();
                let b = if matches!(self.peek(), Tok::KwAs) {
                    self.bump();
                    match self.bump().tok {
                        Tok::Ident(s) => Some(s),
                        _ => return self.err("expected name after `else as`"),
                    }
                } else {
                    None
                };
                (None, b)
            } else {
                let ty = self.parse_type()?;
                let b = if matches!(self.peek(), Tok::KwAs) {
                    self.bump();
                    match self.bump().tok {
                        Tok::Ident(s) => Some(s),
                        _ => return self.err("expected name after `as`"),
                    }
                } else {
                    None
                };
                (Some(ty), b)
            };
            let body = self.parse_block()?;
            arms.push(MatchArm { ty, bind, body });
        }
        Ok(Stmt::Match {
            expr,
            arms,
            line: mtok.line,
        })
    }

    // ---------- expressions ----------

    pub fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_binary(0)
    }

    fn parse_binary(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            // stop at statement boundary: newline before operator ends expr
            let (op, prec) = match self.peek() {
                Tok::EqEq => (BinOp::Eq, 1),
                Tok::NotEq => (BinOp::Ne, 1),
                Tok::Lt => (BinOp::Lt, 2),
                Tok::Gt => (BinOp::Gt, 2),
                Tok::LtEq => (BinOp::Le, 2),
                Tok::GtEq => (BinOp::Ge, 2),
                Tok::KwAnd => (BinOp::And, 1),
                Tok::KwOr | Tok::Pipe => (BinOp::Or, 0),
                Tok::Plus => (BinOp::Add, 3),
                Tok::Minus => (BinOp::Sub, 3),
                Tok::Star => (BinOp::Mul, 4),
                Tok::Percent => (BinOp::Mod, 4),
                Tok::KwAs if !self.in_if_cond => {
                    // cast: expr as Type — lowest precedence postfix.
                    // In an `if` condition a trailing `as name` is the
                    // fnType binding (§8.2), not a cast.
                    if self.at_line_start() || !self.cast_ahead() {
                        break;
                    }
                    self.bump(); // as
                    let ty = self.parse_type()?;
                    lhs = Expr::Cast {
                        expr: Box::new(lhs),
                        ty,
                    };
                    continue;
                }
                _ => break,
            };
            // operator on a new line ends the statement (no leading-dot style)
            if self.at_line_start() {
                break;
            }
            if prec < min_prec {
                break;
            }
            self.bump();
            let rhs = self.parse_binary(prec + 1)?;
            lhs = Expr::Binary {
                op,
                l: Box::new(lhs),
                r: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        match self.peek() {
            Tok::Minus => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(Expr::Unary {
                    op: UnOp::Neg,
                    expr: Box::new(e),
                })
            }
            Tok::KwNot => {
                self.bump();
                let e = self.parse_unary()?;
                Ok(Expr::Unary {
                    op: UnOp::Not,
                    expr: Box::new(e),
                })
            }
            _ => self.parse_postfix(),
        }
    }

    /// After a leading `!` that wasn't a declaration.
    fn parse_unary_rest(&mut self) -> Result<Expr, ParseError> {
        self.parse_unary()
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut e = self.parse_primary()?;
        loop {
            if self.at_line_start() {
                break;
            }
            match self.peek() {
                Tok::Dot => {
                    self.bump();
                    let t = self.bump();
                    match t.tok {
                        Tok::Ident(name) => {
                            e = Expr::Field {
                                obj: Box::new(e),
                                name,
                            };
                        }
                        Tok::Int(n) => {
                            // tuple index .0 .1
                            e = Expr::Field {
                                obj: Box::new(e),
                                name: format!("__idx_{n}"),
                            };
                        }
                        _ => {
                            return Err(ParseError {
                                line: t.line,
                                col: t.col,
                                msg: "expected field name after `.`".into(),
                            })
                        }
                    }
                }
                Tok::LParen => {
                    // call
                    let args = self.parse_args()?;
                    e = Expr::Call {
                        callee: Box::new(e),
                        args,
                    };
                    // trailing callback block: onClick(btn) { ... }
                    if matches!(self.peek(), Tok::LBrace) && !self.at_line_start() {
                        let body = self.parse_block()?;
                        e = Expr::Callback {
                            call: Box::new(e),
                            body,
                        };
                    }
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// Lookahead for `as Type`: true when what follows `as` is a type and
    /// the token after the type is not `{` (which would make `as name` a
    /// binding, e.g. `if f() as u {`).
    fn cast_ahead(&self) -> bool {
        let mut i = self.pos + 1; // skip `as`
        if i >= self.toks.len() {
            return false;
        }
        match &self.toks[i].tok {
            Tok::Ident(_) | Tok::KwNull | Tok::LParen | Tok::Amp => {}
            _ => return false,
        }
        // skip one type atom (with optional call args / tuple / & prefix)
        match &self.toks[i].tok {
            Tok::Amp => i += 2, // & Ident
            Tok::Ident(_) => {
                i += 1;
                if matches!(self.toks.get(i).map(|t| &t.tok), Some(Tok::LParen)) {
                    // type application args — skip balanced parens
                    let mut depth = 0i32;
                    while i < self.toks.len() {
                        match self.toks[i].tok {
                            Tok::LParen => depth += 1,
                            Tok::RParen => {
                                depth -= 1;
                                if depth == 0 {
                                    i += 1;
                                    break;
                                }
                            }
                            Tok::Eof => return false,
                            _ => {}
                        }
                        i += 1;
                    }
                }
            }
            Tok::LParen => {
                let mut depth = 0i32;
                while i < self.toks.len() {
                    match self.toks[i].tok {
                        Tok::LParen => depth += 1,
                        Tok::RParen => {
                            depth -= 1;
                            if depth == 0 {
                                i += 1;
                                break;
                            }
                        }
                        Tok::Eof => return false,
                        _ => {}
                    }
                    i += 1;
                }
            }
            Tok::KwNull => i += 1,
            _ => return false,
        }
        // union continuation `| T`
        while matches!(self.toks.get(i).map(|t| &t.tok), Some(Tok::Pipe)) {
            i += 1;
            if i >= self.toks.len() {
                return false;
            }
            if let Tok::Ident(_) = self.toks[i].tok {
                i += 1;
            } else {
                return false;
            }
        }
        match self.toks.get(i).map(|t| &t.tok) {
            Some(Tok::LBrace) => false, // `as name {` binding
            Some(_) => true,
            None => false,
        }
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>, ParseError> {
        self.expect(&Tok::LParen, "`(`")?;
        let saved = self.in_if_cond;
        self.in_if_cond = false;
        let args = (|| {
            let mut args = Vec::new();
            while !self.eat(&Tok::RParen) {
                // named argument: `name: expr`
                if matches!(self.peek(), Tok::Ident(_))
                    && matches!(self.peek_n(1), Tok::Colon)
                    && !self.at_line_start()
                {
                    let nt = self.bump();
                    let Tok::Ident(name) = nt.tok.clone() else {
                        unreachable!()
                    };
                    self.bump(); // :
                    let value = self.parse_expr()?;
                    args.push(Expr::NamedArg {
                        name,
                        value: Box::new(value),
                    });
                } else {
                    let e = self.parse_expr()?;
                    // declare-and-read arg: `read(int age)` — Type name
                    if let Expr::Ident(ty) = &e {
                        if matches!(self.peek(), Tok::Ident(_)) && !self.at_line_start() {
                            let nt = self.bump();
                            if let Tok::Ident(var) = nt.tok.clone() {
                                args.push(Expr::DeclArg {
                                    ty: ty.clone(),
                                    var,
                                });
                                if !self.eat(&Tok::Comma) {
                                    self.expect(&Tok::RParen, "`,` or `)` in arguments")?;
                                    break;
                                }
                                continue;
                            }
                        }
                    }
                    args.push(e);
                }
                if !self.eat(&Tok::Comma) {
                    self.expect(&Tok::RParen, "`,` or `)` in arguments")?;
                    break;
                }
            }
            Ok::<_, ParseError>(args)
        })();
        self.in_if_cond = saved;
        args
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        let t = self.bump();
        match t.tok {
            Tok::Int(s) => Ok(Expr::Int(s.parse().map_err(|_| ParseError {
                line: t.line,
                col: t.col,
                msg: format!("integer literal `{s}` out of range"),
            })?)),
            Tok::Float(s) => Ok(Expr::Float(s.parse().map_err(|_| ParseError {
                line: t.line,
                col: t.col,
                msg: format!("float literal `{s}` out of range"),
            })?)),
            Tok::Str(s) => Ok(Expr::Str(parse_interp(&s, t.line))),
            Tok::KwTrue => Ok(Expr::Bool(true)),
            Tok::KwFalse => Ok(Expr::Bool(false)),
            Tok::KwNull => Ok(Expr::Null),
            // keywords that double as value names in expression position
            Tok::KwWeak => Ok(Expr::Ident("weak".into())),
            Tok::KwChannel => Ok(Expr::Ident("channel".into())),
            Tok::KwMain => Ok(Expr::Ident("main".into())),
            Tok::Ident(s) => {
                // weak(x) and channel(T) are just calls on idents — keep as Ident;
                // channel needs a type arg though. `channel(int)` — args are exprs;
                // `int` parses as Ident expr. Fine: runtime resolves.
                Ok(Expr::Ident(s))
            }
            Tok::LParen => {
                // record type literal `(name: str, age: int)` — a type value
                if matches!(self.peek(), Tok::Ident(_))
                    && matches!(self.peek_n(1), Tok::Colon)
                {
                    let mut fields = Vec::new();
                    while !self.eat(&Tok::RParen) {
                        let nt = self.bump();
                        let Tok::Ident(fname) = nt.tok.clone() else {
                            return Err(ParseError {
                                line: nt.line,
                                col: nt.col,
                                msg: "expected field name in record type".into(),
                            });
                        };
                        self.expect(&Tok::Colon, "`:` in record type")?;
                        let fty = self.parse_type()?;
                        fields.push((fname, fty));
                        self.eat(&Tok::Comma);
                    }
                    return Ok(Expr::RecordType { fields });
                }
                // tuple or parenthesized expr — `as` inside is a cast again
                let saved = self.in_if_cond;
                self.in_if_cond = false;
                if self.eat(&Tok::RParen) {
                    self.in_if_cond = saved;
                    return Ok(Expr::Tuple(vec![]));
                }
                let first = self.parse_expr();
                let first = match first {
                    Ok(e) => e,
                    Err(e) => {
                        self.in_if_cond = saved;
                        return Err(e);
                    }
                };
                if self.eat(&Tok::Comma) {
                    let mut items = vec![first];
                    while !self.eat(&Tok::RParen) {
                        match self.parse_expr() {
                            Ok(e) => items.push(e),
                            Err(e) => {
                                self.in_if_cond = saved;
                                return Err(e);
                            }
                        }
                        self.eat(&Tok::Comma);
                    }
                    self.in_if_cond = saved;
                    Ok(Expr::Tuple(items))
                } else {
                    let r = self.expect(&Tok::RParen, "`)`");
                    self.in_if_cond = saved;
                    r?;
                    Ok(first)
                }
            }
            Tok::LBracket => {
                // list literal [] or [a, b]
                let mut items = Vec::new();
                while !self.eat(&Tok::RBracket) {
                    items.push(self.parse_expr()?);
                    self.eat(&Tok::Comma);
                }
                Ok(Expr::List(items))
            }
            Tok::LBrace => {
                // map literal {k: v, ...} or empty {}
                let mut items = Vec::new();
                while !self.eat(&Tok::RBrace) {
                    let k = self.parse_expr()?;
                    self.expect(&Tok::Colon, "`:` in map literal")?;
                    let v = self.parse_expr()?;
                    items.push((k, v));
                    self.eat(&Tok::Comma);
                }
                Ok(Expr::Map(items))
            }
            _ => Err(ParseError {
                line: t.line,
                col: t.col,
                msg: format!("unexpected token {:?}", t.tok),
            }),
        }
    }

    // ---------- types ----------

    pub fn parse_type(&mut self) -> Result<TypeExpr, ParseError> {
        let mut ty = self.parse_type_atom()?;
        // union: A | B | C → sugar for type tuple
        if matches!(self.peek(), Tok::Pipe) {
            let mut variants = vec![ty];
            while self.eat(&Tok::Pipe) {
                variants.push(self.parse_type_atom()?);
            }
            return Ok(TypeExpr::Union(variants));
        }
        // tuple type: (A, B) or (A, null)
        Ok(ty)
    }

    fn parse_type_atom(&mut self) -> Result<TypeExpr, ParseError> {
        let t = self.bump();
        match t.tok {
            Tok::Ident(name) => {
                // generic application: Name(args) — types are values, generics are calls
                if matches!(self.peek(), Tok::LParen) {
                    self.bump();
                    let mut args = Vec::new();
                    while !self.eat(&Tok::RParen) {
                        args.push(self.parse_type()?);
                        self.eat(&Tok::Comma);
                    }
                    return Ok(TypeExpr::App { name, args });
                }
                Ok(TypeExpr::Named(name))
            }
            Tok::KwNull => Ok(TypeExpr::Null),
            Tok::Amp => {
                // &str — borrowed slice
                let inner = self.bump();
                match inner.tok {
                    Tok::Ident(name) => Ok(TypeExpr::Ref(name)),
                    _ => Err(ParseError {
                        line: inner.line,
                        col: inner.col,
                        msg: "expected type after `&`".into(),
                    }),
                }
            }
            Tok::LParen => {
                // tuple type
                let mut items = Vec::new();
                while !self.eat(&Tok::RParen) {
                    items.push(self.parse_type()?);
                    self.eat(&Tok::Comma);
                }
                Ok(TypeExpr::Tuple(items))
            }
            Tok::LBracket => {
                // [Type] list sugar? spec uses List(T); accept [T] too
                let inner = self.parse_type()?;
                self.expect(&Tok::RBracket, "`]`")?;
                Ok(TypeExpr::App {
                    name: "List".into(),
                    args: vec![inner],
                })
            }
            Tok::KwChannel => {
                // channel(T) — a channel of T (§14)
                if matches!(self.peek(), Tok::LParen) {
                    self.bump();
                    let inner = self.parse_type()?;
                    self.expect(&Tok::RParen, "`)`")?;
                    Ok(TypeExpr::App {
                        name: "channel".into(),
                        args: vec![inner],
                    })
                } else {
                    Ok(TypeExpr::Named("channel".into()))
                }
            }
            _ => Err(ParseError {
                line: t.line,
                col: t.col,
                msg: format!("expected type, found {:?}", t.tok),
            }),
        }
    }
}

/// Parse `{expr}` interpolations inside a string literal body.
/// `\{` is an escaped literal brace.
fn parse_interp(s: &str, _line: usize) -> Vec<StrPart> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'{') {
            chars.next();
            cur.push('{');
            continue;
        }
        if c == '{' {
            // find matching }
            let mut depth = 1;
            let mut inner = String::new();
            for c2 in chars.by_ref() {
                if c2 == '{' {
                    depth += 1;
                } else if c2 == '}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                inner.push(c2);
            }
            if !cur.is_empty() {
                parts.push(StrPart::Lit(std::mem::take(&mut cur)));
            }
            // parse inner as expression via a sub-lexer/parser
            if let Ok(expr) = parse_expr_string(&inner) {
                parts.push(StrPart::Interp(expr));
            } else {
                // not valid expr — treat literally
                cur.push('{');
                cur.push_str(&inner);
                cur.push('}');
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() || parts.is_empty() {
        parts.push(StrPart::Lit(cur));
    }
    parts
}

fn parse_expr_string(s: &str) -> Result<Expr, ()> {
    let toks = crate::lexer::lex(s).map_err(|_| ())?;
    let mut p = Parser::new(toks);
    let e = p.parse_expr().map_err(|_| ())?;
    Ok(e)
}

pub fn parse(src: &str) -> Result<Program, ParseError> {
    let toks = crate::lexer::lex(src).map_err(|e| ParseError {
        line: e.line,
        col: e.col,
        msg: e.msg,
    })?;
    let mut p = Parser::new(toks);
    p.parse_program()
}

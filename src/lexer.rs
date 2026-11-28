//! Lexer: tokens from `.znz` files (SPEC §5).

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // literals
    Int(String),
    Float(String),
    Str(String), // raw contents; interpolation parsed later
    Ident(String),

    // keywords
    KwFor,
    KwIn,
    KwIf,
    KwElse,
    KwMatch,
    KwAs,
    KwReturn,
    KwParallel,
    KwSpawn,
    KwChannel,
    KwUse,
    KwWeak,
    KwNull,
    KwBreak,
    KwContinue,
    KwAnd,
    KwOr,
    KwNot,
    KwTrue,
    KwFalse,
    KwMain,

    // punctuation
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Dot,
    Semi,
    Pipe,   // | union sugar
    Dollar, // $( count-loop
    Amp,    // & borrowed type (&str)

    // operators
    Bang,        // ! declare / unary not
    Slash,       // / reassign (also /x+ /x- handled in parser via Plus/Minus)
    Assign,      // =
    EqEq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    Plus,
    Minus,
    Star,
    Percent,

    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Clone)]
pub struct LexError {
    pub line: usize,
    pub col: usize,
    pub msg: String,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }

    fn peek3(&self) -> Option<u8> {
        self.src.get(self.pos + 2).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T, LexError> {
        Err(LexError {
            line: self.line,
            col: self.col,
            msg: msg.into(),
        })
    }

    /// `//` ... `\\` block comment skipper (SPEC §5.1): the first `\\`
    /// on its own line closes; no nesting; unterminated is an error.
    fn skip_block_comment(&mut self) -> Result<(), LexError> {
        // consume `//` and rest of opening line
        self.bump();
        self.bump();
        while let Some(c) = self.peek() {
            if c == b'\n' {
                self.bump();
                break;
            }
            self.bump();
        }
        // consume lines until a line whose trimmed content is exactly `\\`
        loop {
            if self.peek().is_none() {
                return self.err("unterminated `//` block comment — expected a `\\\\` line (SPEC §5.1)");
            }
            // read one line
            let mut line_buf = String::new();
            while let Some(c) = self.peek() {
                if c == b'\n' {
                    self.bump();
                    break;
                }
                line_buf.push(c as char);
                self.bump();
            }
            if line_buf.trim() == "\\\\" {
                return Ok(());
            }
            if self.peek().is_none() && line_buf.trim() != "\\\\" {
                // consumed last line without newline; check it
                if line_buf.trim() == "\\\\" {
                    return Ok(());
                }
                return self.err("unterminated `//` block comment — expected a `\\\\` line (SPEC §5.1)");
            }
        }
    }

    fn skip_ws_comments2(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\r' | b'\n') => {
                    self.bump();
                }
                Some(b'/') if self.peek2() == Some(b'/') && self.peek3() == Some(b'/') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some(b'/') if self.peek2() == Some(b'/') => {
                    self.skip_block_comment()?;
                }
                _ => return Ok(()),
            }
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut out = Vec::new();
        loop {
            self.skip_ws_comments2()?;
            let (line, col) = (self.line, self.col);
            let Some(_) = self.peek() else {
                out.push(Token {
                    tok: Tok::Eof,
                    line,
                    col,
                });
                return Ok(out);
            };
            let tok = self.next_token(line, col)?;
            out.push(Token { tok, line, col });
        }
    }

    fn next_token(&mut self, _line: usize, _col: usize) -> Result<Tok, LexError> {
        let c = self.peek().unwrap();
        match c {
            b'0'..=b'9' => self.lex_number(),
            b'"' => self.lex_string(),
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident(),
            _ => self.lex_punct(),
        }
    }

    fn lex_number(&mut self) -> Result<Tok, LexError> {
        let mut s = String::new();
        let mut is_float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => {
                    s.push(c as char);
                    self.bump();
                }
                b'.' if self.peek2().map_or(false, |d| d.is_ascii_digit()) => {
                    is_float = true;
                    s.push('.');
                    self.bump();
                }
                _ => break,
            }
        }
        Ok(if is_float {
            Tok::Float(s)
        } else {
            Tok::Int(s)
        })
    }

    fn lex_string(&mut self) -> Result<Tok, LexError> {
        self.bump(); // opening "
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return self.err("unterminated string literal"),
                Some(b'"') => {
                    self.bump();
                    return Ok(Tok::Str(s));
                }
                Some(b'\\') => {
                    self.bump();
                    match self.peek() {
                        Some(b'n') => {
                            s.push('\n');
                            self.bump();
                        }
                        Some(b't') => {
                            s.push('\t');
                            self.bump();
                        }
                        Some(b'"') => {
                            s.push('"');
                            self.bump();
                        }
                        Some(b'\\') => {
                            s.push('\\');
                            self.bump();
                        }
                        Some(b'{') => {
                            s.push_str("\\{");
                            self.bump();
                        }
                        Some(other) => {
                            s.push('\\');
                            s.push(other as char);
                            self.bump();
                        }
                        None => return self.err("unterminated string literal"),
                    }
                }
                Some(c) => {
                    s.push(c as char);
                    self.bump();
                }
            }
        }
    }

    fn lex_ident(&mut self) -> Result<Tok, LexError> {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' {
                s.push(c as char);
                self.bump();
            } else {
                break;
            }
        }
        Ok(match s.as_str() {
            "for" => Tok::KwFor,
            "in" => Tok::KwIn,
            "if" => Tok::KwIf,
            "else" => Tok::KwElse,
            "match" => Tok::KwMatch,
            "as" => Tok::KwAs,
            "return" => Tok::KwReturn,
            "parallel" => Tok::KwParallel,
            "spawn" => Tok::KwSpawn,
            "channel" => Tok::KwChannel,
            "use" => Tok::KwUse,
            "weak" => Tok::KwWeak,
            "null" => Tok::KwNull,
            "break" => Tok::KwBreak,
            "continue" => Tok::KwContinue,
            "and" => Tok::KwAnd,
            "or" => Tok::KwOr,
            "not" => Tok::KwNot,
            "true" => Tok::KwTrue,
            "false" => Tok::KwFalse,
            "main" => Tok::KwMain,
            _ => Tok::Ident(s),
        })
    }

    fn lex_punct(&mut self) -> Result<Tok, LexError> {
        let c = self.peek().unwrap();
        let c2 = self.peek2();
        let c3 = self.peek3();
        let two = |a: u8, b: u8| c == a && c2 == Some(b);
        let three = |a: u8, b: u8, d: u8| c == a && c2 == Some(b) && c3 == Some(d);

        let tok = if three(b'/', b'/', b'/') {
            // shouldn't reach here — handled by skip_ws_comments2 — but guard anyway
            return self.err("internal: comment reached punct lexer");
        } else if two(b'=', b'=') {
            self.bump();
            self.bump();
            Tok::EqEq
        } else if two(b'!', b'=') {
            self.bump();
            self.bump();
            Tok::NotEq
        } else if two(b'<', b'=') {
            self.bump();
            self.bump();
            Tok::LtEq
        } else if two(b'>', b'=') {
            self.bump();
            self.bump();
            Tok::GtEq
        } else if two(b'&', b'&') {
            self.bump();
            self.bump();
            Tok::KwAnd
        } else if two(b'|', b'|') {
            self.bump();
            self.bump();
            Tok::KwOr
        } else if c == b'&' {
            Tok::Amp
        } else {
            match c {
                b'(' => Tok::LParen,
                b')' => Tok::RParen,
                b'{' => Tok::LBrace,
                b'}' => Tok::RBrace,
                b'[' => Tok::LBracket,
                b']' => Tok::RBracket,
                b',' => Tok::Comma,
                b':' => Tok::Colon,
                b'.' => Tok::Dot,
                b';' => Tok::Semi,
                b'|' => Tok::Pipe,
                b'$' => Tok::Dollar,
                b'!' => Tok::Bang,
                b'/' => Tok::Slash,
                b'=' => Tok::Assign,
                b'<' => Tok::Lt,
                b'>' => Tok::Gt,
                b'+' => Tok::Plus,
                b'-' => Tok::Minus,
                b'*' => Tok::Star,
                b'%' => Tok::Percent,
                _ => {
                    return self.err(format!(
                        "unexpected character `{}`",
                        c as char
                    ));
                }
            }
        };
        // bump consumed chars: for two/three-char we already bumped; for single we bump once.
        if matches!(
            tok,
            Tok::LParen
                | Tok::RParen
                | Tok::LBrace
                | Tok::RBrace
                | Tok::LBracket
                | Tok::RBracket
                | Tok::Comma
                | Tok::Colon
                | Tok::Dot
                | Tok::Semi
                | Tok::Pipe
                | Tok::Dollar
                | Tok::Bang
                | Tok::Slash
                | Tok::Assign
                | Tok::Lt
                | Tok::Gt
                | Tok::Plus
                | Tok::Minus
                | Tok::Star
                | Tok::Percent
                | Tok::Amp
        ) {
            self.bump();
        }
        Ok(tok)
    }
}

pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(src).tokenize()
}

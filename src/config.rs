//! Zappr.yaml — the one config file (SPEC §19).
//! Minimal YAML subset parser: maps, lists, scalars, comments.
//! Unknown keys are errors; defaults exist but are never silent.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum YVal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    Seq(Vec<YVal>),
    Map(BTreeMap<String, YVal>),
}

impl YVal {
    pub fn as_map(&self) -> Option<&BTreeMap<String, YVal>> {
        match self {
            YVal::Map(m) => Some(m),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            YVal::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_seq(&self) -> Option<&[YVal]> {
        match self {
            YVal::Seq(s) => Some(s),
            _ => None,
        }
    }
    pub fn get(&self, key: &str) -> Option<&YVal> {
        self.as_map().and_then(|m| m.get(key))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub name: Option<String>,
    pub version: Option<String>,
    pub license: Option<String>,
    pub license_holder: Option<String>,
    pub targets: Vec<String>,
    pub state_ttl: Option<String>,
    pub frame_rate: Option<YVal>,
    pub auto_null: bool,
    /// damage-rect redraws: only changed regions are uploaded/composited
    pub efficient_redraw: bool,
    pub sign: bool,
    pub sign_key: Option<String>,
    /// val: entries — read verbatim as zapYaml.x
    pub vals: BTreeMap<String, YVal>,
    pub github_deps: BTreeMap<String, String>,
    pub resolved: BTreeMap<String, String>,
    pub cli: Vec<String>,
}

const KNOWN_KEYS: &[&str] = &[
    "name",
    "version",
    "license",
    "licenseHolder",
    "targets",
    "stateTtl",
    "frameRate",
    "autoNull",
    "efficientRedraw",
    "sign",
    "signKey",
    "val",
    "githubDeps",
    "resolved",
    "cli",
];

pub fn parse(src: &str) -> Result<Config, String> {
    let root = parse_yaml(src)?;
    let map = root
        .as_map()
        .ok_or("Zappr.yaml: top level must be a map")?;
    // unknown keys are errors (SPEC §3.3, §19)
    for k in map.keys() {
        if !KNOWN_KEYS.contains(&k.as_str()) {
            return Err(format!("Zappr.yaml: unknown key `{k}`"));
        }
    }
    let mut c = Config {
        sign: true, // default true (§18.2)
        ..Default::default()
    };
    for (k, v) in map {
        match k.as_str() {
            "name" => c.name = v.as_str().map(String::from),
            "version" => c.version = v.as_str().map(String::from),
            "license" => c.license = v.as_str().map(String::from),
            "licenseHolder" => c.license_holder = v.as_str().map(String::from),
            "targets" => {
                c.targets = v
                    .as_seq()
                    .map(|s| {
                        s.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            "stateTtl" => c.state_ttl = v.as_str().map(String::from),
            "frameRate" => c.frame_rate = Some(v.clone()),
            "autoNull" => {
                c.auto_null = match v {
                    YVal::Bool(b) => *b,
                    YVal::Str(s) => s == "true",
                    _ => false,
                }
            }
            "efficientRedraw" => {
                c.efficient_redraw = match v {
                    YVal::Bool(b) => *b,
                    YVal::Str(s) => s == "true",
                    _ => false,
                }
            }
            "sign" => {
                c.sign = match v {
                    YVal::Bool(b) => *b,
                    YVal::Str(s) => s == "true",
                    _ => true,
                }
            }
            "signKey" => c.sign_key = v.as_str().map(String::from),
            "val" => {
                if let Some(m) = v.as_map() {
                    c.vals = m.clone();
                }
            }
            "githubDeps" => {
                if let Some(m) = v.as_map() {
                    for (dk, dv) in m {
                        c.github_deps.insert(dk.clone(), scalar_str(dv));
                    }
                }
            }
            "resolved" => {
                if let Some(m) = v.as_map() {
                    for (dk, dv) in m {
                        c.resolved.insert(dk.clone(), scalar_str(dv));
                    }
                }
            }
            "cli" => {
                c.cli = v
                    .as_seq()
                    .map(|s| {
                        s.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            }
            _ => {}
        }
    }
    // required keys (§19)
    if c.name.is_none() {
        return Err("Zappr.yaml: `name` is required".into());
    }
    if c.version.is_none() {
        return Err("Zappr.yaml: `version` is required".into());
    }
    if c.state_ttl.is_none() {
        eprintln!("zap: warning — `stateTtl` not declared in Zappr.yaml (default 10m applies; silent behavior is a sidecar, §19.1)");
    }
    Ok(c)
}

fn scalar_str(v: &YVal) -> String {
    match v {
        YVal::Str(s) => s.clone(),
        YVal::Int(i) => i.to_string(),
        YVal::Float(f) => f.to_string(),
        YVal::Bool(b) => b.to_string(),
        YVal::Null => String::new(),
        _ => String::new(),
    }
}

// ---------- minimal YAML ----------

pub fn parse_yaml(src: &str) -> Result<YVal, String> {
    let mut p = YParser {
        lines: src.lines().collect(),
        pos: 0,
    };
    p.skip_blank();
    let v = p.parse_block(0)?;
    Ok(v)
}

struct YParser<'a> {
    lines: Vec<&'a str>,
    pos: usize,
}

impl<'a> YParser<'a> {
    fn skip_blank(&mut self) {
        while self.pos < self.lines.len() {
            let l = self.lines[self.pos].trim();
            if l.is_empty() || l.starts_with('#') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn indent_of(&self) -> usize {
        let l = self.lines[self.pos];
        l.len() - l.trim_start().len()
    }

    fn parse_block(&mut self, min_indent: usize) -> Result<YVal, String> {
        self.skip_blank();
        if self.pos >= self.lines.len() {
            return Ok(YVal::Null);
        }
        let ind = self.indent_of();
        if ind < min_indent {
            return Ok(YVal::Null);
        }
        let l = self.lines[self.pos].trim();
        if l.starts_with("- ") || l == "-" {
            self.parse_seq(ind)
        } else if l.contains(':') {
            self.parse_map(ind)
        } else {
            Ok(YVal::Str(self.parse_scalar(l)?.0))
        }
    }

    fn parse_map(&mut self, indent: usize) -> Result<YVal, String> {
        let mut m = BTreeMap::new();
        while self.pos < self.lines.len() {
            self.skip_blank();
            if self.pos >= self.lines.len() || self.indent_of() != indent {
                break;
            }
            let line = self.lines[self.pos].trim().to_string();
            self.pos += 1;
            // strip comment (naive — no # inside quotes handling)
            let line = strip_comment(&line);
            let Some((key, rest)) = line.split_once(':') else {
                return Err(format!("Zappr.yaml: bad line `{line}`"));
            };
            let key = key.trim().to_string();
            let rest = rest.trim();
            if rest.is_empty() {
                // nested block or null
                self.skip_blank();
                if self.pos < self.lines.len() && self.indent_of() > indent {
                    m.insert(key, self.parse_block(indent + 1)?);
                } else {
                    m.insert(key, YVal::Null);
                }
            } else if rest == "|" || rest == ">" {
                // block scalar — join following indented lines
                let mut buf = String::new();
                while self.pos < self.lines.len() {
                    self.skip_blank();
                    if self.pos >= self.lines.len() || self.indent_of() <= indent {
                        break;
                    }
                    buf.push_str(self.lines[self.pos].trim());
                    buf.push('\n');
                    self.pos += 1;
                }
                m.insert(key, YVal::Str(buf.trim_end().to_string()));
            } else {
                m.insert(key, self.scalar(rest)?);
            }
        }
        Ok(YVal::Map(m))
    }

    fn parse_seq(&mut self, indent: usize) -> Result<YVal, String> {
        let mut items = Vec::new();
        while self.pos < self.lines.len() {
            self.skip_blank();
            if self.pos >= self.lines.len() || self.indent_of() != indent {
                break;
            }
            let line = self.lines[self.pos].trim().to_string();
            if !line.starts_with('-') {
                break;
            }
            self.pos += 1;
            let rest = line[1..].trim().to_string();
            if rest.is_empty() {
                items.push(self.parse_block(indent + 1)?);
            } else if rest.contains(':') && !rest.starts_with('"') {
                // inline map entry: `- key: value` (single level)
                let mut m = BTreeMap::new();
                for part in rest.split(',') {
                    if let Some((k, v)) = part.split_once(':') {
                        m.insert(k.trim().to_string(), self.scalar(v)?);
                    }
                }
                items.push(YVal::Map(m));
            } else {
                items.push(self.scalar(&rest)?);
            }
        }
        Ok(YVal::Seq(items))
    }

    fn parse_scalar(&mut self, s: &str) -> Result<(String, ()), String> {
        Ok((s.to_string(), ()))
    }

    fn scalar(&mut self, s: &str) -> Result<YVal, String> {
        let s = strip_comment(s).trim().to_string();
        if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
            return Ok(YVal::Str(s[1..s.len() - 1].replace("\\\"", "\"").replace("\\n", "\n")));
        }
        if s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2 {
            return Ok(YVal::Str(s[1..s.len() - 1].to_string()));
        }
        Ok(match s.as_str() {
            "true" | "True" => YVal::Bool(true),
            "false" | "False" => YVal::Bool(false),
            "null" | "~" | "" => YVal::Null,
            _ => {
                if let Ok(i) = s.parse::<i64>() {
                    YVal::Int(i)
                } else if let Ok(f) = s.parse::<f64>() {
                    YVal::Float(f)
                } else {
                    YVal::Str(s)
                }
            }
        })
    }
}

fn strip_comment(s: &str) -> String {
    // naive: # starts a comment unless inside quotes
    let mut in_s = false;
    let mut in_d = false;
    for (i, c) in s.char_indices() {
        match c {
            '\'' if !in_d => in_s = !in_s,
            '"' if !in_s => in_d = !in_d,
            '#' if !in_s && !in_d => return s[..i].to_string(),
            _ => {}
        }
    }
    s.to_string()
}

//! LaTeX front end: delimiters, a small TeX-like macro expander (user
//! `\newcommand`/`\def`/`\DeclareMathOperator`, notebook/MathJax aliases) and
//! environment rewrites, producing input the vendored engine understands.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock};

use super::MathError;

/// Most macro expansions per formula (guards `\def\a{\a\a}` bombs).
const MAX_EXPANSIONS: usize = 20_000;
/// Most tokens an expanded formula may have.
const MAX_TOKENS: usize = 40_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    /// `\name` (letters).
    Word(Arc<str>),
    /// `\x` for a non-letter `x` (`\,`, `\{`, `\\`, `\ `).
    Sym(char),
    Char(char),
    Open,
    Close,
    /// `#1`..`#9` in a macro body.
    Param(u8),
    Space,
}

fn word(s: &str) -> Tok {
    Tok::Word(Arc::from(s))
}

pub(crate) fn tokenize(s: &str) -> Result<Vec<Tok>, MathError> {
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.next() {
                None => return Err(MathError::Parse("trailing backslash".into())),
                Some(n) if n.is_ascii_alphabetic() => {
                    let mut name = String::new();
                    name.push(n);
                    while let Some(&n) = it.peek() {
                        if n.is_ascii_alphabetic() {
                            name.push(n);
                            it.next();
                        } else {
                            break;
                        }
                    }
                    // TeX skips spaces after a control word.
                    while matches!(it.peek(), Some(c) if c.is_whitespace()) {
                        it.next();
                    }
                    out.push(Tok::Word(Arc::from(name)));
                }
                Some(n) if n.is_whitespace() => out.push(Tok::Sym(' ')),
                Some(n) => out.push(Tok::Sym(n)),
            },
            '{' => out.push(Tok::Open),
            '}' => out.push(Tok::Close),
            '#' => match it.peek() {
                Some(&d @ '1'..='9') => {
                    it.next();
                    out.push(Tok::Param(d as u8 - b'0'));
                }
                Some('#') => {
                    it.next();
                    out.push(Tok::Char('#'));
                }
                // Outside a macro body `#` is literal text (MathJax prints it).
                _ => out.push(Tok::Sym('#')),
            },
            '%' => {
                // Comment to end of line (and the line break itself).
                for c in it.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {
                while matches!(it.peek(), Some(c) if c.is_whitespace()) {
                    it.next();
                }
                if !matches!(out.last(), Some(Tok::Space)) {
                    out.push(Tok::Space);
                }
            }
            c => out.push(Tok::Char(c)),
        }
    }
    Ok(out)
}

pub(crate) fn serialize(toks: &[Tok]) -> String {
    let mut s = String::with_capacity(toks.len() * 2);
    let mut after_word = false;
    for t in toks {
        match t {
            Tok::Word(n) => {
                s.push('\\');
                s.push_str(n);
                after_word = true;
                continue;
            }
            Tok::Sym(c) => {
                s.push('\\');
                s.push(*c);
            }
            Tok::Char(c) => {
                if after_word && c.is_ascii_alphabetic() {
                    s.push(' ');
                }
                s.push(*c);
            }
            Tok::Open => s.push('{'),
            Tok::Close => s.push('}'),
            Tok::Param(n) => {
                let _ = write!(s, "#{n}");
            }
            Tok::Space => s.push(' '),
        }
        after_word = false;
    }
    s
}

/// A user macro: `\newcommand{\name}[params][default]{body}`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Macro {
    params: u8,
    /// Default for an optional first argument (`[default]` in `\newcommand`).
    default: Option<Vec<Tok>>,
    body: Vec<Tok>,
}

/// Macros defined by earlier formulas, applied to later ones.
///
/// MathJax in Jupyter keeps `\newcommand`, `\renewcommand`, `\def`, `\let`
/// and `\DeclareMathOperator` definitions from any rendered formula for the
/// rest of the page, and notebooks rely on that (a hidden `$\newcommand{\R}
/// {\mathbb{R}}$` cell at the top). Feed every formula of a notebook, in
/// order, to [`Macros::scan`] and pass the table to
/// [`render_svg_with`](super::render_svg_with).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Macros {
    defs: HashMap<String, Macro>,
}

impl Macros {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the definitions made in `latex` (a formula, with or without
    /// `$` delimiters). Anything else in it is ignored; malformed
    /// definitions are skipped.
    pub fn scan(&mut self, latex: &str) {
        if !latex.contains('\\') {
            return;
        }
        let (inner, _) = super::strip_delimiters(latex);
        let Ok(toks) = tokenize(inner) else { return };
        let mut ex = Expander::new(toks, Some(&*self));
        ex.scan_only = true;
        if ex.run().is_ok() || !ex.local.is_empty() {
            for (k, v) in ex.local {
                self.defs.insert(k, v);
            }
        }
    }

    /// Number of macros defined.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Stable hash of the definitions, for keying a render cache on
    /// `(source, display, macros.fingerprint())`.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut names: Vec<&String> = self.defs.keys().collect();
        names.sort();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for n in names {
            let m = &self.defs[n];
            n.hash(&mut h);
            m.params.hash(&mut h);
            m.default.as_ref().map(|d| serialize(d)).hash(&mut h);
            serialize(&m.body).hash(&mut h);
        }
        h.finish()
    }

    /// True if `\name` (without the backslash) is defined.
    pub fn contains(&self, name: &str) -> bool {
        self.defs.contains_key(name)
    }
}

/// Expand macros and rewrite environments; returns engine input.
pub(crate) fn expand(src: &str, macros: Option<&Macros>) -> Result<String, MathError> {
    let toks = tokenize(src)?;
    let mut ex = Expander::new(toks, macros);
    ex.run()?;
    Ok(serialize(&ex.out))
}

struct Expander<'m> {
    /// Remaining input, reversed (next token at the end).
    input: Vec<Tok>,
    out: Vec<Tok>,
    local: HashMap<String, Macro>,
    global: Option<&'m Macros>,
    expansions: usize,
    /// Only collect definitions (for [`Macros::scan`]).
    scan_only: bool,
}

impl<'m> Expander<'m> {
    fn new(mut toks: Vec<Tok>, global: Option<&'m Macros>) -> Self {
        toks.reverse();
        Expander {
            input: toks,
            out: Vec::new(),
            local: HashMap::new(),
            global,
            expansions: 0,
            scan_only: false,
        }
    }

    fn next(&mut self) -> Option<Tok> {
        self.input.pop()
    }

    fn peek(&self) -> Option<&Tok> {
        self.input.last()
    }

    fn push_front(&mut self, toks: &[Tok]) -> Result<(), MathError> {
        self.expansions += 1;
        if self.expansions > MAX_EXPANSIONS || self.input.len() + toks.len() > MAX_TOKENS * 2 {
            return Err(MathError::TooComplex);
        }
        self.input.extend(toks.iter().rev().cloned());
        Ok(())
    }

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(Tok::Space)) {
            self.input.pop();
        }
    }

    /// A TeX argument: a balanced `{…}` group (without braces) or one token.
    fn arg(&mut self) -> Result<Vec<Tok>, MathError> {
        self.skip_spaces();
        match self.next() {
            Some(Tok::Open) => self.group_rest(),
            Some(Tok::Close) | None => Err(MathError::Parse("missing argument".into())),
            Some(t) => Ok(vec![t]),
        }
    }

    /// Tokens up to the `}` matching an already consumed `{`.
    fn group_rest(&mut self) -> Result<Vec<Tok>, MathError> {
        let mut depth = 1usize;
        let mut v = Vec::new();
        while let Some(t) = self.next() {
            match t {
                Tok::Open => depth += 1,
                Tok::Close => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(v);
                    }
                }
                _ => {}
            }
            v.push(t);
        }
        Err(MathError::Parse("unbalanced braces".into()))
    }

    /// `[…]` optional argument if present (balanced in braces).
    fn optional(&mut self) -> Result<Option<Vec<Tok>>, MathError> {
        self.skip_spaces();
        if self.peek() != Some(&Tok::Char('[')) {
            return Ok(None);
        }
        self.next();
        let mut depth = 0usize;
        let mut v = Vec::new();
        while let Some(t) = self.next() {
            match &t {
                Tok::Open => depth += 1,
                Tok::Close => depth = depth.saturating_sub(1),
                Tok::Char(']') if depth == 0 => return Ok(Some(v)),
                _ => {}
            }
            v.push(t);
        }
        Err(MathError::Parse("unterminated [".into()))
    }

    fn star(&mut self) -> bool {
        self.skip_spaces();
        if self.peek() == Some(&Tok::Char('*')) {
            self.next();
            true
        } else {
            false
        }
    }

    /// Macro name for definitions: `\name` or `{\name}`.
    fn def_name(&mut self) -> Result<String, MathError> {
        self.skip_spaces();
        match self.next() {
            Some(Tok::Word(n)) => Ok(n.to_string()),
            Some(Tok::Sym(c)) => Ok(c.to_string()),
            Some(Tok::Open) => {
                let g = self.group_rest()?;
                let g: Vec<&Tok> = g.iter().filter(|t| **t != Tok::Space).collect();
                match g.as_slice() {
                    [Tok::Word(n)] => Ok(n.to_string()),
                    [Tok::Sym(c)] => Ok(c.to_string()),
                    _ => Err(MathError::Parse("bad macro name".into())),
                }
            }
            _ => Err(MathError::Parse("bad macro name".into())),
        }
    }

    fn lookup(&self, name: &str) -> Option<Macro> {
        if let Some(m) = self.local.get(name) {
            return Some(m.clone());
        }
        if let Some(m) = self.global.and_then(|g| g.defs.get(name)) {
            return Some(m.clone());
        }
        builtins().get(name).cloned()
    }

    fn defined(&self, name: &str) -> bool {
        self.local.contains_key(name) || self.global.is_some_and(|g| g.defs.contains_key(name))
    }

    fn run(&mut self) -> Result<(), MathError> {
        while let Some(t) = self.next() {
            if self.out.len() > MAX_TOKENS {
                return Err(MathError::TooComplex);
            }
            let Tok::Word(name) = &t else {
                if !self.scan_only {
                    // `~` is a no-break space in TeX, not a tilde.
                    self.out.push(if t == Tok::Char('~') {
                        Tok::Sym(' ')
                    } else {
                        t
                    });
                }
                continue;
            };
            let name = name.clone();
            match &*name {
                "newcommand" | "renewcommand" | "providecommand" | "DeclareRobustCommand" => {
                    self.star();
                    let n = self.def_name()?;
                    let params = match self.optional()? {
                        Some(p) => serialize(&p)
                            .trim()
                            .parse::<u8>()
                            .ok()
                            .filter(|n| *n <= 9)
                            .ok_or_else(|| MathError::Parse("bad parameter count".into()))?,
                        None => 0,
                    };
                    let default = self.optional()?;
                    let body = self.arg()?;
                    if &*name != "providecommand" || !self.defined(&n) {
                        self.local.insert(
                            n,
                            Macro {
                                params,
                                default,
                                body,
                            },
                        );
                    }
                }
                "def" | "gdef" | "edef" | "xdef" => {
                    let n = self.def_name()?;
                    let mut params = 0u8;
                    loop {
                        match self.next() {
                            Some(Tok::Param(k)) if k == params + 1 => params = k,
                            Some(Tok::Open) => break,
                            Some(Tok::Space) => {}
                            _ => {
                                return Err(MathError::Unsupported(
                                    "delimited \\def parameters".into(),
                                ));
                            }
                        }
                    }
                    let body = self.group_rest()?;
                    self.local.insert(
                        n,
                        Macro {
                            params,
                            default: None,
                            body,
                        },
                    );
                }
                "let" => {
                    let n = self.def_name()?;
                    self.skip_spaces();
                    if self.peek() == Some(&Tok::Char('=')) {
                        self.next();
                    }
                    self.skip_spaces();
                    let target = self
                        .next()
                        .ok_or_else(|| MathError::Parse("\\let without target".into()))?;
                    let body = match &target {
                        Tok::Word(w) => match self.lookup(w) {
                            Some(m) => {
                                self.local.insert(n, m);
                                continue;
                            }
                            None => vec![target],
                        },
                        _ => vec![target],
                    };
                    self.local.insert(
                        n,
                        Macro {
                            params: 0,
                            default: None,
                            body,
                        },
                    );
                }
                "DeclareMathOperator" => {
                    let star = self.star();
                    let n = self.def_name()?;
                    let text = self.arg()?;
                    let mut body = vec![word("operatorname")];
                    if star {
                        body.push(Tok::Char('*'));
                    }
                    body.push(Tok::Open);
                    body.extend(text);
                    body.push(Tok::Close);
                    self.local.insert(
                        n,
                        Macro {
                            params: 0,
                            default: None,
                            body,
                        },
                    );
                }
                "global" | "long" | "protected" => {}
                "ce" | "pu" if !self.scan_only => {
                    return Err(MathError::Unsupported(format!("mhchem \\{name}")));
                }
                _ if self.scan_only => {}
                "begin" => self.begin_env()?,
                "end" => {
                    let env = self.arg()?;
                    let env = serialize(&env);
                    let env = env.trim();
                    let mapped = env_rename(env).map(|(to, _)| to).unwrap_or(env);
                    if env != "subequations" {
                        self.out.push(word("end"));
                        self.out.push(Tok::Open);
                        self.out.extend(tokenize(mapped)?);
                        self.out.push(Tok::Close);
                    }
                }
                "unicode" => {
                    let a = serialize(&self.arg()?);
                    let a = a.trim();
                    let cp = if let Some(h) = a.strip_prefix("x").or_else(|| a.strip_prefix("X")) {
                        u32::from_str_radix(h, 16).ok()
                    } else {
                        a.parse::<u32>().ok()
                    };
                    let c = cp
                        .and_then(char::from_u32)
                        .ok_or_else(|| MathError::Parse("bad \\unicode".into()))?;
                    self.out.push(Tok::Char(c));
                }
                "kern" | "mkern" | "hskip" | "mskip" | "mspace" => {
                    let dim = self.tex_dimen()?;
                    if matches!(&*name, "hskip" | "mskip") {
                        // Glue: drop `plus …` / `minus …` stretch.
                        for kw in ["plus", "minus"] {
                            if self.keyword(kw) {
                                self.tex_dimen()?;
                            }
                        }
                    }
                    self.push_front(&tokenize(&format!("\\hspace{{{dim}}}"))?)?;
                }
                "int" | "iint" | "iiint" | "oint" | "oiint" => {
                    // `\int\limits_a^b`: the engine's integral ignores
                    // `\limits`; as an Op atom it takes limits above/below.
                    self.skip_spaces();
                    if matches!(self.peek(), Some(Tok::Word(w)) if &**w == "limits") {
                        self.next();
                        self.out
                            .extend([word("mathop"), Tok::Open, t.clone(), Tok::Close]);
                    } else {
                        self.out.push(t);
                    }
                }
                "bf" | "rm" | "it" | "sf" | "tt" | "cal" | "mit" | "frak" => {
                    // Old-style switches act on the rest of the group.
                    let cmd = match &*name {
                        "bf" => "mathbf",
                        "rm" => "mathrm",
                        "it" | "mit" => "mathit",
                        "sf" => "mathsf",
                        "tt" => "mathtt",
                        "cal" => "mathcal",
                        _ => "mathfrak",
                    };
                    let rest = self.rest_of_group();
                    let mut g = vec![word(cmd), Tok::Open];
                    g.extend(rest);
                    g.push(Tok::Close);
                    self.push_front(&g)?;
                }
                "tiny" | "scriptsize" | "footnotesize" | "small" | "normalsize" | "large"
                | "Large" | "LARGE" | "huge" | "Huge" => {
                    // MathJax scales; a single-size renderer ignores them.
                }
                "hspace" => {
                    self.star();
                    self.out.push(t);
                }
                "bbox" => {
                    self.optional()?;
                    let body = self.arg()?;
                    self.push_group(&body)?;
                }
                "smash" | "mathrlap" | "mathllap" | "mathclap" | "rlap" | "llap" | "clap" => {
                    self.optional()?;
                    let body = self.arg()?;
                    self.push_group(&body)?;
                }
                _ => match self.lookup(&name) {
                    Some(m) => self.expand_macro(&m)?,
                    None => self.out.push(t),
                },
            }
        }
        Ok(())
    }

    /// Tokens up to (not including) the `}` closing the current group.
    fn rest_of_group(&mut self) -> Vec<Tok> {
        let mut depth = 0usize;
        let mut v = Vec::new();
        while let Some(t) = self.peek() {
            match t {
                Tok::Open => depth += 1,
                Tok::Close if depth == 0 => break,
                Tok::Close => depth -= 1,
                Tok::Word(w) if depth == 0 && (&**w == "end" || &**w == "right") => break,
                Tok::Char('&') | Tok::Sym('\\') if depth == 0 => break,
                _ => {}
            }
            v.push(self.next().unwrap());
        }
        v
    }

    /// Literal TeX dimension after `\kern`/`\hskip`: `[=][-]1.5em`.
    fn tex_dimen(&mut self) -> Result<String, MathError> {
        self.skip_spaces();
        if self.peek() == Some(&Tok::Char('=')) {
            self.next();
            self.skip_spaces();
        }
        let mut s = String::new();
        while let Some(Tok::Char(c @ ('-' | '+'))) = self.peek().cloned() {
            self.next();
            if c == '-' {
                s = if s.starts_with('-') {
                    String::new()
                } else {
                    "-".into()
                };
            }
        }
        let mut digits = false;
        while let Some(Tok::Char(c)) = self.peek().cloned() {
            if c.is_ascii_digit() || c == '.' || c == ',' {
                s.push(if c == ',' { '.' } else { c });
                digits |= c.is_ascii_digit();
                self.next();
            } else {
                break;
            }
        }
        self.skip_spaces();
        let mut unit = String::new();
        while unit.len() < 2 {
            match self.peek().cloned() {
                Some(Tok::Char(c)) if c.is_ascii_alphabetic() => {
                    unit.push(c);
                    self.next();
                }
                _ => break,
            }
        }
        if !digits || unit.len() != 2 {
            return Err(MathError::Unsupported("non-literal dimension".into()));
        }
        Ok(s + &unit)
    }

    /// Consume a keyword spelled as letters (`plus`), skipping spaces.
    fn keyword(&mut self, kw: &str) -> bool {
        self.skip_spaces();
        let n = kw.len();
        if self.input.len() < n {
            return false;
        }
        let tail = &self.input[self.input.len() - n..];
        let matches = tail
            .iter()
            .rev()
            .zip(kw.chars())
            .all(|(t, c)| *t == Tok::Char(c));
        if matches {
            self.input.truncate(self.input.len() - n);
        }
        matches
    }

    fn push_group(&mut self, body: &[Tok]) -> Result<(), MathError> {
        let mut g = Vec::with_capacity(body.len() + 2);
        g.push(Tok::Open);
        g.extend_from_slice(body);
        g.push(Tok::Close);
        self.push_front(&g)
    }

    fn expand_macro(&mut self, m: &Macro) -> Result<(), MathError> {
        let mut args: Vec<Vec<Tok>> = Vec::with_capacity(m.params as usize);
        let mut first = 0;
        if let Some(default) = &m.default {
            args.push(self.optional()?.unwrap_or_else(|| default.clone()));
            first = 1;
        }
        for _ in first..m.params {
            args.push(self.arg()?);
        }
        let mut body = Vec::with_capacity(m.body.len());
        for t in &m.body {
            match t {
                Tok::Param(k) => {
                    let a = args
                        .get(*k as usize - 1)
                        .ok_or_else(|| MathError::Parse("macro parameter out of range".into()))?;
                    body.extend(a.iter().cloned());
                }
                t => body.push(t.clone()),
            }
        }
        self.push_front(&body)
    }

    fn begin_env(&mut self) -> Result<(), MathError> {
        let env_toks = self.arg()?;
        let env = serialize(&env_toks);
        let env = env.trim().to_string();
        let Some((to, rule)) = env_rename(&env) else {
            self.emit_begin(&env)?;
            return Ok(());
        };
        match rule {
            Rule::Rename => self.emit_begin(to)?,
            Rule::DropArg => {
                self.arg()?;
                self.emit_begin(to)?;
            }
            Rule::DropOpt => {
                self.optional()?;
                self.emit_begin(to)?;
            }
            Rule::Drop => {}
            Rule::Cells {
                prefix,
                open,
                close,
            } => {
                self.optional()?; // gathered[t]
                let body = self.env_body(&env)?;
                let rows = split_cells(&body);
                let mut out: Vec<Tok> = tokenize(open)?;
                let pre = tokenize(prefix)?;
                for (ri, row) in rows.iter().enumerate() {
                    if ri > 0 {
                        out.push(Tok::Sym('\\'));
                    }
                    for (ci, cell) in row.iter().enumerate() {
                        if ci > 0 {
                            out.push(Tok::Char('&'));
                        }
                        let blank = cell.iter().all(|t| *t == Tok::Space);
                        if !blank {
                            out.push(Tok::Open);
                            out.extend(pre.iter().cloned());
                            out.push(Tok::Space);
                            out.extend(cell.iter().cloned());
                            out.push(Tok::Close);
                        }
                    }
                }
                out.extend(tokenize(close)?);
                self.push_front(&out)?;
            }
        }
        Ok(())
    }

    fn emit_begin(&mut self, env: &str) -> Result<(), MathError> {
        self.out.push(word("begin"));
        self.out.push(Tok::Open);
        self.out.extend(tokenize(env)?);
        self.out.push(Tok::Close);
        Ok(())
    }

    /// Body of `\begin{env}` up to its matching `\end{env}` (consumed).
    fn env_body(&mut self, env: &str) -> Result<Vec<Tok>, MathError> {
        let mut depth = 1usize;
        let mut body = Vec::new();
        while let Some(t) = self.next() {
            if let Tok::Word(w) = &t
                && (&**w == "begin" || &**w == "end")
            {
                let is_begin = &**w == "begin";
                let a = self.arg()?;
                if serialize(&a).trim() == env {
                    if is_begin {
                        depth += 1;
                    } else {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(body);
                        }
                    }
                }
                body.push(t);
                body.push(Tok::Open);
                body.extend(a);
                body.push(Tok::Close);
                continue;
            }
            body.push(t);
        }
        Err(MathError::Parse(format!("missing \\end{{{env}}}")))
    }
}

/// Split an environment body into rows (`\\`, `\cr`) and cells (`&`) at the
/// top level (not inside braces or nested environments).
fn split_cells(body: &[Tok]) -> Vec<Vec<Vec<Tok>>> {
    let mut rows = vec![vec![Vec::new()]];
    let mut depth = 0i32;
    let mut env = 0i32;
    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        match t {
            Tok::Open => depth += 1,
            Tok::Close => depth -= 1,
            Tok::Word(w) if &**w == "begin" => env += 1,
            Tok::Word(w) if &**w == "end" => env -= 1,
            _ => {}
        }
        let top = depth == 0 && env == 0;
        match t {
            Tok::Char('&') if top => rows.last_mut().unwrap().push(Vec::new()),
            Tok::Sym('\\') if top => {
                rows.push(vec![Vec::new()]);
                // `\\[2pt]` spacing argument: drop.
                let mut j = i + 1;
                while matches!(body.get(j), Some(Tok::Space)) {
                    j += 1;
                }
                if body.get(j) == Some(&Tok::Char('['))
                    && let Some(k) = body[j..].iter().position(|t| *t == Tok::Char(']'))
                {
                    i = j + k;
                }
            }
            Tok::Word(w) if top && &**w == "cr" => rows.push(vec![Vec::new()]),
            _ => rows.last_mut().unwrap().last_mut().unwrap().push(t.clone()),
        }
        i += 1;
    }
    // A trailing `\\` leaves an empty last row: drop it like LaTeX does.
    if rows.len() > 1
        && rows
            .last()
            .is_some_and(|r| r.len() == 1 && r[0].iter().all(|t| *t == Tok::Space))
    {
        rows.pop();
    }
    rows
}

#[derive(Clone, Copy)]
enum Rule {
    Rename,
    /// Rename and drop a `{n}` argument (`alignat{2}`).
    DropArg,
    /// Rename and drop an optional `[…]` (`pmatrix*[r]`).
    DropOpt,
    /// Remove `\begin`/`\end` (`subequations`).
    Drop,
    /// Rebuild the body: each cell becomes `{prefix cell}` inside `open … close`.
    Cells {
        prefix: &'static str,
        open: &'static str,
        close: &'static str,
    },
}

/// Environments the engine lacks, mapped onto ones it has.
fn env_rename(env: &str) -> Option<(&'static str, Rule)> {
    Some(match env {
        "align*" | "flalign" | "flalign*" => ("align", Rule::Rename),
        "alignat" | "alignat*" | "xalignat" | "xalignat*" | "xxalignat" => ("align", Rule::DropArg),
        "equation*" => ("equation", Rule::Rename),
        "gather*" => ("gather", Rule::Rename),
        "multline*" => ("multline", Rule::Rename),
        "cases*" => ("cases", Rule::Rename),
        "matrix*" => ("matrix", Rule::DropOpt),
        "pmatrix*" => ("pmatrix", Rule::DropOpt),
        "bmatrix*" => ("bmatrix", Rule::DropOpt),
        "Bmatrix*" => ("Bmatrix", Rule::DropOpt),
        "vmatrix*" => ("vmatrix", Rule::DropOpt),
        "Vmatrix*" => ("Vmatrix", Rule::DropOpt),
        "subequations" => ("subequations", Rule::Drop),
        "alignedat" => ("aligned", Rule::DropArg),
        "gathered" => (
            "array",
            Rule::Cells {
                prefix: "\\displaystyle",
                open: "\\begin{array}{c}",
                close: "\\end{array}",
            },
        ),
        "eqnarray" | "eqnarray*" => (
            "array",
            Rule::Cells {
                prefix: "\\displaystyle",
                open: "\\begin{array}{rcl}",
                close: "\\end{array}",
            },
        ),
        "smallmatrix" => (
            "matrix",
            Rule::Cells {
                prefix: "\\scriptstyle",
                open: "\\begin{matrix}",
                close: "\\end{matrix}",
            },
        ),
        "dcases" | "dcases*" => (
            "cases",
            Rule::Cells {
                prefix: "\\displaystyle",
                open: "\\begin{cases}",
                close: "\\end{cases}",
            },
        ),
        "rcases" | "rcases*" | "drcases" | "drcases*" => (
            "array",
            Rule::Cells {
                prefix: "",
                open: "\\left.\\begin{array}{ll}",
                close: "\\end{array}\\right\\}",
            },
        ),
        _ => return None,
    })
}

/// Built-in macros: notebook/MathJax/amsmath commands the engine lacks,
/// expressed with ones it has. User definitions take precedence.
fn builtins() -> &'static HashMap<&'static str, Macro> {
    static B: OnceLock<HashMap<&'static str, Macro>> = OnceLock::new();
    B.get_or_init(|| {
        let table: &[(&str, u8, &str)] = &[
            ("bm", 1, "\\boldsymbol{#1}"),
            ("bold", 1, "\\mathbf{#1}"),
            ("boldmath", 0, ""),
            ("mathds", 1, "\\mathbb{#1}"),
            ("mathbbm", 1, "\\mathbb{#1}"),
            ("Bbb", 1, "\\mathbb{#1}"),
            ("mathnormal", 1, "{#1}"),
            ("emph", 1, "\\textit{#1}"),
            ("textup", 1, "\\textrm{#1}"),
            ("textnormal", 1, "\\textrm{#1}"),
            ("textmd", 1, "\\textrm{#1}"),
            ("textsl", 1, "\\textit{#1}"),
            ("textsc", 1, "\\textrm{#1}"),
            ("hbox", 1, "\\text{#1}"),
            ("ensuremath", 1, "{#1}"),
            ("lbrace", 0, "\\{"),
            ("rbrace", 0, "\\}"),
            ("lbrack", 0, "["),
            ("rbrack", 0, "]"),
            ("bmod", 0, "\\mathbin{\\mathrm{mod}}"),
            ("pmod", 1, "\\quad(\\mathrm{mod}\\ #1)"),
            ("mod", 1, "\\quad\\mathrm{mod}\\ #1"),
            ("pod", 1, "\\quad(#1)"),
            ("degree", 0, "{}^{\\circ}"),
            ("coloneqq", 0, "\\mathrel{:}\\mathrel{=}"),
            ("coloneq", 0, "\\mathrel{:}\\mathrel{-}"),
            ("Coloneqq", 0, "\\mathrel{::}\\mathrel{=}"),
            ("eqqcolon", 0, "\\mathrel{=}\\mathrel{:}"),
            ("vcentcolon", 0, "\\mathrel{:}"),
            ("dblcolon", 0, "\\mathrel{::}"),
            ("eqref", 1, "(\\text{???})"),
            ("ref", 1, "\\text{???}"),
            ("copyright", 0, "\\text{©}"),
            ("textcopyright", 0, "\\text{©}"),
            ("textregistered", 0, "\\text{®}"),
            ("pounds", 0, "\\text{£}"),
            ("euro", 0, "\\text{€}"),
            ("dag", 0, "\\dagger"),
            ("ddag", 0, "\\ddagger"),
            ("ulcorner", 0, "\\mathopen{⌜}"),
            ("urcorner", 0, "\\mathclose{⌝}"),
            ("llcorner", 0, "\\mathopen{⌞}"),
            ("lrcorner", 0, "\\mathclose{⌟}"),
            ("sideset", 3, "{}#1#3#2"),
            ("lt", 0, "<"),
            ("gt", 0, ">"),
            ("And", 0, "\\&"),
            ("space", 0, "\\ "),
            ("nobreakspace", 0, "~"),
            ("thinspace", 0, "\\,"),
            ("medspace", 0, "\\:"),
            ("thickspace", 0, "\\;"),
            ("negthinspace", 0, "\\!"),
            ("negmedspace", 0, "\\hspace{-0.2222em}"),
            ("negthickspace", 0, "\\hspace{-0.2778em}"),
            ("enspace", 0, "\\hspace{0.5em}"),
            ("enskip", 0, "\\hspace{0.5em}"),
            ("newline", 0, "\\\\"),
            ("require", 1, ""),
            ("class", 2, "#2"),
            ("cssId", 2, "#2"),
            ("style", 2, "#2"),
            ("href", 2, "#2"),
            ("texttip", 2, "#1"),
            ("mathtip", 2, "#1"),
            ("displaylines", 1, "\\begin{gathered}#1\\end{gathered}"),
            ("hdots", 0, "\\cdots"),
            ("dotsm", 0, "\\cdots"),
            ("dotsi", 0, "\\cdots"),
            ("dotso", 0, "\\ldots"),
            ("nobreak", 0, ""),
            ("allowbreak", 0, ""),
            ("hfill", 0, ""),
            ("hfil", 0, ""),
            ("hdashline", 0, "\\hline"),
            ("cline", 1, ""),
            ("mathstrut", 0, "\\vphantom{(}"),
            ("implies", 0, "\\;\\Longrightarrow\\;"),
            ("impliedby", 0, "\\;\\Longleftarrow\\;"),
            ("iff", 0, "\\;\\Longleftrightarrow\\;"),
            ("argmax", 0, "\\operatorname*{arg\\,max}"),
            ("argmin", 0, "\\operatorname*{arg\\,min}"),
            ("tr", 0, "\\operatorname{tr}"),
            ("Tr", 0, "\\operatorname{Tr}"),
            ("rank", 0, "\\operatorname{rank}"),
            ("diag", 0, "\\operatorname{diag}"),
            ("sgn", 0, "\\operatorname{sgn}"),
            ("sign", 0, "\\operatorname{sign}"),
            ("Var", 0, "\\operatorname{Var}"),
            ("Cov", 0, "\\operatorname{Cov}"),
        ];
        let mut m = HashMap::new();
        for &(name, params, body) in table {
            let body = tokenize(body).expect("builtin macro");
            m.insert(
                name,
                Macro {
                    params,
                    default: None,
                    body,
                },
            );
        }
        m
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(s: &str) -> String {
        expand(s, None).unwrap()
    }

    #[test]
    fn roundtrip_plain() {
        assert_eq!(ex(r"\frac{a}{b} + x_1^2"), r"\frac{a}{b} + x_1^2");
        assert_eq!(ex(r"\alpha b"), r"\alpha b");
        assert_eq!(ex(r"\alpha\beta"), r"\alpha\beta");
        assert_eq!(ex(r"a\,b\\c"), r"a\,b\\c");
        assert_eq!(ex("a % comment\nb"), "a b");
    }

    #[test]
    fn user_macros() {
        assert_eq!(
            ex(r"\newcommand{\R}{\mathbb{R}} x \in \R"),
            r" x \in\mathbb{R}"
        );
        assert_eq!(
            ex(r"\newcommand\norm[1]{\left\|#1\right\|}\norm{x}"),
            r"\left\|x\right\|"
        );
        assert_eq!(
            ex(r"\newcommand{\pd}[2][t]{\partial_{#1} #2}\pd{u} \pd[x]{v}"),
            r"\partial_{t} u \partial_{x} v"
        );
        assert_eq!(ex(r"\def\x#1#2{#2#1}\x ab"), "ba");
        assert_eq!(
            ex(r"\DeclareMathOperator*{\amax}{arg\,max}\amax_x"),
            r"\operatorname*{arg\,max}_x"
        );
        assert_eq!(ex(r"\let\a=\alpha \a"), r"\alpha");
    }

    #[test]
    fn global_macros_persist() {
        let mut m = Macros::new();
        m.scan(r"$\newcommand{\R}{\mathbb{R}}\renewcommand{\vec}[1]{\mathbf{#1}}$");
        assert_eq!(m.len(), 2);
        assert_eq!(
            expand(r"\vec{x} \in \R^n", Some(&m)).unwrap(),
            r"\mathbf{x} \in\mathbb{R}^n"
        );
    }

    #[test]
    fn bombs_are_errors() {
        assert_eq!(expand(r"\def\a{\a\a}\a", None), Err(MathError::TooComplex));
        assert_eq!(expand(r"\def\a{x\a}\a", None), Err(MathError::TooComplex));
    }

    #[test]
    fn env_rewrites() {
        assert_eq!(
            ex(r"\begin{align*}a&=b\end{align*}"),
            r"\begin{align}a&=b\end{align}"
        );
        assert_eq!(
            ex(r"\begin{alignat}{2}a&=b\end{alignat}"),
            r"\begin{align}a&=b\end{align}"
        );
        assert_eq!(
            ex(r"\begin{eqnarray}a&=&b\\c&=&d\end{eqnarray}"),
            r"\begin{array}{rcl}{\displaystyle a}&{\displaystyle =}&{\displaystyle b}\\{\displaystyle c}&{\displaystyle =}&{\displaystyle d}\end{array}"
        );
        assert_eq!(ex(r"\begin{subequations}x\end{subequations}"), "x");
        assert_eq!(
            ex(r"\begin{pmatrix*}[r]1\end{pmatrix*}"),
            r"\begin{pmatrix}1\end{pmatrix}"
        );
    }

    #[test]
    fn builtins_apply() {
        assert_eq!(ex(r"\bm{x}"), r"\boldsymbol{x}");
        assert_eq!(ex(r"\lbrace x \rbrace"), r"\{x \}");
        assert_eq!(ex(r"a \pmod{n}"), r"a \quad(\mathrm{mod}\ n)");
        assert_eq!(ex(r"\unicode{x263A}"), "☺");
    }
}

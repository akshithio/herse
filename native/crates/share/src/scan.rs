//! Pre-publish check: things in a notebook the user may not want public.
//!
//! [`scan`] is pure and fast (regexes with literal prefixes over the text
//! parts only; images and other binary payloads are skipped, only counted for
//! size). It reports, per cell and place (source, output N, metadata), at most
//! one [`Warning`] per kind, with a count:
//!
//! * **Secrets** ([`SecretKind`]): provider tokens by their formats (AWS
//!   access key ids, GitHub, OpenAI, Anthropic, Google API, Slack, Stripe
//!   live, Hugging Face, JWTs), PEM private keys, `aws_secret_access_key`
//!   values, passwords in URLs (`postgres://user:pw@host`), quoted literals
//!   assigned to secret-looking names (`password = "…"`,
//!   `api_key="…"`, `{"client_secret": "…"}`, `os.environ["X_TOKEN"] = "…"`),
//!   and `.env` / `export` / `%env` lines (`OPENAI_API_KEY=…`). Placeholders
//!   (`"<your key>"`, `"xxxx"`, `"${TOKEN}"`, `"changeme"`…) are ignored, and
//!   values for token/key/secret names must look random (letters and
//!   digits, 12+ chars). Excerpts are redacted: `ghp_…`, `password = "…"`.
//! * **Home paths**: the user's home directory (`/Users/me/…`), which
//!   reveals the account name and folder layout (tracebacks are full of it).
//! * **Large outputs** (> [`ScanOptions::large_output_bytes`]) and a large
//!   notebook (> [`ScanOptions::large_notebook_bytes`]): slow to load on
//!   nbviewer, and GitHub may refuse very large gists.
//! * **Widget state** (`metadata.widgets`): saved ipywidgets models, which
//!   can hold data from the session.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use herse_nb::{MimeData, Notebook, Output, Value};
use regex::Regex;

/// The kinds of secrets the scanner recognizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretKind {
    PrivateKey,
    AwsAccessKeyId,
    AwsSecretKey,
    GitHubToken,
    OpenAiKey,
    AnthropicKey,
    GoogleApiKey,
    SlackToken,
    StripeKey,
    HuggingFaceToken,
    Jwt,
    /// A password in a URL (`scheme://user:password@host`).
    UrlCredentials,
    /// A literal assigned to a password-like name.
    Password,
    /// A random-looking literal assigned to a token/key/secret-like name, or
    /// such a `.env`-style line.
    SecretAssignment,
}

impl SecretKind {
    pub fn label(self) -> &'static str {
        match self {
            SecretKind::PrivateKey => "private key",
            SecretKind::AwsAccessKeyId => "AWS access key ID",
            SecretKind::AwsSecretKey => "AWS secret access key",
            SecretKind::GitHubToken => "GitHub token",
            SecretKind::OpenAiKey => "OpenAI API key",
            SecretKind::AnthropicKey => "Anthropic API key",
            SecretKind::GoogleApiKey => "Google API key",
            SecretKind::SlackToken => "Slack token",
            SecretKind::StripeKey => "Stripe live key",
            SecretKind::HuggingFaceToken => "Hugging Face token",
            SecretKind::Jwt => "JSON Web Token",
            SecretKind::UrlCredentials => "password in a URL",
            SecretKind::Password => "password",
            SecretKind::SecretAssignment => "API key or token",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WarningKind {
    Secret(SecretKind),
    /// The user's home directory path.
    HomePath,
    LargeOutput,
    LargeNotebook,
    WidgetState,
}

/// Where in the notebook.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Place {
    Source,
    /// The output at this index in the cell's outputs.
    Output(usize),
    CellMetadata,
    NotebookMetadata,
    /// The notebook as a whole (size).
    Notebook,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    /// Cell index; `None` for notebook-level places.
    pub cell: Option<usize>,
    pub cell_id: Option<String>,
    pub place: Place,
    /// 1-based line of the first occurrence, for [`Place::Source`].
    pub line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub kind: WarningKind,
    pub location: Location,
    /// What was found, safe to display: secrets redacted (`ghp_…`), paths
    /// shortened. Empty for size warnings.
    pub excerpt: String,
    /// Occurrences of this kind at this location.
    pub count: usize,
    /// Size for [`WarningKind::LargeOutput`], [`WarningKind::LargeNotebook`],
    /// [`WarningKind::WidgetState`].
    pub bytes: Option<usize>,
}

impl Warning {
    pub fn is_secret(&self) -> bool {
        matches!(self.kind, WarningKind::Secret(_))
    }

    /// Gone if outputs are stripped before publishing.
    pub fn removed_by_strip_outputs(&self) -> bool {
        matches!(self.location.place, Place::Output(_)) || self.kind == WarningKind::WidgetState
    }

    /// One sentence for the publish dialog, e.g. "GitHub token (ghp_…) in
    /// cell 3 source, line 2".
    pub fn message(&self) -> String {
        let place = match (self.location.cell, self.location.place) {
            (Some(cell), Place::Source) => match self.location.line {
                Some(line) => format!("cell {} source, line {line}", cell + 1),
                None => format!("cell {} source", cell + 1),
            },
            (Some(cell), Place::Output(_)) => format!("cell {} output", cell + 1),
            (Some(cell), Place::CellMetadata) => format!("cell {} metadata", cell + 1),
            (_, Place::NotebookMetadata) => "notebook metadata".to_string(),
            _ => "the notebook".to_string(),
        };
        let times = if self.count > 1 {
            format!(" ({} times)", self.count)
        } else {
            String::new()
        };
        let size = self.bytes.map(human_bytes).unwrap_or_default();
        match self.kind {
            WarningKind::Secret(kind) => {
                let label = kind.label();
                let mut label = label.to_string();
                if let Some(first) = label.get_mut(..1) {
                    first.make_ascii_uppercase();
                }
                format!("{label} ({}) in {place}{times}", self.excerpt)
            }
            WarningKind::HomePath => format!("Local path {} in {place}{times}", self.excerpt),
            WarningKind::LargeOutput => format!("Large output ({size}) in {place}"),
            WarningKind::LargeNotebook => format!(
                "The notebook is large ({size}); it may be slow to open or rejected by GitHub"
            ),
            WarningKind::WidgetState => format!("Saved widget state ({size}) will be published"),
        }
    }
}

fn human_bytes(n: usize) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else {
        format!("{} KB", n.div_ceil(1024))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanOptions {
    /// Home directory to look for; `None` skips the check. Default: `$HOME`.
    pub home_dir: Option<String>,
    /// Default 1 MiB.
    pub large_output_bytes: usize,
    /// Default 10 MiB.
    pub large_notebook_bytes: usize,
    /// Scan outputs and widget state (`false` when they'll be stripped).
    pub include_outputs: bool,
    /// Stop collecting after this many warnings (the rest are counted in
    /// [`ScanReport::omitted`]). Default 500.
    pub max_warnings: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            home_dir: std::env::var("HOME").ok(),
            large_output_bytes: 1 << 20,
            large_notebook_bytes: 10 << 20,
            include_outputs: true,
            max_warnings: 500,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// In notebook order (notebook-level warnings first).
    pub warnings: Vec<Warning>,
    /// Estimated size of the published file (text and payload bytes, without
    /// JSON punctuation).
    pub notebook_bytes: usize,
    /// Warnings beyond [`ScanOptions::max_warnings`].
    pub omitted: usize,
}

impl ScanReport {
    pub fn has_secrets(&self) -> bool {
        self.warnings.iter().any(Warning::is_secret)
    }

    pub fn is_empty(&self) -> bool {
        self.warnings.is_empty() && self.omitted == 0
    }
}

/// Scan a notebook before publishing. Scan what will actually be uploaded:
/// pass `include_outputs: false` when stripping outputs.
pub fn scan(nb: &Notebook, opts: &ScanOptions) -> ScanReport {
    let home = opts
        .home_dir
        .as_deref()
        .map(|h| h.trim_end_matches(['/', '\\']))
        .filter(|h| useful_home(h));
    let s = Scanner { home };
    let mut notebook_level = Vec::new();
    let mut warnings = Vec::new();

    let mut total = 0usize;
    let mut meta = nb.metadata.clone();
    meta.shift_remove(crate::meta::META_NAMESPACE);
    let widgets = meta.shift_remove("widgets");
    if let Some(widgets) = widgets.filter(|_| opts.include_outputs) {
        let bytes = json_size(&widgets);
        total += bytes;
        notebook_level.push(Warning {
            kind: WarningKind::WidgetState,
            location: Location {
                cell: None,
                cell_id: None,
                place: Place::NotebookMetadata,
                line: None,
            },
            excerpt: String::new(),
            count: 1,
            bytes: Some(bytes),
        });
    }
    if !meta.is_empty() {
        let text = serde_json::to_string(&Value::Object(meta)).unwrap_or_default();
        total += text.len();
        let mut hits = Hits::default();
        s.scan_text(&text, false, &mut hits);
        let location = Location {
            cell: None,
            cell_id: None,
            place: Place::NotebookMetadata,
            line: None,
        };
        notebook_level.extend(hits.into_warnings(&location));
    }

    for (index, cell) in nb.cells.iter().enumerate() {
        let location = |place| Location {
            cell: Some(index),
            cell_id: cell.id.clone(),
            place,
            line: None,
        };
        total += cell.source.len();
        let mut hits = Hits::default();
        s.scan_text(&cell.source, true, &mut hits);
        warnings.extend(hits.into_warnings(&location(Place::Source)));

        if !cell.metadata.is_empty() {
            let text = serde_json::to_string(&cell.metadata).unwrap_or_default();
            total += text.len();
            let mut hits = Hits::default();
            s.scan_text(&text, false, &mut hits);
            warnings.extend(hits.into_warnings(&location(Place::CellMetadata)));
        }
        if let Some(attachments) = &cell.attachments {
            total += json_size(attachments);
        }
        if !opts.include_outputs {
            continue;
        }
        for (oi, output) in cell.outputs.iter().enumerate() {
            let mut hits = Hits::default();
            let bytes = s.scan_output(output, &mut hits);
            total += bytes;
            let loc = location(Place::Output(oi));
            warnings.extend(hits.into_warnings(&loc));
            if bytes > opts.large_output_bytes {
                warnings.push(Warning {
                    kind: WarningKind::LargeOutput,
                    location: loc,
                    excerpt: String::new(),
                    count: 1,
                    bytes: Some(bytes),
                });
            }
        }
    }

    if total > opts.large_notebook_bytes {
        notebook_level.insert(
            0,
            Warning {
                kind: WarningKind::LargeNotebook,
                location: Location {
                    cell: None,
                    cell_id: None,
                    place: Place::Notebook,
                    line: None,
                },
                excerpt: String::new(),
                count: 1,
                bytes: Some(total),
            },
        );
    }
    // Notebook-level warnings are few and important: they go first and are kept.
    notebook_level.append(&mut warnings);
    let mut warnings = notebook_level;
    let omitted = warnings.len().saturating_sub(opts.max_warnings);
    warnings.truncate(opts.max_warnings);
    ScanReport {
        warnings,
        notebook_bytes: total,
        omitted,
    }
}

/// An absolute path below the root (`/root` is fine, `/` or `` is not).
fn useful_home(home: &str) -> bool {
    (home.len() > 1 && home.starts_with(['/', '\\']))
        || (home.len() > 3 && home.get(1..3) == Some(":\\"))
}

struct Scanner<'a> {
    home: Option<&'a str>,
}

impl Scanner<'_> {
    /// Scan one output; returns its size.
    fn scan_output(&self, output: &Output, hits: &mut Hits) -> usize {
        match output {
            Output::Stream { text, .. } => {
                self.scan_text(text, false, hits);
                text.len()
            }
            Output::Error {
                ename,
                evalue,
                traceback,
            } => {
                let mut text = format!("{ename}: {evalue}\n");
                for line in traceback {
                    text.push_str(line);
                    text.push('\n');
                }
                self.scan_text(&text, false, hits);
                text.len()
            }
            Output::DisplayData { data, metadata, .. }
            | Output::ExecuteResult { data, metadata, .. } => {
                let mut bytes = 0;
                for (mime, payload) in data.iter() {
                    let skip = mime.starts_with("image/")
                        || mime == "application/pdf"
                        || mime.starts_with("application/vnd.jupyter.widget");
                    match payload {
                        MimeData::Text(text) => {
                            bytes += text.len();
                            if !skip {
                                self.scan_text(text, false, hits);
                            }
                        }
                        MimeData::Json(value) => {
                            bytes += json_size(value);
                            if !skip {
                                let mut joined = String::new();
                                collect_strings(value, &mut joined);
                                self.scan_text(&joined, false, hits);
                            }
                        }
                    }
                }
                if !metadata.is_empty() {
                    let text = serde_json::to_string(metadata).unwrap_or_default();
                    bytes += text.len();
                    self.scan_text(&text, false, hits);
                }
                bytes
            }
            Output::Unknown(map) => {
                let text = serde_json::to_string(map).unwrap_or_default();
                self.scan_text(&text, false, hits);
                text.len()
            }
        }
    }

    fn scan_text(&self, text: &str, lines: bool, hits: &mut Hits) {
        if text.is_empty() {
            return;
        }
        let mut claims = Claims::default();
        let line_of = |offset: usize| {
            lines.then(|| {
                text.as_bytes()[..offset]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count()
                    + 1
            })
        };

        for rule in TOKEN_RULES.iter() {
            for m in rule.re.find_iter(text) {
                if claims.claim(m.start(), m.end()) {
                    hits.add(WarningKind::Secret(rule.kind), || {
                        let excerpt = if rule.reveal == usize::MAX {
                            m.as_str().to_string()
                        } else {
                            format!("{}…", &m.as_str()[..rule.reveal.min(m.len())])
                        };
                        (excerpt, line_of(m.start()))
                    });
                }
            }
        }

        for c in AWS_SECRET.captures_iter(text) {
            let value = c.get(1).expect("group 1");
            if claims.claim(value.start(), value.end()) {
                let name = c.get(0).map_or("", |m| &text[m.start()..value.start()]);
                let name = one_line(name.trim_end_matches(['"', '\'', ' ', '\t']));
                hits.add(WarningKind::Secret(SecretKind::AwsSecretKey), || {
                    (format!("{name} …"), line_of(value.start()))
                });
            }
        }

        for c in URL_CREDENTIALS.captures_iter(text) {
            let (user, pass) = (c.get(1).expect("group 1"), c.get(2).expect("group 2"));
            if is_placeholder(pass.as_str())
                || pass.len() < 3
                || !claims.claim(pass.start(), pass.end())
            {
                continue;
            }
            let start = c.get(0).map_or(user.start(), |m| m.start());
            let excerpt = format!("{}:…@", &text[start..user.end()]);
            hits.add(WarningKind::Secret(SecretKind::UrlCredentials), || {
                (shorten(&excerpt, 80), line_of(start))
            });
        }

        for (name, value) in quoted_assignments(text) {
            let (name_str, value_str) = (&text[name.clone()], &text[value.clone()]);
            let Some(kind) = classify_name(name_str) else {
                continue;
            };
            if !value_looks_secret(kind, value_str) || !claims.claim(value.start, value.end) {
                continue;
            }
            let excerpt = format!("{} = \"…\"", shorten(name_str, 60));
            hits.add(WarningKind::Secret(kind), || (excerpt, line_of(name.start)));
        }

        for c in ENV_LINE.captures_iter(text) {
            let (name, value) = (c.get(1).expect("group 1"), c.get(2).expect("group 2"));
            let Some(kind) = classify_name(name.as_str()) else {
                continue;
            };
            let code_like = value
                .as_str()
                .contains(['(', ')', '[', ']', '{', '}', '$', ',', '`']);
            if code_like
                || !value_looks_secret(kind, value.as_str())
                || !claims.claim(value.start(), value.end())
            {
                continue;
            }
            let excerpt = format!("{}=…", shorten(name.as_str(), 60));
            hits.add(WarningKind::Secret(kind), || {
                (excerpt, line_of(name.start()))
            });
        }

        if let Some(home) = self.home {
            for (offset, _) in text.match_indices(home) {
                let after = &text[offset + home.len()..];
                // `/Users/al` must not match `/Users/alice`.
                if after.starts_with(|c: char| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
                {
                    continue;
                }
                hits.add(WarningKind::HomePath, || {
                    let end = after
                        .find(|c: char| {
                            c.is_whitespace()
                                || matches!(
                                    c,
                                    '"' | '\'' | '<' | '>' | '`' | ')' | ',' | ';' | '\x1b'
                                )
                        })
                        .unwrap_or(after.len());
                    (
                        shorten(&format!("{home}{}", &after[..end]), 100),
                        line_of(offset),
                    )
                });
            }
        }
    }
}

/// Per-place hit aggregation: kind → (first excerpt, first line, count).
#[derive(Default)]
struct Hits {
    by_kind: HashMap<WarningKind, (String, Option<usize>, usize)>,
}

impl Hits {
    fn add(&mut self, kind: WarningKind, first: impl FnOnce() -> (String, Option<usize>)) {
        self.by_kind
            .entry(kind)
            .and_modify(|e| e.2 += 1)
            .or_insert_with(|| {
                let (excerpt, line) = first();
                (excerpt, line, 1)
            });
    }

    fn into_warnings(self, location: &Location) -> Vec<Warning> {
        let mut out: Vec<Warning> = self
            .by_kind
            .into_iter()
            .map(|(kind, (excerpt, line, count))| Warning {
                kind,
                location: Location {
                    line,
                    ..location.clone()
                },
                excerpt,
                count,
                bytes: None,
            })
            .collect();
        out.sort_by_key(|w| (w.location.line, w.kind));
        out
    }
}

/// Byte ranges already attributed to a (more specific) rule in one text.
#[derive(Default)]
struct Claims {
    spans: Vec<(usize, usize)>,
}

impl Claims {
    /// Claim `[start, end)` unless it overlaps an earlier claim.
    fn claim(&mut self, start: usize, end: usize) -> bool {
        let i = self.spans.partition_point(|&(s, _)| s < start);
        let overlaps_prev = i > 0 && self.spans[i - 1].1 > start;
        let overlaps_next = i < self.spans.len() && self.spans[i].0 < end;
        if overlaps_prev || overlaps_next {
            return false;
        }
        if self.spans.len() < 4096 {
            self.spans.insert(i, (start, end));
        }
        true
    }
}

struct TokenRule {
    kind: SecretKind,
    re: Regex,
    /// Characters shown in the excerpt (the fixed prefix); `usize::MAX`: all.
    reveal: usize,
}

// ASCII word boundaries (`(?-u:\b)`) keep every regex on the fast DFA path
// even for non-ASCII text.
static TOKEN_RULES: LazyLock<Vec<TokenRule>> = LazyLock::new(|| {
    let rule = |kind, pattern: &str, reveal| TokenRule {
        kind,
        re: Regex::new(pattern).expect("valid regex"),
        reveal,
    };
    vec![
        rule(
            SecretKind::PrivateKey,
            r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
            usize::MAX,
        ),
        rule(
            SecretKind::AnthropicKey,
            r"(?-u:\b)sk-ant-[A-Za-z0-9_-]{20,}",
            7,
        ),
        rule(
            SecretKind::OpenAiKey,
            r"(?-u:\b)sk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{20,}|(?-u:\b)sk-[A-Za-z0-9]{32,}(?-u:\b)",
            3,
        ),
        rule(
            SecretKind::GitHubToken,
            r"(?-u:\b)(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})(?-u:\b)",
            4,
        ),
        rule(
            SecretKind::AwsAccessKeyId,
            r"(?-u:\b)(?:AKIA|ASIA|ABIA|ACCA)[0-9A-Z]{16}(?-u:\b)",
            4,
        ),
        rule(
            SecretKind::GoogleApiKey,
            r"(?-u:\b)AIza[0-9A-Za-z_-]{35}",
            4,
        ),
        rule(
            SecretKind::SlackToken,
            r"(?-u:\b)xox[abposr]-[A-Za-z0-9-]{10,}",
            5,
        ),
        rule(
            SecretKind::StripeKey,
            r"(?-u:\b)[sr]k_live_[A-Za-z0-9]{20,}",
            8,
        ),
        rule(
            SecretKind::HuggingFaceToken,
            r"(?-u:\b)hf_[A-Za-z0-9]{30,}(?-u:\b)",
            3,
        ),
        rule(
            SecretKind::Jwt,
            r"(?-u:\b)eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            3,
        ),
    ]
});

static AWS_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)aws_?secret_?(?:access_?)?key["']?\]?[ \t]*[:=][ \t]*["']?([A-Za-z0-9/+]{40})(?:[^A-Za-z0-9/+=]|$)"#)
        .expect("valid regex")
});

static URL_CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?-u:\b)[A-Za-z][A-Za-z0-9+.-]{1,15}://([^\s:/@'"<>]{1,100}):([^\s@/'"<>]{1,200})@[A-Za-z0-9\[]"#)
        .expect("valid regex")
});

/// A word a secret's variable name contains (anchors the assignment search).
static SECRET_NAME_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)password|passwd|passphrase|pwd|secret|token|credential|bearer|apikey|(?:api|access|private|auth|client|signing|encryption|master)[_-]?key",
    )
    .expect("valid regex")
});

/// `name = "value"`, `name: 'value'`, `"name": "value"`, `env["NAME"] = "value"`,
/// `f(name="value")` for names containing a [`SECRET_NAME_WORD`]: (name, value)
/// byte ranges. Hand-parsed from each keyword hit outwards, which stays linear
/// where one big regex falls off the DFA on long random text.
fn quoted_assignments(text: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let b = text.as_bytes();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let blank = |c: u8| c == b' ' || c == b'\t';
    let mut out = Vec::new();
    let mut prev_end = 0;
    for m in SECRET_NAME_WORD.find_iter(text) {
        let (mut start, mut end) = (m.start(), m.end());
        while start > 0 && ident(b[start - 1]) {
            start -= 1;
        }
        while end < b.len() && ident(b[end]) {
            end += 1;
        }
        if end <= prev_end {
            continue; // another keyword of the same identifier
        }
        prev_end = end;
        if end - start > 81 || !(b[start].is_ascii_alphabetic() || b[start] == b'_') {
            continue;
        }
        // name["']?]? : or = (not ==), then an optional r/b/u/f prefix and a quote.
        let mut i = end;
        if i < b.len() && matches!(b[i], b'"' | b'\'') {
            i += 1;
        }
        if i < b.len() && b[i] == b']' {
            i += 1;
        }
        while i < b.len() && blank(b[i]) {
            i += 1;
        }
        match b.get(i) {
            Some(b'=') if b.get(i + 1) != Some(&b'=') => i += 1,
            Some(b':') => i += if b.get(i + 1) == Some(&b'=') { 2 } else { 1 },
            _ => continue,
        }
        while i < b.len() && blank(b[i]) {
            i += 1;
        }
        let mut j = i;
        while j < b.len()
            && j - i < 2
            && matches!(b[j], b'r' | b'R' | b'b' | b'B' | b'u' | b'U' | b'f' | b'F')
        {
            j += 1;
        }
        if !matches!(b.get(j), Some(b'"' | b'\'')) {
            continue;
        }
        let quote = b[j];
        let value_start = j + 1;
        let value_end = b[value_start..]
            .iter()
            .take(301)
            .position(|&c| c == quote || c == b'\n' || c == b'\r')
            .map(|p| value_start + p);
        if let Some(value_end) = value_end
            && b[value_end] == quote
            && value_end - value_start >= 4
        {
            out.push((start..end, value_start..value_end));
        }
    }
    out
}

/// `.env` / shell / `%env` lines with an unquoted value: `NAME=value`.
static ENV_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^[ \t]*(?:export[ \t]+|%env[ \t]+|%set_env[ \t]+|!export[ \t]+)?([A-Z][A-Z0-9_]{1,80})=([^\s"'#]{4,300})"#)
        .expect("valid regex")
});

/// Split an identifier into lowercase words (`OPENAI_API_KEY`, `apiKey`).
fn words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in name
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
    {
        let mut word = String::new();
        let mut prev_lower = false;
        for c in part.chars() {
            if c.is_ascii_uppercase() && prev_lower && !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
            word.push(c.to_ascii_lowercase());
        }
        out.push(word);
    }
    out
}

/// Is this a name a secret would be stored under?
fn classify_name(name: &str) -> Option<SecretKind> {
    let w = words(name);
    let has = |s: &str| w.iter().any(|x| x == s);
    let pair = |a: &str, b: &str| w.windows(2).any(|p| p[0] == a && p[1] == b);
    if has("password") || has("passwd") || has("passphrase") || has("pwd") {
        return Some(SecretKind::Password);
    }
    let secretish = has("secret")
        || has("token")
        || has("apikey")
        || has("credential")
        || has("credentials")
        || has("bearer")
        || [
            "api",
            "access",
            "private",
            "auth",
            "client",
            "signing",
            "encryption",
            "master",
        ]
        .iter()
        .any(|a| pair(a, "key"));
    secretish.then_some(SecretKind::SecretAssignment)
}

const PLACEHOLDER_PARTS: &[&str] = &[
    "your",
    "example",
    "placeholder",
    "redacted",
    "changeme",
    "change_me",
    "xxxx",
    "****",
    "....",
    "…",
    "<",
    ">",
    "${",
    "{{",
    "%(",
    "insert",
    "replace",
    "dummy",
    "fake",
    "sample",
    "_here",
    "-here",
    "todo",
    "fixme",
];
const PLACEHOLDER_VALUES: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "none",
    "null",
    "nil",
    "true",
    "false",
    "undefined",
    "required",
    "optional",
];

fn is_placeholder(value: &str) -> bool {
    let v = value.trim().to_ascii_lowercase();
    if v.is_empty() || v.starts_with(['$', '%', '{', '[', '@']) {
        return true;
    }
    let mut chars = v.chars();
    let first = chars.next();
    if chars.all(|c| Some(c) == first) {
        return true;
    }
    PLACEHOLDER_VALUES.contains(&v.as_str()) || PLACEHOLDER_PARTS.iter().any(|p| v.contains(p))
}

fn value_looks_secret(kind: SecretKind, value: &str) -> bool {
    if is_placeholder(value) || value.chars().any(char::is_whitespace) {
        return false;
    }
    let path_like = value.starts_with(['/', '~', '\\'])
        || value.starts_with("./")
        || value.starts_with("../")
        || value.contains("://")
        || value.get(1..3) == Some(":\\");
    if path_like {
        return false;
    }
    // Code punctuation means the "value" spans the end of one string literal
    // and the start of the next (`"?api_key="],tileSize:256}"` in minified JS).
    let code_punctuation = value.contains(['{', '}', '[', ']', '(', ')', '<', '>', ',', ';', '`']);
    match kind {
        SecretKind::Password => value.len() >= 4 && !code_punctuation,
        _ => {
            const FILE_EXTS: &[&str] = &[
                ".json", ".txt", ".pem", ".key", ".yaml", ".yml", ".csv", ".cfg", ".ini", ".env",
                ".toml", ".p12", ".crt",
            ];
            let lower = value.to_ascii_lowercase();
            // Keys and tokens are random strings over a small alphabet.
            let token_chars = value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-+/=.~".contains(&b));
            value.len() >= 12
                && token_chars
                && value.bytes().any(|b| b.is_ascii_digit())
                && value.bytes().any(|b| b.is_ascii_alphabetic())
                && !FILE_EXTS.iter().any(|ext| lower.ends_with(ext))
        }
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shorten(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Every string in a JSON value, newline-separated.
fn collect_strings(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

/// Serialized size of a JSON value, without allocating it.
fn json_size(value: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_words() {
        assert_eq!(words("OPENAI_API_KEY"), ["openai", "api", "key"]);
        assert_eq!(words("apiKey"), ["api", "key"]);
        assert_eq!(words("githubToken2"), ["github", "token2"]);
        assert_eq!(words("db-password"), ["db", "password"]);
        assert_eq!(
            classify_name("OPENAI_API_KEY"),
            Some(SecretKind::SecretAssignment)
        );
        assert_eq!(classify_name("db_password"), Some(SecretKind::Password));
        assert_eq!(
            classify_name("accessToken"),
            Some(SecretKind::SecretAssignment)
        );
        for not_secret in [
            "tokenizer",
            "max_tokens",
            "monkey",
            "key",
            "dict_key",
            "secrets_dir",
            "url",
            "passport",
        ] {
            assert_eq!(classify_name(not_secret), None, "{not_secret}");
        }
    }

    #[test]
    fn placeholders() {
        for p in [
            "<your-api-key>",
            "xxxxxxxx",
            "********",
            "${OPENAI_API_KEY}",
            "YOUR_TOKEN_HERE",
            "changeme",
            "None",
            "{token}",
        ] {
            assert!(is_placeholder(p), "{p}");
        }
        assert!(!is_placeholder("hunter22"));
        assert!(value_looks_secret(SecretKind::Password, "hunter22"));
        assert!(
            !value_looks_secret(SecretKind::SecretAssignment, "hunter22"),
            "too short for a key"
        );
        assert!(
            !value_looks_secret(SecretKind::SecretAssignment, "bert-base-uncased"),
            "no digits"
        );
        assert!(
            !value_looks_secret(SecretKind::SecretAssignment, "service-account-2024.json"),
            "a file name"
        );
        assert!(
            !value_looks_secret(SecretKind::Password, "/home/user/x"),
            "a path"
        );
        assert!(value_looks_secret(
            SecretKind::SecretAssignment,
            "a8f5f167f44f4964e6c998dee827110c"
        ));
        assert!(
            !value_looks_secret(
                SecretKind::SecretAssignment,
                "],tileSize:256}},layers:[{id:"
            ),
            "JS between literals"
        );
        assert!(
            !value_looks_secret(SecretKind::Password, "),a(1);b("),
            "JS between literals"
        );
    }

    #[test]
    fn claims_reject_overlaps() {
        let mut c = Claims::default();
        assert!(c.claim(10, 20));
        assert!(!c.claim(15, 25));
        assert!(!c.claim(5, 11));
        assert!(!c.claim(12, 18));
        assert!(c.claim(20, 30));
        assert!(c.claim(0, 10));
        assert!(!c.claim(0, 40));
    }
}

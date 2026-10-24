//! Completion and docs in the cell editor. Completion comes as you type
//! (Settings ▸ Code Completion), and Tab takes the suggestion. The running
//! kernel answers first (`complete_request`: the names really defined, an
//! object's real attributes); the notebook's own text fills in the rest and
//! answers alone when no kernel runs: every name the notebook defines or
//! uses, the attributes it has used on each name (`df.groupby`), the modules
//! it imports, and Python's keywords and builtins. Completion never starts a
//! kernel. Docs on hover come from the kernel (`inspect_request`).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use gpui_kit::component::input::{CompletionProvider, HoverProvider, Rope, RopeExt as _};
use gpui_kit::{App, AppContext as _, Task, WeakEntity, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    Hover, HoverContents, MarkupContent, MarkupKind, TextEdit,
};

use super::NotebookView;

/// Kernels queue completion behind a running cell; don't hang the menu on it.
const TIMEOUT: Duration = Duration::from_millis(1500);

pub struct KernelIntellisense {
    pub notebook: WeakEntity<NotebookView>,
}

impl KernelIntellisense {
    fn kernel(&self, cx: &App) -> Option<herse_kernel::Kernel> {
        let nb = self.notebook.upgrade()?;
        let session = nb.read(cx).session.as_ref()?;
        Some(session.kernel.clone())
    }
}

fn kind_for(t: &str) -> Option<CompletionItemKind> {
    Some(match t {
        "function" | "builtin_function_or_method" => CompletionItemKind::FUNCTION,
        "module" => CompletionItemKind::MODULE,
        "class" | "type" => CompletionItemKind::CLASS,
        "keyword" => CompletionItemKind::KEYWORD,
        "statement" | "instance" => CompletionItemKind::VARIABLE,
        "property" => CompletionItemKind::PROPERTY,
        "path" => CompletionItemKind::FILE,
        "param" => CompletionItemKind::VARIABLE,
        _ => return None,
    })
}

impl CompletionProvider for KernelIntellisense {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let code = rope.to_string();
        // The notebook's own words, worked out here (the index is cached).
        let local = self
            .notebook
            .upgrade()
            .map(|nb| nb.read(cx).word_index())
            .map(|index| local_items(&index, &code, offset, rope))
            .unwrap_or_default();
        let Some(kernel) = self.kernel(cx) else {
            return Task::ready(Ok(CompletionResponse::Array(local)));
        };
        let cursor = herse_kernel::byte_to_cursor_pos(&code, offset);
        let rx = kernel.complete(&code, cursor);
        let rope = rope.clone();
        cx.background_spawn(async move {
            let reply = smol::future::or(async { rx.recv().await.ok() }, async {
                smol_timer(TIMEOUT).await;
                None
            })
            .await;
            let Some(Ok(reply)) = reply else {
                return Ok(CompletionResponse::Array(local));
            };
            let start = herse_kernel::cursor_pos_to_byte(&code, reply.cursor_start);
            let end = herse_kernel::cursor_pos_to_byte(&code, reply.cursor_end);
            let range = lsp_types::Range::new(rope.offset_to_position(start), rope.offset_to_position(end));
            // IPython describes each match in an experimental metadata list.
            let types = reply
                .metadata
                .get("_jupyter_types_experimental")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let items = reply
                .matches
                .iter()
                .enumerate()
                .map(|(i, m)| {
                    let meta = types.get(i);
                    let kind = meta
                        .and_then(|t| t.get("type"))
                        .and_then(|t| t.as_str())
                        .and_then(kind_for);
                    let detail = meta
                        .and_then(|t| t.get("signature"))
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string);
                    CompletionItem {
                        label: m.clone(),
                        kind,
                        detail,
                        sort_text: Some(format!("{i:05}")),
                        filter_text: Some(m.clone()),
                        text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                            range,
                            new_text: m.clone(),
                        })),
                        ..Default::default()
                    }
                })
                .collect::<Vec<CompletionItem>>();
            // The kernel knows what's defined now; the notebook adds what's
            // written but not run yet.
            let mut items = items;
            let known: HashSet<String> = items.iter().map(|i| i.label.clone()).collect();
            items.extend(local.into_iter().filter(|i| !known.contains(&i.label)).take(30));
            Ok(CompletionResponse::Array(items))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        // As you type: after a `.`, and on each letter of a name (the menu
        // refines as the name grows). Off in Settings: Tab asks.
        if !crate::settings::complete_as_you_type() {
            return false;
        }
        let mut chars = new_text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => c == '.' || c.is_alphanumeric() || c == '_',
            _ => false,
        }
    }
}

impl HoverProvider for KernelIntellisense {
    fn hover(
        &self,
        rope: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        let Some(kernel) = self.kernel(cx) else {
            return Task::ready(Ok(None));
        };
        let code = rope.to_string();
        let cursor = herse_kernel::byte_to_cursor_pos(&code, offset);
        let rx = kernel.inspect(&code, cursor);
        cx.background_spawn(async move {
            let reply = smol::future::or(async { rx.recv().await.ok() }, async {
                smol_timer(TIMEOUT).await;
                None
            })
            .await;
            let Some(Ok(reply)) = reply else { return Ok(None) };
            if !reply.found {
                return Ok(None);
            }
            let text = reply
                .data
                .get("text/plain")
                .and_then(|v| v.as_str())
                .map(|t| herse_rich::ansi::strip(t).into_owned())
                .unwrap_or_default();
            if text.trim().is_empty() {
                return Ok(None);
            }
            // Docstrings are long; the first screenful is what a hover is for.
            let mut shown: String = text.lines().take(40).collect::<Vec<_>>().join("\n");
            if shown.len() > 4000 {
                shown.truncate(shown.floor_char_boundary(4000));
            }
            Ok(Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: format!("```text\n{shown}\n```"),
                }),
                range: None,
            }))
        })
    }
}

/// What the notebook's code says about its names: each name with how often
/// it appears, the attributes used on each (`df` → `groupby`, `head`), and the
/// modules it imports. Built from the code cells, cached per edit.
#[derive(Default)]
pub struct WordIndex {
    words: Vec<(String, usize)>,
    attrs: HashMap<String, Vec<String>>,
    modules: Vec<String>,
}

impl WordIndex {
    pub fn build<'a>(sources: impl Iterator<Item = &'a str>) -> WordIndex {
        let mut counts: HashMap<String, usize> = HashMap::new();
        let mut attrs: HashMap<String, HashSet<String>> = HashMap::new();
        let mut modules: Vec<String> = Vec::new();
        for source in sources {
            for m in super::preflight::imports(source) {
                if !modules.contains(&m) {
                    modules.push(m);
                }
            }
            let mut prev: Option<String> = None;
            let mut after_dot = false;
            for token in tokens(source) {
                match token {
                    Token::Name(name) => {
                        if after_dot && let Some(obj) = &prev {
                            attrs.entry(obj.clone()).or_default().insert(name.to_string());
                        } else if name.len() >= 2 {
                            *counts.entry(name.to_string()).or_default() += 1;
                        }
                        prev = Some(name.to_string());
                        after_dot = false;
                    }
                    Token::Dot => after_dot = prev.is_some(),
                    Token::Other => {
                        prev = None;
                        after_dot = false;
                    }
                }
            }
        }
        let mut words: Vec<(String, usize)> = counts.into_iter().collect();
        words.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let attrs = attrs
            .into_iter()
            .map(|(k, v)| {
                let mut v: Vec<String> = v.into_iter().collect();
                v.sort();
                (k, v)
            })
            .collect();
        WordIndex { words, attrs, modules }
    }
}

enum Token<'a> {
    Name(&'a str),
    Dot,
    Other,
}

/// Names, dots and the rest, skipping strings and comments.
fn tokens(source: &str) -> impl Iterator<Item = Token<'_>> {
    let b = source.as_bytes();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i < b.len() {
            let c = b[i];
            if c == b'#' {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if c == b'"' || c == b'\'' {
                let q = c;
                let triple = b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q);
                i += if triple { 3 } else { 1 };
                while i < b.len() {
                    if b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == q && (!triple || (b.get(i + 1) == Some(&q) && b.get(i + 2) == Some(&q))) {
                        i += if triple { 3 } else { 1 };
                        break;
                    }
                    if b[i] == b'\n' && !triple {
                        break;
                    }
                    i += 1;
                }
                return Some(Token::Other);
            }
            if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
                let start = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] >= 0x80) {
                    i += 1;
                }
                while i < b.len() && !source.is_char_boundary(i) {
                    i += 1;
                }
                return Some(Token::Name(&source[start..i]));
            }
            i += 1;
            if c == b'.' {
                return Some(Token::Dot);
            }
            if !c.is_ascii_whitespace() {
                return Some(Token::Other);
            }
        }
        None
    })
}

const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del",
    "elif", "else", "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal",
    "not", "or", "pass", "raise", "return", "try", "while", "with", "yield",
];

const BUILTINS: &[&str] = &[
    "abs", "all", "any", "bool", "dict", "dir", "enumerate", "filter", "float", "format", "getattr", "hasattr",
    "help", "id", "input", "int", "isinstance", "issubclass", "iter", "len", "list", "map", "max", "min", "next",
    "object", "open", "print", "range", "repr", "reversed", "round", "set", "setattr", "slice", "sorted", "str",
    "sum", "super", "tuple", "type", "vars", "zip", "Exception", "ValueError", "TypeError", "KeyError",
];

/// Modules worth offering after `import` even before the notebook names them.
const POPULAR_MODULES: &[&str] = &[
    "numpy", "pandas", "polars", "matplotlib", "seaborn", "plotly", "altair", "scipy", "sklearn", "statsmodels",
    "duckdb", "pyarrow", "torch", "tensorflow", "requests", "json", "os", "sys", "re", "math", "pathlib",
    "collections", "itertools", "functools", "datetime", "time", "random", "typing", "dataclasses", "csv",
];

/// Completions from the notebook's text for the name being typed at `offset`.
fn local_items(index: &WordIndex, code: &str, offset: usize, rope: &Rope) -> Vec<CompletionItem> {
    let offset = offset.min(code.len());
    let before = &code[..offset];
    let word_start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(offset, |(i, _)| i);
    let prefix = &code[word_start..offset];
    let line = before[..word_start].rsplit('\n').next().unwrap_or("");
    let line_trim = line.trim_start();
    let mut out: Vec<(String, CompletionItemKind)> = Vec::new();
    if let Some(obj) = line.strip_suffix('.') {
        // `obj.`: what the notebook has used on that name.
        let name: String = obj.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<Vec<_>>().into_iter().rev().collect();
        if let Some(attrs) = index.attrs.get(&name) {
            out.extend(attrs.iter().map(|a| (a.clone(), CompletionItemKind::PROPERTY)));
        }
    } else if (line_trim.starts_with("import ") || line_trim.starts_with("from ")) && !line_trim.contains(" import ") {
        let mut seen = HashSet::new();
        for m in index.modules.iter().map(String::as_str).chain(POPULAR_MODULES.iter().copied()) {
            if seen.insert(m) {
                out.push((m.to_string(), CompletionItemKind::MODULE));
            }
        }
    } else {
        if prefix.is_empty() {
            return Vec::new();
        }
        out.extend(index.words.iter().filter(|(w, n)| w != prefix || *n > 1).map(|(w, _)| (w.clone(), CompletionItemKind::VARIABLE)));
        let have: HashSet<String> = out.iter().map(|(w, _)| w.clone()).collect();
        out.extend(KEYWORDS.iter().filter(|k| !have.contains(**k)).map(|k| (k.to_string(), CompletionItemKind::KEYWORD)));
        out.extend(BUILTINS.iter().filter(|k| !have.contains(**k)).map(|k| (k.to_string(), CompletionItemKind::FUNCTION)));
    }
    let lower = prefix.to_lowercase();
    let range = lsp_types::Range::new(rope.offset_to_position(word_start), rope.offset_to_position(offset));
    out.into_iter()
        .filter(|(w, _)| w.to_lowercase().starts_with(&lower) && w != prefix)
        .take(60)
        .enumerate()
        .map(|(i, (label, kind))| CompletionItem {
            label: label.clone(),
            kind: Some(kind),
            sort_text: Some(format!("~{i:05}")),
            filter_text: Some(label.clone()),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: label })),
            ..Default::default()
        })
        .collect()
}

async fn smol_timer(d: Duration) {
    smol::Timer::after(d).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_names_attributes_and_imports() {
        let index = WordIndex::build(
            ["import pandas as pd\ndf = pd.read_csv('a.csv')  # df.ignored", "df.groupby('x').mean()\ntotal = df.shape"].into_iter(),
        );
        assert_eq!(index.modules, ["pandas"]);
        assert_eq!(index.attrs["df"], ["groupby", "shape"]);
        assert_eq!(index.attrs["pd"], ["read_csv"]);
        assert!(index.words.iter().any(|(w, n)| w == "df" && *n == 3), "{:?}", index.words);
        assert!(!index.words.iter().any(|(w, _)| w == "ignored" || w == "csv"));
    }

    #[test]
    fn completes_from_the_notebook() {
        let index = WordIndex::build(["revenue = 1\nrevenue_by_month = {}\ndf.groupby(1)"].into_iter());
        let labels = |code: &str| {
            let rope = Rope::from(code);
            local_items(&index, code, code.len(), &rope).into_iter().map(|i| i.label).collect::<Vec<_>>()
        };
        assert_eq!(labels("rev"), ["revenue", "revenue_by_month", "reversed"]);
        assert_eq!(labels("df.gr"), ["groupby"]);
        assert!(labels("import pa").contains(&"pandas".to_string()));
        assert!(labels("pri").contains(&"print".to_string()));
        assert!(labels("").is_empty());
    }
}

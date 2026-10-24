//! `stateDiagram` / `stateDiagram-v2` → [`Graph`]: states, `[*]` start and
//! end points (one pair per composite state), transitions with labels,
//! `state "…" as X`, `X : description`, composite states (drawn as
//! clusters) and `<<fork>>` / `<<join>>` / `<<choice>>`. Notes are skipped
//! (with a note on the result); concurrent regions (`--`) are declined.

use super::flowchart::parse_dir;
use super::graph::{Cluster, Detail, Edge, End, Graph, Head, Line, Node, Shape};
use super::label_lines;
use super::layout::Dir;

const FONT: f64 = 14.;

#[derive(Default)]
struct St {
    /// States by id, in order of first mention: (id, descriptions, shape,
    /// enclosing composite).
    states: Vec<(String, Vec<String>, Shape, Option<usize>)>,
    /// Composite states: (id, title, parent).
    composites: Vec<(String, String, Option<usize>)>,
    transitions: Vec<(String, String, String)>,
    scope: Vec<usize>,
    dir: Option<Dir>,
}

impl St {
    fn state(&mut self, id: &str) -> Option<usize> {
        if id.is_empty() || id.contains(char::is_whitespace) || id.contains(['{', '}', '"', ':']) {
            return None;
        }
        if let Some(i) = self.states.iter().position(|s| s.0 == id) {
            return Some(i);
        }
        self.states.push((id.to_string(), Vec::new(), Shape::State, self.scope.last().copied()));
        Some(self.states.len() - 1)
    }

    /// `[*]` resolves to the current scope's start (as a source) or end.
    fn endpoint(&mut self, id: &str, source: bool) -> Option<String> {
        if id != "[*]" {
            self.state(id)?;
            return Some(id.to_string());
        }
        let scope = self.scope.last().map_or("root".to_string(), |c| c.to_string());
        let key = format!("[*]{}:{scope}", if source { "start" } else { "end" });
        if !self.states.iter().any(|s| s.0 == key) {
            let shape = if source { Shape::Start } else { Shape::End };
            self.states.push((key.clone(), Vec::new(), shape, self.scope.last().copied()));
        }
        Some(key)
    }

    fn line(&mut self, l: &str) -> Option<()> {
        if l == "}" {
            self.scope.pop()?;
            return Some(());
        }
        if l == "--" {
            return None;
        }
        if let Some(d) = l.strip_prefix("direction ") {
            let d = parse_dir(d.trim())?;
            if self.scope.is_empty() {
                self.dir = Some(d);
            }
            return Some(());
        }
        for skip in ["classDef ", "class ", "style ", "accTitle", "accDescr", "hide empty description"] {
            if l.starts_with(skip) {
                return Some(());
            }
        }
        if let Some(rest) = l.strip_prefix("state ") {
            return self.declare(rest.trim());
        }
        let l = strip_class(l);
        if let Some(i) = l.find("-->") {
            let from = l[..i].trim();
            let rest = &l[i + 3..];
            let (to, label) = match rest.find(':') {
                Some(c) => (rest[..c].trim(), rest[c + 1..].trim()),
                None => (rest.trim(), ""),
            };
            let from = self.endpoint(strip_class(from), true)?;
            let to = self.endpoint(strip_class(to), false)?;
            self.transitions.push((from, to, label.to_string()));
            return Some(());
        }
        if let Some(c) = l.find(':') {
            let i = self.state(l[..c].trim())?;
            self.states[i].1.push(l[c + 1..].trim().to_string());
            return Some(());
        }
        self.state(l.trim()).map(|_| ())
    }

    fn declare(&mut self, rest: &str) -> Option<()> {
        let (desc, after) = if let Some(q) = rest.strip_prefix('"') {
            let end = q.find('"')?;
            let after = q[end + 1..].trim().strip_prefix("as")?;
            if !after.starts_with(char::is_whitespace) {
                return None;
            }
            (Some(q[..end].to_string()), after.trim())
        } else {
            (None, rest)
        };
        let (id, tail) = match after.find(|c: char| c.is_whitespace() || c == '{' || c == '<' || c == ':') {
            Some(i) => (&after[..i], after[i..].trim()),
            None => (after, ""),
        };
        let id = strip_class(id);
        if tail == "{" {
            let parent = self.scope.last().copied();
            let title = desc.unwrap_or_else(|| id.to_string());
            let k = match self.composites.iter().position(|c| c.0 == id) {
                Some(k) => k,
                None => {
                    self.composites.push((id.to_string(), title, parent));
                    self.composites.len() - 1
                }
            };
            self.scope.push(k);
            return Some(());
        }
        let i = self.state(id)?;
        if let Some(d) = desc {
            self.states[i].1.push(d);
        }
        match tail {
            "" => {}
            "<<fork>>" | "<<join>>" => self.states[i].2 = Shape::Fork,
            "<<choice>>" => self.states[i].2 = Shape::Choice,
            t if t.starts_with(':') => self.states[i].1.push(t[1..].trim().to_string()),
            _ => return None,
        }
        Some(())
    }
}

fn strip_class(s: &str) -> &str {
    match s.find(":::") {
        Some(i) => s[..i].trim(),
        None => s.trim(),
    }
}

pub fn parse(lines: &[String]) -> Option<(Graph, Vec<String>)> {
    let header = lines.first()?.trim();
    if header != "stateDiagram" && header != "stateDiagram-v2" {
        return None;
    }
    let mut st = St::default();
    let mut in_note = false;
    let mut had_notes = false;
    for l in &lines[1..] {
        let l = l.trim().trim_end_matches(';').trim();
        if l.is_empty() {
            continue;
        }
        if in_note {
            if l == "end note" {
                in_note = false;
            }
            continue;
        }
        if l.starts_with("note ") {
            had_notes = true;
            in_note = !l.contains(':');
            continue;
        }
        st.line(l)?;
    }
    if in_note || !st.scope.is_empty() {
        return None;
    }
    // States that turned out to be composites are clusters, not nodes.
    let comp_of = |id: &str| st.composites.iter().position(|c| c.0 == id);
    let mut node_index: Vec<Option<usize>> = Vec::with_capacity(st.states.len());
    let mut nodes = Vec::new();
    for (id, descs, shape, parent) in &st.states {
        if comp_of(id).is_some() {
            node_index.push(None);
            continue;
        }
        node_index.push(Some(nodes.len()));
        let label = match shape {
            Shape::State if descs.is_empty() => label_lines(id, FONT),
            Shape::State => descs.iter().flat_map(|d| label_lines(d, FONT)).collect(),
            _ => Vec::new(),
        };
        nodes.push(Node { id: id.clone(), label, shape: *shape, cluster: *parent, detail: Detail::None });
    }
    let clusters: Vec<Cluster> = st.composites.iter().map(|(id, title, parent)| Cluster { id: id.clone(), title: label_lines(title, FONT), parent: *parent }).collect();
    let end_of = |id: &str| -> Option<End> {
        if let Some(c) = comp_of(id) {
            return Some(End::Cluster(c));
        }
        let i = st.states.iter().position(|s| s.0 == id)?;
        node_index[i].map(End::Node)
    };
    let mut edges = Vec::new();
    for (a, b, label) in &st.transitions {
        edges.push(Edge {
            from: end_of(a)?,
            to: end_of(b)?,
            label: if label.is_empty() { Vec::new() } else { label_lines(label, FONT) },
            line: Line::Solid,
            head: Head::Arrow,
            tail: Head::None,
            minlen: 1,
            end_labels: Default::default(),
        });
    }
    let notes = if had_notes { vec!["notes not shown".to_string()] } else { Vec::new() };
    Some((Graph { dir: st.dir.unwrap_or(Dir::TB), nodes, edges, clusters }, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(src: &str) -> Graph {
        parse(&src.lines().map(String::from).collect::<Vec<_>>()).unwrap_or_else(|| panic!("failed:\n{src}")).0
    }

    #[test]
    fn states_and_transitions() {
        let gr = g("stateDiagram-v2\n  direction LR\n  [*] --> Still\n  Still --> [*]\n  Still --> Moving : push\n  Moving --> Still\n  Moving --> Crash\n  Crash --> [*]\n  state \"Long name\" as LN\n  LN : second line\n  state fork1 <<fork>>");
        assert_eq!(gr.dir, Dir::LR);
        let shapes: Vec<Shape> = gr.nodes.iter().map(|n| n.shape).collect();
        assert_eq!(shapes.iter().filter(|&&s| s == Shape::Start).count(), 1);
        assert_eq!(shapes.iter().filter(|&&s| s == Shape::End).count(), 1);
        let ln = gr.nodes.iter().find(|n| n.id == "LN").unwrap();
        assert_eq!(ln.label, vec!["Long name", "second line"]);
        assert_eq!(gr.edges[2].label, vec!["push"]);
        assert_eq!(gr.edges.len(), 6);
    }

    #[test]
    fn composites_and_pseudo_states() {
        let gr = g("stateDiagram\n  [*] --> First\n  state First {\n    [*] --> second\n    second --> [*]\n  }\n  First --> Done\n  state Done <<choice>>\n  state f <<fork>>\n  note right of First : skipped\n  note left of Done\n    multi\n  end note");
        assert_eq!(gr.clusters.len(), 1);
        assert!(gr.edges.iter().any(|e| e.to == End::Cluster(0)));
        assert!(gr.edges.iter().any(|e| e.from == End::Cluster(0)));
        let inner_start = gr.nodes.iter().filter(|n| n.shape == Shape::Start && n.cluster == Some(0)).count();
        assert_eq!(inner_start, 1);
        assert_eq!(gr.nodes.iter().find(|n| n.id == "Done").unwrap().shape, Shape::Choice);
        assert_eq!(gr.nodes.iter().find(|n| n.id == "f").unwrap().shape, Shape::Fork);
        let (_, notes) = parse(&"stateDiagram\nA --> B\nnote right of A : x".lines().map(String::from).collect::<Vec<_>>()).unwrap();
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn declines() {
        for src in ["stateDiagram\nstate A {\nB --> C\n--\nD --> E\n}", "stateDiagram\nstate A {\nB", "stateDiagram\n}", "stateDiagram\nstate A <<weird>>"] {
            assert!(parse(&src.lines().map(String::from).collect::<Vec<_>>()).is_none(), "{src}");
        }
    }
}

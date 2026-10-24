//! `classDiagram` → [`Graph`]: classes with attributes and methods (from
//! `class X { … }` blocks and `X : member` lines), generics (`~T~`),
//! annotations (`<<interface>>`), relations (`<|--`, `*--`, `o--`, `-->`,
//! `..>`, `..|>`, `--`, `..`, lollipops) with multiplicities and labels,
//! namespaces (drawn as clusters) and notes.

use super::flowchart::parse_dir;
use super::graph::{Cluster, Detail, Edge, End, Graph, Head, Line, Member, Node, Shape};
use super::label_lines;
use super::layout::Dir;

const FONT: f64 = 14.;

#[derive(Default)]
struct Class {
    id: String,
    label: Option<String>,
    /// The name with its generic part (`Square<Shape>`).
    generic: Option<String>,
    annotation: Option<String>,
    attrs: Vec<Member>,
    methods: Vec<Member>,
    namespace: Option<usize>,
}

struct Rel {
    a: usize,
    b: usize,
    tail: Head,
    head: Head,
    dotted: bool,
    cards: [String; 2],
    label: String,
}

#[derive(Default)]
struct P {
    classes: Vec<Class>,
    rels: Vec<Rel>,
    notes: Vec<(String, Option<usize>)>,
    namespaces: Vec<String>,
    /// Open `namespace … {` blocks.
    ns_stack: Vec<usize>,
    /// The class whose `{ … }` body is open.
    body: Option<usize>,
    dir: Option<Dir>,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '~'
}

/// `List~int~` → `List<int>` (a `~` before a name opens, otherwise closes).
fn generics(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '~' && i > 0 {
            let opens = chars.get(i + 1).is_some_and(|n| n.is_alphanumeric() || *n == '_');
            out.push(if opens { '<' } else { '>' });
        } else {
            out.push(c);
        }
    }
    out
}

/// A member line; `*` (abstract) or `$` (static) may follow the `()` or
/// end the line.
fn member(text: &str) -> Member {
    let t = text.trim();
    let mut s = t.to_string();
    let (mut italic, mut underline) = (false, false);
    for (mark, flag) in [('*', &mut italic), ('$', &mut underline)] {
        if let Some(rest) = s.strip_suffix(mark) {
            s = rest.trim_end().to_string();
            *flag = true;
        } else if let Some(i) = s.find(&format!("){mark}")) {
            s.remove(i + 1);
            *flag = true;
        }
    }
    Member { text: generics(s.trim()), italic, underline }
}

impl P {
    fn class(&mut self, name: &str) -> Option<usize> {
        let name = name.trim();
        let id = name.split('~').next().unwrap_or("");
        let generic = name.contains('~').then(|| generics(name));
        if id.is_empty() || !id.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.') {
            return None;
        }
        let i = match self.classes.iter().position(|c| c.id == id) {
            Some(i) => i,
            None => {
                self.classes.push(Class { id: id.to_string(), namespace: self.ns_stack.last().copied(), ..Default::default() });
                self.classes.len() - 1
            }
        };
        if generic.is_some() {
            self.classes[i].generic = generic;
        }
        Some(i)
    }

    fn add_member(&mut self, i: usize, text: &str) {
        let m = member(text);
        if m.text.is_empty() {
            return;
        }
        if m.text.contains('(') {
            self.classes[i].methods.push(m);
        } else {
            self.classes[i].attrs.push(m);
        }
    }

    fn line(&mut self, l: &str) -> Option<()> {
        if let Some(i) = self.body {
            if l == "}" {
                self.body = None;
            } else if let Some(a) = l.strip_prefix("<<").and_then(|a| a.strip_suffix(">>")) {
                self.classes[i].annotation = Some(a.trim().to_string());
            } else {
                self.add_member(i, l);
            }
            return Some(());
        }
        if l == "}" {
            self.ns_stack.pop()?;
            return Some(());
        }
        for skip in ["classDef ", "cssClass ", "style ", "click ", "link ", "callback ", "accTitle", "accDescr"] {
            if l.starts_with(skip) {
                return (!l.ends_with('{')).then_some(());
            }
        }
        if let Some(d) = l.strip_prefix("direction ") {
            self.dir = Some(parse_dir(d.trim())?);
            return Some(());
        }
        if let Some(rest) = l.strip_prefix("namespace ") {
            let name = rest.trim().strip_suffix('{')?.trim();
            if name.is_empty() {
                return None;
            }
            self.namespaces.push(name.to_string());
            self.ns_stack.push(self.namespaces.len() - 1);
            return Some(());
        }
        if let Some(rest) = l.strip_prefix("note") {
            return self.note(rest.trim());
        }
        if let Some(rest) = l.strip_prefix("<<") {
            let end = rest.find(">>")?;
            let i = self.class(rest[end + 2..].trim())?;
            self.classes[i].annotation = Some(rest[..end].trim().to_string());
            return Some(());
        }
        if let Some(rest) = l.strip_prefix("class ") {
            return self.declare(rest.trim());
        }
        if let Some(r) = self.relation(l) {
            return r;
        }
        if let Some(c) = l.find(':') {
            let i = self.class(l[..c].trim())?;
            self.add_member(i, &l[c + 1..]);
            return Some(());
        }
        self.class(l).map(|_| ())
    }

    /// `class Name~T~["Label"]:::css { … }`
    fn declare(&mut self, rest: &str) -> Option<()> {
        let mut rest = rest;
        let open = rest.ends_with('{');
        if open {
            rest = rest[..rest.len() - 1].trim_end();
        }
        let (rest, inline_close) = match rest.strip_suffix('}') {
            // `class A {}` / `class A { }`
            Some(r) if r.trim_end().ends_with('{') => (r.trim_end().trim_end_matches('{').trim_end(), true),
            _ => (rest, false),
        };
        let rest = match rest.find(":::") {
            Some(k) => rest[..k].trim(),
            None => rest,
        };
        let (name, label) = match rest.find('[') {
            Some(k) => {
                let lab = rest[k + 1..].strip_suffix(']')?.trim();
                (&rest[..k], Some(lab.trim_matches('"').to_string()))
            }
            None => (rest, None),
        };
        let i = self.class(name)?;
        if label.is_some() {
            self.classes[i].label = label;
        }
        if open && !inline_close {
            self.body = Some(i);
        }
        Some(())
    }

    fn note(&mut self, rest: &str) -> Option<()> {
        let (target, text) = match rest.strip_prefix("for ") {
            Some(r) => {
                let q = r.find('"')?;
                (Some(self.class(r[..q].trim())?), &r[q..])
            }
            None => (None, rest),
        };
        let text = text.trim().strip_prefix('"')?.strip_suffix('"')?;
        self.notes.push((text.replace("\\n", "<br>"), target));
        Some(())
    }

    /// A relation line, or `None` if `l` has no relation in it.
    fn relation(&mut self, l: &str) -> Option<Option<()>> {
        // The line part (`--` or `..`) outside quotes.
        let bytes = l.as_bytes();
        let mut quoted = false;
        let mut at = None;
        for i in 0..bytes.len().saturating_sub(1) {
            match bytes[i] {
                b'"' => quoted = !quoted,
                b'-' | b'.' if !quoted && bytes[i + 1] == bytes[i] => {
                    at = Some(i);
                    break;
                }
                _ => {}
            }
        }
        let at = at?;
        let dotted = bytes[at] == b'.';
        let before = &l[..at];
        let after = &l[at + 2..];
        let ws_or_start = |s: &str| s.is_empty() || s.ends_with(|c: char| c.is_whitespace() || c == '"');
        let ws_or_end = |s: &str| s.is_empty() || s.starts_with(|c: char| c.is_whitespace() || c == '"');
        let (tail, lhs) = if let Some(s) = before.strip_suffix("<|") {
            (Head::Triangle, s)
        } else if let Some(s) = before.strip_suffix("()") {
            (Head::CircleOpen, s)
        } else if let Some(s) = before.strip_suffix('<') {
            (Head::Arrow, s)
        } else if let Some(s) = before.strip_suffix('*').filter(|s| ws_or_start(s)) {
            (Head::Diamond, s)
        } else if let Some(s) = before.strip_suffix('o').filter(|s| ws_or_start(s)) {
            (Head::DiamondOpen, s)
        } else {
            (Head::None, before)
        };
        let (head, rhs) = if let Some(s) = after.strip_prefix("|>") {
            (Head::Triangle, s)
        } else if let Some(s) = after.strip_prefix("()") {
            (Head::CircleOpen, s)
        } else if let Some(s) = after.strip_prefix('>') {
            (Head::Arrow, s)
        } else if let Some(s) = after.strip_prefix('*').filter(|s| ws_or_end(s)) {
            (Head::Diamond, s)
        } else if let Some(s) = after.strip_prefix('o').filter(|s| ws_or_end(s)) {
            (Head::DiamondOpen, s)
        } else {
            (Head::None, after)
        };
        let (rhs, label) = match rhs.find(':') {
            Some(c) => (&rhs[..c], rhs[c + 1..].trim()),
            None => (rhs, ""),
        };
        // Multiplicities: `A "1" --> "*" B`.
        let quoted_end = |s: &str| -> Option<(String, String)> {
            let s = s.trim();
            let inner = s.strip_suffix('"')?;
            let q = inner.rfind('"')?;
            Some((inner[q + 1..].to_string(), inner[..q].trim().to_string()))
        };
        let quoted_start = |s: &str| -> Option<(String, String)> {
            let s = s.trim();
            let inner = s.strip_prefix('"')?;
            let q = inner.find('"')?;
            Some((inner[..q].to_string(), inner[q + 1..].trim().to_string()))
        };
        let (card_a, a) = quoted_end(lhs).unwrap_or((String::new(), lhs.trim().to_string()));
        let (card_b, b) = quoted_start(rhs).unwrap_or((String::new(), rhs.trim().to_string()));
        if a.is_empty() || b.is_empty() || !a.chars().all(is_ident) || !b.chars().all(is_ident) {
            return Some(None);
        }
        let (Some(a), Some(b)) = (self.class(&a), self.class(&b)) else { return Some(None) };
        let label = label.trim_matches('"').to_string();
        self.rels.push(Rel { a, b, tail, head, dotted, cards: [card_a, card_b], label });
        Some(Some(()))
    }
}

pub fn parse(lines: &[String]) -> Option<(Graph, Vec<String>)> {
    let header = lines.first()?.trim();
    if header != "classDiagram" && header != "classDiagram-v2" {
        return None;
    }
    let mut p = P::default();
    for l in &lines[1..] {
        let l = l.trim();
        if !l.is_empty() {
            p.line(l)?;
        }
    }
    if p.body.is_some() || !p.ns_stack.is_empty() {
        return None;
    }
    let mut nodes: Vec<Node> = p
        .classes
        .iter()
        .map(|c| {
            let name = match (&c.label, &c.generic) {
                (Some(l), _) => l.clone(),
                (None, Some(g)) => g.clone(),
                (None, None) => c.id.clone(),
            };
            Node {
                id: c.id.clone(),
                label: label_lines(&name, FONT),
                shape: Shape::Class,
                cluster: c.namespace,
                detail: Detail::Class { annotation: c.annotation.clone(), attrs: c.attrs.clone(), methods: c.methods.clone() },
            }
        })
        .collect();
    let mut edges: Vec<Edge> = p
        .rels
        .iter()
        .map(|r| Edge {
            from: End::Node(r.a),
            to: End::Node(r.b),
            label: if r.label.is_empty() { Vec::new() } else { label_lines(&r.label, FONT) },
            line: if r.dotted { Line::Dotted } else { Line::Solid },
            head: r.head,
            tail: r.tail,
            minlen: 1,
            end_labels: r.cards.clone(),
        })
        .collect();
    for (k, (text, target)) in p.notes.iter().enumerate() {
        nodes.push(Node {
            id: format!("note{k}"),
            label: label_lines(text, FONT),
            shape: Shape::Note,
            cluster: target.and_then(|t| p.classes[t].namespace),
            detail: Detail::None,
        });
        if let Some(t) = target {
            edges.push(Edge {
                from: End::Node(nodes.len() - 1),
                to: End::Node(*t),
                label: Vec::new(),
                line: Line::Dotted,
                head: Head::None,
                tail: Head::None,
                minlen: 1,
                end_labels: Default::default(),
            });
        }
    }
    let clusters = p.namespaces.iter().map(|n| Cluster { id: n.clone(), title: label_lines(n, FONT), parent: None }).collect();
    Some((Graph { dir: p.dir.unwrap_or(Dir::TB), nodes, edges, clusters }, Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(src: &str) -> Graph {
        parse(&src.lines().map(String::from).collect::<Vec<_>>()).unwrap_or_else(|| panic!("failed:\n{src}")).0
    }

    fn class<'a>(g: &'a Graph, id: &str) -> (&'a Node, Option<String>, Vec<String>, Vec<String>) {
        let n = g.nodes.iter().find(|n| n.id == id).unwrap();
        match &n.detail {
            Detail::Class { annotation, attrs, methods } => {
                (n, annotation.clone(), attrs.iter().map(|m| m.text.clone()).collect(), methods.iter().map(|m| m.text.clone()).collect())
            }
            _ => panic!("not a class"),
        }
    }

    #[test]
    fn members_and_generics() {
        assert_eq!(generics("Foo~List~int~~"), "Foo<List<int>>");
        assert_eq!(generics("~pkg()"), "~pkg()");
        let m = member("+charge(amount)* bool");
        assert_eq!((m.text.as_str(), m.italic), ("+charge(amount) bool", true));
        let m = member("+count() int$");
        assert_eq!((m.text.as_str(), m.underline), ("+count() int", true));
        let gr = g("classDiagram\n  class Animal {\n    <<abstract>>\n    +int age\n    +String gender\n    +isMammal() bool\n    +mate()*\n  }\n  Animal : +List~int~ ids\n  Animal: +someStatic()$\n  class Square~Shape~\n  class Named[\"A label\"]\n  <<interface>> Shape");
        let (_, ann, attrs, methods) = class(&gr, "Animal");
        assert_eq!(ann.as_deref(), Some("abstract"));
        assert_eq!(attrs, ["+int age", "+String gender", "+List<int> ids"]);
        assert_eq!(methods, ["+isMammal() bool", "+mate()", "+someStatic()"]);
        if let Detail::Class { methods, .. } = &class(&gr, "Animal").0.detail {
            assert!(methods[1].italic && methods[2].underline);
        }
        assert_eq!(class(&gr, "Square").0.label, vec!["Square<Shape>"]);
        assert_eq!(class(&gr, "Named").0.label, vec!["A label"]);
        assert_eq!(class(&gr, "Shape").1.as_deref(), Some("interface"));
    }

    #[test]
    fn relations() {
        let gr = g("classDiagram\n  A <|-- B\n  C *-- D\n  E o-- F\n  G <-- H\n  I -- J\n  K ..> L\n  M ..|> N\n  O .. P\n  Q --o R\n  S --* T\n  Customer \"1\" --> \"*\" Ticket : buys\n  U ()-- V\n  Foo--Bar");
        let e = |a: &str| {
            let i = gr.nodes.iter().position(|n| n.id == a).unwrap();
            gr.edges.iter().find(|e| e.from == End::Node(i)).unwrap().clone()
        };
        assert_eq!((e("A").tail, e("A").head, e("A").line), (Head::Triangle, Head::None, Line::Solid));
        assert_eq!(e("C").tail, Head::Diamond);
        assert_eq!(e("E").tail, Head::DiamondOpen);
        assert_eq!(e("G").tail, Head::Arrow);
        assert_eq!((e("I").tail, e("I").head), (Head::None, Head::None));
        assert_eq!((e("K").head, e("K").line), (Head::Arrow, Line::Dotted));
        assert_eq!((e("M").head, e("M").line), (Head::Triangle, Line::Dotted));
        assert_eq!(e("O").line, Line::Dotted);
        assert_eq!(e("Q").head, Head::DiamondOpen);
        assert_eq!(e("S").head, Head::Diamond);
        let c = e("Customer");
        assert_eq!((c.end_labels.clone(), c.label.clone()), (["1".to_string(), "*".to_string()], vec!["buys".to_string()]));
        assert_eq!(e("U").tail, Head::CircleOpen);
        // `o` at the end of a name isn't an aggregation.
        assert_eq!(e("Foo").tail, Head::None);
    }

    #[test]
    fn namespaces_notes_and_declines() {
        let gr = g("classDiagram\n  direction LR\n  namespace Shapes {\n    class Triangle\n    class Square\n  }\n  note for Square \"four sides\"\n  note \"floating\"\n  Triangle --> Square");
        assert_eq!(gr.dir, Dir::LR);
        assert_eq!(gr.clusters.len(), 1);
        assert_eq!(class(&gr, "Square").0.cluster, Some(0));
        assert_eq!(gr.nodes.iter().filter(|n| n.shape == Shape::Note).count(), 2);
        for bad in ["classDiagram\n  class A {\n  +x", "classDiagram\n  namespace X {\n  class A", "classDiagram\n  A <|-- ", "classDiagram\n  }"] {
            assert!(parse(&bad.lines().map(String::from).collect::<Vec<_>>()).is_none(), "{bad}");
        }
    }
}

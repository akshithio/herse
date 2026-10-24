//! `graph` / `flowchart` syntax → [`Graph`].
//!
//! Statements are node chains (`A[x] & B -->|label| C --> D`), `subgraph`
//! blocks and styling statements, which are accepted and ignored. Any
//! statement this parser can't consume completely makes the whole diagram
//! `None`.

use super::graph::{Cluster, Detail, Edge, End, Graph, Head, Line, Node, Shape};
use super::label_lines;
use super::layout::Dir;

const FONT: f64 = 14.;

pub fn parse(lines: &[String]) -> Option<Graph> {
    let text = lines.join("\n");
    let stmts = statements(&text);
    let mut it = stmts.into_iter();
    let header = it.next()?;
    let mut words = header.split_whitespace();
    let kw = words.next()?;
    if !matches!(kw, "graph" | "flowchart" | "flowchart-elk") {
        return None;
    }
    let dir = match words.next() {
        None => Dir::TB,
        Some(d) => parse_dir(d)?,
    };
    if words.next().is_some() {
        return None;
    }
    let mut p = Parser { dir, nodes: Vec::new(), edges: Vec::new(), clusters: Vec::new(), open: Vec::new(), claimed: Vec::new() };
    for s in it {
        p.statement(s.trim())?;
    }
    if !p.open.is_empty() {
        return None;
    }
    p.finish()
}

pub(crate) fn parse_dir(d: &str) -> Option<Dir> {
    Some(match d {
        "TB" | "TD" | "v" => Dir::TB,
        "BT" | "^" => Dir::BT,
        "LR" | ">" => Dir::LR,
        "RL" | "<" => Dir::RL,
        _ => return None,
    })
}

/// Splits at newlines and at `;` outside quotes and brackets.
fn statements(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quoted = false;
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '(' | '{' if !quoted => depth += 1,
            ']' | ')' | '}' if !quoted => depth -= 1,
            '\n' if !quoted => {
                out.push(&text[start..i]);
                start = i + 1;
                depth = 0;
            }
            ';' if !quoted && depth <= 0 => {
                out.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&text[start..]);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

struct OpenCluster {
    index: usize,
    /// Node ids mentioned inside, in order.
    mentions: Vec<usize>,
}

struct Parser {
    dir: Dir,
    nodes: Vec<NodeDef>,
    edges: Vec<EdgeDef>,
    clusters: Vec<Cluster>,
    open: Vec<OpenCluster>,
    /// Owning cluster per node (claimed when a subgraph closes).
    claimed: Vec<Option<usize>>,
}

struct NodeDef {
    id: String,
    label: Option<String>,
    shape: Shape,
}

struct EdgeDef {
    from: usize,
    to: usize,
    link: Link,
}

#[derive(Clone, Debug, PartialEq)]
struct Link {
    line: Line,
    head: Head,
    tail: Head,
    len: usize,
    label: Option<String>,
}

impl Parser {
    fn statement(&mut self, s: &str) -> Option<()> {
        if s.is_empty() {
            return Some(());
        }
        let first = s.split_whitespace().next().unwrap_or("");
        match first {
            "end" if s == "end" => return self.close(),
            "subgraph" => return self.open_subgraph(s["subgraph".len()..].trim()),
            "direction" => return parse_dir(s["direction".len()..].trim()).map(|_| ()),
            "classDef" | "class" | "style" | "linkStyle" | "click" | "callback" => return Some(()),
            _ => {}
        }
        if s.starts_with("accTitle") || s.starts_with("accDescr") {
            // Multi-line `accDescr { … }` isn't supported.
            return (!s.contains('{')).then_some(());
        }
        self.chain(s)
    }

    fn node(&mut self, id: &str) -> usize {
        let i = match self.nodes.iter().position(|n| n.id == id) {
            Some(i) => i,
            None => {
                self.nodes.push(NodeDef { id: id.to_string(), label: None, shape: Shape::Rect });
                self.claimed.push(None);
                self.nodes.len() - 1
            }
        };
        if let Some(o) = self.open.last_mut()
            && !o.mentions.contains(&i)
        {
            o.mentions.push(i);
        }
        i
    }

    fn open_subgraph(&mut self, rest: &str) -> Option<()> {
        let (id, title) = if rest.is_empty() {
            (format!("subGraph{}", self.clusters.len()), String::new())
        } else if rest.starts_with('"') {
            let t = rest.trim_matches('"').to_string();
            (t.clone(), t)
        } else if let Some(b) = rest.find('[') {
            let id = rest[..b].trim();
            let inner = rest[b + 1..].trim_end().strip_suffix(']')?;
            if id.is_empty() || id.contains(char::is_whitespace) {
                return None;
            }
            (id.to_string(), inner.trim().to_string())
        } else {
            (rest.to_string(), rest.to_string())
        };
        let parent = self.open.last().map(|o| o.index);
        self.clusters.push(Cluster { id, title: label_lines(&title, FONT), parent });
        self.open.push(OpenCluster { index: self.clusters.len() - 1, mentions: Vec::new() });
        Some(())
    }

    fn close(&mut self) -> Option<()> {
        let o = self.open.pop()?;
        for &n in &o.mentions {
            if self.claimed[n].is_none() {
                self.claimed[n] = Some(o.index);
            }
        }
        Some(())
    }

    fn chain(&mut self, s: &str) -> Option<()> {
        let mut c = Cursor { s, i: 0 };
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut links: Vec<Link> = Vec::new();
        loop {
            let mut group = Vec::new();
            loop {
                c.ws();
                let (id, shape, label) = c.node()?;
                let i = self.node(&id);
                if let Some(shape) = shape {
                    self.nodes[i].shape = shape;
                    self.nodes[i].label = label;
                }
                group.push(i);
                c.ws();
                if c.eat("&") {
                    continue;
                }
                break;
            }
            groups.push(group);
            c.ws();
            if c.done() {
                break;
            }
            links.push(c.link()?);
        }
        for (k, link) in links.iter().enumerate() {
            for &a in &groups[k] {
                for &b in &groups[k + 1] {
                    self.edges.push(EdgeDef { from: a, to: b, link: link.clone() });
                }
            }
        }
        Some(())
    }

    fn finish(self) -> Option<Graph> {
        // Ids that name a subgraph refer to its box, not to a node.
        let cluster_of_id = |id: &str| self.clusters.iter().position(|c| c.id == id);
        let mut map: Vec<Option<usize>> = Vec::with_capacity(self.nodes.len());
        let mut nodes = Vec::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if n.label.is_none() && cluster_of_id(&n.id).is_some() {
                map.push(None);
                continue;
            }
            map.push(Some(nodes.len()));
            let label = n.label.as_deref().unwrap_or(&n.id);
            nodes.push(Node { id: n.id.clone(), label: label_lines(label, FONT), shape: n.shape, cluster: self.claimed[i], detail: Detail::None });
        }
        let end = |i: usize| -> Option<End> {
            match map[i] {
                Some(v) => Some(End::Node(v)),
                None => cluster_of_id(&self.nodes[i].id).map(End::Cluster),
            }
        };
        let mut edges = Vec::new();
        for e in &self.edges {
            edges.push(Edge {
                from: end(e.from)?,
                to: end(e.to)?,
                label: e.link.label.as_deref().map(|l| label_lines(l, FONT)).unwrap_or_default(),
                line: e.link.line,
                head: e.link.head,
                tail: e.link.tail,
                minlen: e.link.len,
                end_labels: Default::default(),
            });
        }
        Some(Graph { dir: self.dir, nodes, edges, clusters: self.clusters })
    }
}

struct Cursor<'a> {
    s: &'a str,
    i: usize,
}

fn is_id_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '$' || c == '\''
}

impl Cursor<'_> {
    fn rest(&self) -> &str {
        &self.s[self.i..]
    }

    fn done(&self) -> bool {
        self.i >= self.s.len()
    }

    fn ws(&mut self) {
        let r = self.rest();
        self.i += r.len() - r.trim_start().len();
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.rest().starts_with(p) {
            self.i += p.len();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// A node id, optionally with a shape and label, and a `:::class`.
    fn node(&mut self) -> Option<(String, Option<Shape>, Option<String>)> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if !is_id_char(c) {
                break;
            }
            let r = self.rest();
            // A link starts here: `A-->B`, `A-.->B`, `A==>B`.
            if (c == '-' && (r.starts_with("--") || r.starts_with("-.") || r.starts_with("->"))) || (c == '.' && r.starts_with(".-")) {
                break;
            }
            self.i += c.len_utf8();
        }
        let id = self.s[start..self.i].to_string();
        if id.is_empty() {
            return None;
        }
        if self.rest().starts_with("@{") {
            // mermaid 11 `A@{ shape: … }`: not supported yet.
            return None;
        }
        const SHAPES: &[(&str, &[&str], Shape)] = &[
            ("(((", &[")))"], Shape::DoubleCircle),
            ("((", &["))"], Shape::Circle),
            ("([", &["])"], Shape::Stadium),
            ("[[", &["]]"], Shape::Subroutine),
            ("[(", &[")]"], Shape::Cylinder),
            ("{{", &["}}"], Shape::Hexagon),
            ("[/", &["/]", "\\]"], Shape::LeanRight),
            ("[\\", &["\\]", "/]"], Shape::LeanLeft),
            ("[", &["]"], Shape::Rect),
            ("(", &[")"], Shape::Round),
            ("{", &["}"], Shape::Rhombus),
            (">", &["]"], Shape::Flag),
        ];
        let mut shape = None;
        let mut label = None;
        for &(open, closes, sh) in SHAPES {
            if !self.rest().starts_with(open) {
                continue;
            }
            self.i += open.len();
            let body = self.rest();
            let trimmed = body.trim_start();
            let (text, after) = if let Some(q) = trimmed.strip_prefix('"') {
                let end = q.find('"')?;
                let lead = body.len() - trimmed.len();
                let after_quote = &q[end + 1..];
                let ws = after_quote.len() - after_quote.trim_start().len();
                (format!("\"{}\"", &q[..end]), lead + 1 + end + 1 + ws)
            } else {
                // The first closing delimiter ends the label.
                let end = closes.iter().filter_map(|c| body.find(c)).min()?;
                (body[..end].to_string(), end)
            };
            self.i += after;
            let rest = self.rest();
            let (close, mapped) = closes
                .iter()
                .enumerate()
                .find(|(_, c)| rest.starts_with(**c))
                .map(|(k, c)| {
                    let mapped = match (sh, k) {
                        (Shape::LeanRight, 1) => Shape::Trapezoid,
                        (Shape::LeanLeft, 1) => Shape::TrapezoidAlt,
                        _ => sh,
                    };
                    (*c, mapped)
                })?;
            self.i += close.len();
            shape = Some(mapped);
            label = Some(text);
            break;
        }
        if self.eat(":::") {
            while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-') {
                self.i += 1;
            }
        }
        Some((id, shape, label))
    }

    /// A link with its label: `-->`, `-- text -->`, `-->|text|`, …
    fn link(&mut self) -> Option<Link> {
        let mut tail = Head::None;
        let r = self.rest();
        if r.starts_with('<') {
            tail = Head::Arrow;
            self.i += 1;
        } else if (r.starts_with('o') || r.starts_with('x')) && r[1..].starts_with(['-', '=']) {
            tail = if r.starts_with('o') { Head::Circle } else { Head::Cross };
            self.i += 1;
        }
        let body_start = self.i;
        while self.peek().is_some_and(|c| matches!(c, '-' | '=' | '.' | '~')) {
            self.i += 1;
        }
        let body = &self.s[body_start..self.i];
        let mut link = match self.peek() {
            Some('>') => {
                self.i += 1;
                classify(body, Head::Arrow)?
            }
            Some(c @ ('o' | 'x')) if classify(body, Head::Circle).is_some() => {
                self.i += 1;
                classify(body, if c == 'o' { Head::Circle } else { Head::Cross })?
            }
            _ => match body {
                // `-- text -->`: the label runs to the closing link.
                "--" | "==" | "-." => self.text_link(body)?,
                _ => classify(body, Head::None)?,
            },
        };
        if tail != Head::None && link.head == Head::None {
            return None;
        }
        link.tail = tail;
        self.ws();
        if self.eat("|") {
            let rest = self.rest();
            let end = rest.find('|')?;
            link.label = Some(rest[..end].trim().to_string());
            self.i += end + 1;
        }
        Some(link)
    }

    fn text_link(&mut self, open: &str) -> Option<Link> {
        let rest = self.rest();
        let (marker, kind) = match open {
            "--" => ("--", Line::Solid),
            "==" => ("==", Line::Thick),
            _ => (".-", Line::Dotted),
        };
        let at = rest.find(marker)?;
        let label = rest[..at].trim().to_string();
        if label.is_empty() {
            return None;
        }
        self.i += at;
        // The closing part: `-->`, `---`, `==>`, `.->`, `.-`, with any head.
        let close_start = self.i;
        while self.peek().is_some_and(|c| matches!(c, '-' | '=' | '.')) {
            self.i += 1;
        }
        let close = &self.s[close_start..self.i];
        let head = match self.peek() {
            Some('>') => Head::Arrow,
            Some('o') if !self.rest()[1..].starts_with(|c: char| c.is_alphanumeric()) => Head::Circle,
            Some('x') if !self.rest()[1..].starts_with(|c: char| c.is_alphanumeric()) => Head::Cross,
            _ => Head::None,
        };
        if head != Head::None {
            self.i += 1;
        }
        let len = match kind {
            Line::Dotted => {
                let dots = close.chars().filter(|&c| c == '.').count();
                if !close.ends_with('-') || dots == 0 {
                    return None;
                }
                dots
            }
            _ => {
                let ch = if kind == Line::Thick { '=' } else { '-' };
                if !close.chars().all(|c| c == ch) {
                    return None;
                }
                let n = close.len();
                if head != Head::None {
                    n.checked_sub(1).filter(|&l| l >= 1)?
                } else {
                    n.checked_sub(2).filter(|&l| l >= 1)?
                }
            }
        };
        Some(Link { line: kind, head, tail: Head::None, len, label: Some(label) })
    }
}

/// A link body (dashes, equals, dots or tildes) with its head.
fn classify(body: &str, head: Head) -> Option<Link> {
    let n = body.len();
    let link = |line, len| Some(Link { line, head, tail: Head::None, len, label: None });
    if body.chars().all(|c| c == '-') {
        let len = if head != Head::None { n.checked_sub(1)? } else { n.checked_sub(2)? };
        return if len >= 1 { link(Line::Solid, len) } else { None };
    }
    if body.chars().all(|c| c == '=') {
        let len = if head != Head::None { n.checked_sub(1)? } else { n.checked_sub(2)? };
        return if len >= 1 { link(Line::Thick, len) } else { None };
    }
    if body.chars().all(|c| c == '~') && head == Head::None && n >= 3 {
        return link(Line::Invisible, n - 2);
    }
    // -.-  -.->  -..->
    if let Some(mid) = body.strip_prefix('-').and_then(|b| b.strip_suffix('-'))
        && !mid.is_empty()
        && mid.chars().all(|c| c == '.')
    {
        return link(Line::Dotted, mid.len());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(src: &str) -> Graph {
        let lines: Vec<String> = src.lines().map(|l| l.to_string()).filter(|l| !l.trim().is_empty()).collect();
        parse(&lines).unwrap_or_else(|| panic!("failed to parse:\n{src}"))
    }

    fn shape(g: &Graph, id: &str) -> Shape {
        g.nodes.iter().find(|n| n.id == id).unwrap().shape
    }

    #[test]
    fn shapes_and_labels() {
        let gr = g(r#"flowchart LR
            A[rect] --> B(round) --> C([stadium]) --> D[[sub]] --> E[(db)]
            F((circle)) --- G{rhombus} --- H{{hex}} --- I>flag] --- J[/lean/]
            K[\left\] --- L[/trap\] --- M[\inv/] --- N(((double)))
            O["quoted (parens) [and] {braces}"]
            P["`**md** string`"]
            Q[one<br>two]"#);
        assert_eq!(gr.dir, Dir::LR);
        let expect = [
            ("A", Shape::Rect),
            ("B", Shape::Round),
            ("C", Shape::Stadium),
            ("D", Shape::Subroutine),
            ("E", Shape::Cylinder),
            ("F", Shape::Circle),
            ("G", Shape::Rhombus),
            ("H", Shape::Hexagon),
            ("I", Shape::Flag),
            ("J", Shape::LeanRight),
            ("K", Shape::LeanLeft),
            ("L", Shape::Trapezoid),
            ("M", Shape::TrapezoidAlt),
            ("N", Shape::DoubleCircle),
        ];
        for (id, sh) in expect {
            assert_eq!(shape(&gr, id), sh, "{id}");
        }
        let label = |id: &str| gr.nodes.iter().find(|n| n.id == id).unwrap().label.clone();
        assert_eq!(label("O"), vec!["quoted (parens) [and] {braces}"]);
        assert_eq!(label("P"), vec!["md string"]);
        assert_eq!(label("Q"), vec!["one", "two"]);
        assert_eq!(label("E"), vec!["db"]);
        assert_eq!(gr.edges.len(), 11);
    }

    #[test]
    fn links() {
        let gr = g("graph TD\nA-->B\nA---C\nA-.->D\nA-.-E\nA==>F\nA===G\nA--oH\nA--xI\nA<-->J\nA--->K\nA-->|yes|L\nA-- no -->M\nA-. maybe .->N\nA== go ==>O\nA~~~P\nA o--o Q\nA ---->R");
        let e = |to: &str| {
            let i = gr.nodes.iter().position(|n| n.id == to).unwrap();
            gr.edges.iter().find(|e| e.to == End::Node(i)).unwrap().clone()
        };
        assert_eq!((e("B").line, e("B").head, e("B").minlen), (Line::Solid, Head::Arrow, 1));
        assert_eq!((e("C").line, e("C").head), (Line::Solid, Head::None));
        assert_eq!((e("D").line, e("D").head), (Line::Dotted, Head::Arrow));
        assert_eq!((e("E").line, e("E").head), (Line::Dotted, Head::None));
        assert_eq!((e("F").line, e("F").head), (Line::Thick, Head::Arrow));
        assert_eq!((e("G").line, e("G").head), (Line::Thick, Head::None));
        assert_eq!(e("H").head, Head::Circle);
        assert_eq!(e("I").head, Head::Cross);
        assert_eq!((e("J").head, e("J").tail), (Head::Arrow, Head::Arrow));
        assert_eq!(e("K").minlen, 2);
        assert_eq!(e("L").label, vec!["yes"]);
        assert_eq!((e("M").label.clone(), e("M").head), (vec!["no".to_string()], Head::Arrow));
        assert_eq!((e("N").label.clone(), e("N").line), (vec!["maybe".to_string()], Line::Dotted));
        assert_eq!((e("O").label.clone(), e("O").line), (vec!["go".to_string()], Line::Thick));
        assert_eq!(e("P").line, Line::Invisible);
        assert_eq!((e("Q").head, e("Q").tail), (Head::Circle, Head::Circle));
        assert_eq!(e("R").minlen, 3);
    }

    #[test]
    fn chains_groups_and_subgraphs() {
        let gr = g("flowchart TB\n  c1-->a2\n  subgraph one [First one]\n    a1-->a2\n    subgraph inner\n      x1\n    end\n  end\n  subgraph two\n    b1-->b2\n  end\n  A & B --> C --> D; D --> one\n  classDef red fill:#f00\n  class A red\n  style B fill:#0f0\n  click A callback\n  linkStyle 0 stroke:#f00\n  E:::red --> F");
        let cl = |id: &str| gr.nodes.iter().find(|n| n.id == id).unwrap().cluster.map(|c| gr.clusters[c].id.clone());
        assert_eq!(cl("a1").as_deref(), Some("one"));
        assert_eq!(cl("a2").as_deref(), Some("one"));
        assert_eq!(cl("x1").as_deref(), Some("inner"));
        assert_eq!(cl("b1").as_deref(), Some("two"));
        assert_eq!(cl("c1"), None);
        assert_eq!(gr.clusters[0].title, vec!["First one"]);
        assert_eq!(gr.clusters[1].parent, Some(0));
        // A & B --> C --> D, D --> one, plus the others.
        let one = gr.clusters.iter().position(|c| c.id == "one").unwrap();
        assert!(gr.edges.iter().any(|e| e.to == End::Cluster(one)));
        assert_eq!(gr.edges.len(), 1 + 1 + 1 + 2 + 1 + 1 + 1);
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        let bad = |src: &str| {
            let lines: Vec<String> = src.lines().map(|l| l.to_string()).collect();
            assert!(parse(&lines).is_none(), "should fail: {src}");
        };
        bad("graph XY\nA-->B");
        bad("graph TD\nA-->");
        bad("graph TD\nA[unclosed --> B");
        bad("graph TD\nsubgraph x\nA");
        bad("graph TD\nend");
        bad("graph TD\nA@{ shape: rect }");
        bad("graph TD\nA --> B junk junk");
    }
}

//! `erDiagram` → [`Graph`]: entities with attribute tables (`type name
//! PK,FK "comment"`), and relationships in crow's-foot notation (`||--o{`,
//! `}|..|{`, …) with labels.

use super::flowchart::parse_dir;
use super::graph::{Detail, Edge, End, Graph, Head, Line, Node, Shape};
use super::label_lines;
use super::layout::Dir;

const FONT: f64 = 14.;

#[derive(Default)]
struct Entity {
    id: String,
    alias: Option<String>,
    rows: Vec<[String; 4]>,
}

#[derive(Default)]
struct P {
    entities: Vec<Entity>,
    rels: Vec<(usize, usize, Head, Head, bool, String)>,
    body: Option<usize>,
    dir: Option<Dir>,
}

/// Splits at whitespace, keeping `"quoted strings"` whole.
fn tokens(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                cur.push(c);
                if quoted {
                    out.push(std::mem::take(&mut cur));
                }
                quoted = !quoted;
            }
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if quoted {
        return None;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Some(out)
}

fn unquote(s: &str) -> &str {
    s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s)
}

impl P {
    fn entity(&mut self, name: &str) -> Option<usize> {
        let name = unquote(name.trim());
        if name.is_empty() || name.contains(['{', '}', ':', '|']) {
            return None;
        }
        if let Some(i) = self.entities.iter().position(|e| e.id == name) {
            return Some(i);
        }
        self.entities.push(Entity { id: name.to_string(), ..Default::default() });
        Some(self.entities.len() - 1)
    }

    fn line(&mut self, l: &str) -> Option<()> {
        if let Some(i) = self.body {
            if l == "}" {
                self.body = None;
                return Some(());
            }
            let t = tokens(l)?;
            if t.len() < 2 || t[0].starts_with('"') {
                return None;
            }
            let mut row = [t[0].clone(), t[1].clone(), String::new(), String::new()];
            for tok in &t[2..] {
                if tok.starts_with('"') {
                    row[3] = unquote(tok).to_string();
                } else if tok.split(',').all(|k| matches!(k.trim(), "PK" | "FK" | "UK" | "")) {
                    if !row[2].is_empty() && !row[2].ends_with(',') {
                        row[2].push_str(", ");
                    }
                    row[2].push_str(&tok.split(',').filter(|k| !k.trim().is_empty()).collect::<Vec<_>>().join(", "));
                } else {
                    return None;
                }
            }
            self.entities[i].rows.push(row);
            return Some(());
        }
        for skip in ["classDef ", "class ", "style ", "accTitle", "accDescr"] {
            if l.starts_with(skip) {
                return (!l.ends_with('{')).then_some(());
            }
        }
        if let Some(d) = l.strip_prefix("direction ") {
            self.dir = Some(parse_dir(d.trim())?);
            return Some(());
        }
        if let Some(head) = l.strip_suffix('{') {
            // `NAME {` or `NAME["Alias"] {`
            let head = head.trim();
            let (name, alias) = match head.find('[') {
                Some(k) => (&head[..k], Some(unquote(head[k + 1..].strip_suffix(']')?.trim()).to_string())),
                None => (head, None),
            };
            let i = self.entity(name)?;
            if alias.is_some() {
                self.entities[i].alias = alias;
            }
            self.body = Some(i);
            return Some(());
        }
        let (lhs, label) = match l.find(':') {
            Some(c) => (&l[..c], Some(unquote(l[c + 1..].trim()).to_string())),
            None => (l, None),
        };
        let t = tokens(lhs)?;
        match (t.len(), label) {
            (3, Some(label)) => {
                let (tail, head, dotted) = relationship(&t[1])?;
                let a = self.entity(&t[0])?;
                let b = self.entity(&t[2])?;
                self.rels.push((a, b, tail, head, dotted, label));
                Some(())
            }
            (1, None) => self.entity(&t[0]).map(|_| ()),
            _ => None,
        }
    }
}

/// `||--o{` → (marker at the left entity, at the right one, dashed).
fn relationship(t: &str) -> Option<(Head, Head, bool)> {
    if t.len() != 6 || !t.is_ascii() {
        return None;
    }
    let left = match &t[..2] {
        "|o" => Head::ZeroOne,
        "||" => Head::One,
        "}o" => Head::ZeroMany,
        "}|" => Head::Many,
        _ => return None,
    };
    let dotted = match &t[2..4] {
        "--" => false,
        ".." => true,
        _ => return None,
    };
    let right = match &t[4..] {
        "o|" => Head::ZeroOne,
        "||" => Head::One,
        "o{" => Head::ZeroMany,
        "|{" => Head::Many,
        _ => return None,
    };
    Some((left, right, dotted))
}

pub fn parse(lines: &[String]) -> Option<(Graph, Vec<String>)> {
    if lines.first()?.trim() != "erDiagram" {
        return None;
    }
    let mut p = P::default();
    for l in &lines[1..] {
        let l = l.trim();
        if !l.is_empty() {
            p.line(l)?;
        }
    }
    if p.body.is_some() {
        return None;
    }
    let nodes = p
        .entities
        .iter()
        .map(|e| Node {
            id: e.id.clone(),
            label: label_lines(e.alias.as_deref().unwrap_or(&e.id), FONT),
            shape: Shape::Entity,
            cluster: None,
            detail: Detail::Entity { rows: e.rows.clone() },
        })
        .collect();
    let edges = p
        .rels
        .iter()
        .map(|(a, b, tail, head, dotted, label)| Edge {
            from: End::Node(*a),
            to: End::Node(*b),
            label: if label.is_empty() { Vec::new() } else { label_lines(label, FONT) },
            line: if *dotted { Line::Dotted } else { Line::Solid },
            head: *head,
            tail: *tail,
            minlen: 1,
            end_labels: Default::default(),
        })
        .collect();
    Some((Graph { dir: p.dir.unwrap_or(Dir::TB), nodes, edges, clusters: Vec::new() }, Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(src: &str) -> Option<Graph> {
        parse(&src.lines().map(String::from).collect::<Vec<_>>()).map(|r| r.0)
    }

    #[test]
    fn entities_and_relationships() {
        let gr = g("erDiagram\n  CUSTOMER ||--o{ ORDER : places\n  ORDER ||--|{ LINE-ITEM : contains\n  CUSTOMER }|..|{ DELIVERY-ADDRESS : \"uses\"\n  CUSTOMER {\n    string name\n    string custNumber PK \"the key\"\n    int orgId FK,UK\n  }\n  PRODUCT[\"Product item\"] {\n    varchar(255) title\n  }\n  LONELY").unwrap();
        let names: Vec<&str> = gr.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(names, ["CUSTOMER", "ORDER", "LINE-ITEM", "DELIVERY-ADDRESS", "PRODUCT", "LONELY"]);
        assert_eq!((gr.edges[0].tail, gr.edges[0].head, gr.edges[0].line), (Head::One, Head::ZeroMany, Line::Solid));
        assert_eq!((gr.edges[1].head, gr.edges[2].tail, gr.edges[2].line), (Head::Many, Head::Many, Line::Dotted));
        assert_eq!(gr.edges[2].label, vec!["uses"]);
        match &gr.nodes[0].detail {
            Detail::Entity { rows } => {
                assert_eq!(rows[1], ["string".to_string(), "custNumber".into(), "PK".into(), "the key".into()]);
                assert_eq!(rows[2][2], "FK, UK");
            }
            _ => panic!(),
        }
        assert_eq!(gr.nodes[4].label, vec!["Product item"]);
    }

    #[test]
    fn declines() {
        for src in ["erDiagram\n  A ||--o B : x", "erDiagram\n  A ||--o{ B", "erDiagram\n  A {\n  string", "erDiagram\n  A one or more to zero or more B : x", "erDiagram\n  A {\n  string name PRIMARY\n  }"] {
            assert!(g(src).is_none(), "{src}");
        }
    }
}

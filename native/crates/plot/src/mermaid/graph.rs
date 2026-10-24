//! Node-and-edge diagrams (flowcharts, state, class and ER diagrams): the
//! shared model, node shapes, and drawing on top of the layered layout.
//!
//! Edges leave a node on the side facing the next rank and arrive on the
//! side facing the previous one, at ports spread along that side, and run
//! between ranks as cubic curves whose tangents follow the rank axis.

use super::layout::{self, Dir, Rect};
use super::{Doc, LINE, Theme, measure, text_block, text_line};
use crate::color::Rgba;
use crate::svg::{self, Anchor, PathData, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Rect,
    Round,
    Stadium,
    Subroutine,
    Cylinder,
    Circle,
    DoubleCircle,
    Rhombus,
    Hexagon,
    /// `>text]`, the asymmetric flag.
    Flag,
    /// `[/text/]`
    LeanRight,
    /// `[\text\]`
    LeanLeft,
    /// `[/text\]`, wider at the bottom.
    Trapezoid,
    /// `[\text/]`, wider at the top.
    TrapezoidAlt,
    /// A state: rounded box.
    State,
    /// `[*]` as a source.
    Start,
    /// `[*]` as a target.
    End,
    /// `<<fork>>` / `<<join>>` bar.
    Fork,
    /// `<<choice>>` diamond.
    Choice,
    /// An empty subgraph, drawn as a cluster box.
    Box,
    /// A UML class: name, attributes and methods (see [`Detail::Class`]).
    Class,
    /// An ER entity: name over a table of attributes.
    Entity,
    /// A note attached to a class.
    Note,
}

/// Content of nodes that are more than a label.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Detail {
    #[default]
    None,
    Class { annotation: Option<String>, attrs: Vec<Member>, methods: Vec<Member> },
    /// Rows of (type, name, keys, comment).
    Entity { rows: Vec<[String; 4]> },
}

/// A class member as displayed (`+name: Type`, `+run() bool`).
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub text: String,
    /// Abstract (`*`).
    pub italic: bool,
    /// Static (`$`).
    pub underline: bool,
}

#[derive(Clone, Debug)]
pub struct Node {
    /// The source id (tests and debugging; the label is what's drawn).
    #[cfg_attr(not(test), allow(dead_code))]
    pub id: String,
    pub label: Vec<String>,
    pub shape: Shape,
    pub cluster: Option<usize>,
    pub detail: Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    Node(usize),
    Cluster(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    Solid,
    Dotted,
    Thick,
    Invisible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Head {
    None,
    Arrow,
    Circle,
    Cross,
    /// UML inheritance / realization (hollow).
    Triangle,
    /// UML composition (filled) and aggregation (hollow).
    Diamond,
    DiamondOpen,
    /// UML lollipop interface.
    CircleOpen,
    /// ER cardinalities: `||`, `|o`, `}|`, `}o`.
    One,
    ZeroOne,
    Many,
    ZeroMany,
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub from: End,
    pub to: End,
    pub label: Vec<String>,
    pub line: Line,
    pub head: Head,
    pub tail: Head,
    pub minlen: usize,
    /// Text by the `from` and `to` ends (UML multiplicities).
    pub end_labels: [String; 2],
}

#[derive(Clone, Debug)]
pub struct Cluster {
    pub id: String,
    pub title: Vec<String>,
    pub parent: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct Graph {
    pub dir: Dir,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub clusters: Vec<Cluster>,
}

type P = (f64, f64);

/// Diagrams bigger than this are declined rather than laid out.
const MAX_NODES: usize = 2000;
const MAX_EDGES: usize = 4000;

const ARROW_LEN: f64 = 8.;
const ARROW_HALF: f64 = 4.;
const LOOP: f64 = 30.;
/// Class members and entity attributes.
const MEMBER: f64 = 13.;

/// A node as laid out: the model's nodes, then one per empty cluster.
struct Item {
    shape: Shape,
    label: Vec<String>,
    detail: Detail,
    cluster: Option<usize>,
    w: f64,
    h: f64,
}

/// Everything [`render`] draws, positioned.
struct Geom {
    items: Vec<Item>,
    centers: Vec<P>,
    clusters: Vec<Option<Rect>>,
    /// Per edge (`None` if invisible): its path from port to port, before
    /// arrowheads are cut out of it.
    paths: Vec<Option<EdgePath>>,
    labels: Vec<(P, usize)>,
    w: f64,
    h: f64,
}

#[cfg_attr(not(test), allow(dead_code))]
struct EdgePath {
    segs: Vec<Seg>,
    from: usize,
    to: usize,
    /// The ends that stop at a cluster's box instead of a node.
    from_cluster: Option<usize>,
    to_cluster: Option<usize>,
}

pub fn render(g: &Graph, th: &Theme) -> Option<Doc> {
    let geo = geometry(g, th)?;
    let mut out = svg::Svg::fragment();
    let o = &mut out.out;
    // Clusters, outermost first.
    let nc = g.clusters.len();
    let depth = |c: usize| {
        let mut d = 0;
        let mut k = g.clusters[c].parent;
        while let Some(x) = k {
            d += 1;
            k = g.clusters.get(x).and_then(|cl| cl.parent);
            if d > nc {
                break;
            }
        }
        d
    };
    let mut order: Vec<usize> = (0..nc).collect();
    order.sort_by_key(|&c| depth(c));
    for &c in &order {
        if let Some(r) = geo.clusters[c] {
            cluster_box(o, r, &[], th);
        }
    }
    let paper_at = |q: P| if geo.clusters.iter().flatten().any(|r| r.contains(q)) { th.cluster_fill } else { th.bg };
    for (i, p) in geo.paths.iter().enumerate() {
        let Some(p) = p else { continue };
        let (Some(first), Some(last)) = (p.segs.first(), p.segs.last()) else { continue };
        let e = &g.edges[i];
        let width = if e.line == Line::Thick { 2.5 } else { 1. };
        let paper = [paper_at(first.p0), paper_at(last.p1)];
        let mut segs = p.segs.clone();
        draw_edge(o, &mut segs, e, width, th, paper);
    }
    // Titles over the edges that pass under them.
    for &c in &order {
        if let Some(r) = geo.clusters[c] {
            cluster_title(o, r, &g.clusters[c].title, th);
        }
    }
    for (i, it) in geo.items.iter().enumerate() {
        draw_node(o, it, geo.centers[i], th);
    }
    // Edge labels on top, on a pad of whatever is behind them.
    for &(p, ei) in &geo.labels {
        let lines = &g.edges[ei].label;
        let (w, h) = measure(lines, th.small, false);
        rounded_rect(o, p.0 - w / 2. - 4., p.1 - h / 2. - 1., w + 8., h + 2., 4., &Style::fill(paper_at(p)));
        text_block(o, p.0, p.1, lines, th.small, th.muted, 400, Anchor::Middle);
    }
    Some(Doc { body: out.out, w: geo.w, h: geo.h, title: None, notes: Vec::new() })
}

fn geometry(g: &Graph, th: &Theme) -> Option<Geom> {
    if g.nodes.len() > MAX_NODES || g.edges.len() > MAX_EDGES || (g.nodes.is_empty() && g.clusters.is_empty()) {
        return None;
    }
    let nc = g.clusters.len();
    let horiz = g.dir.horizontal();
    let mut items: Vec<Item> = g
        .nodes
        .iter()
        .map(|n| {
            let (w, h) = node_size(n, th, horiz);
            Item { shape: n.shape, label: n.label.clone(), detail: n.detail.clone(), cluster: n.cluster, w, h }
        })
        .collect();
    // Clusters with no nodes at all become boxes of their own.
    let mut has_nodes = vec![false; nc];
    for n in &g.nodes {
        let mut c = n.cluster;
        let mut guard = 0;
        while let Some(k) = c {
            if k >= nc || guard > nc {
                return None;
            }
            has_nodes[k] = true;
            c = g.clusters[k].parent;
            guard += 1;
        }
    }
    let mut box_of: Vec<Option<usize>> = vec![None; nc];
    for c in 0..nc {
        if !has_nodes[c] {
            let title = &g.clusters[c].title;
            let (tw, tht) = measure(title, th.small, true);
            box_of[c] = Some(items.len());
            items.push(Item {
                shape: Shape::Box,
                label: title.clone(),
                detail: Detail::None,
                cluster: g.clusters[c].parent.filter(|&p| has_nodes[p]),
                w: (tw + 40.).max(80.),
                h: tht + 40.,
            });
        }
    }
    // Edges to or from a cluster attach (for layout) to a node inside it and
    // are clipped at its box when drawn.
    let descendants = |c: usize| -> Vec<usize> {
        (0..g.nodes.len())
            .filter(|&i| {
                let mut k = g.nodes[i].cluster;
                let mut guard = 0;
                while let Some(x) = k {
                    if x == c {
                        return true;
                    }
                    k = g.clusters.get(x).and_then(|cl| cl.parent);
                    guard += 1;
                    if guard > nc {
                        return false;
                    }
                }
                false
            })
            .collect()
    };
    let mut ends: Vec<(usize, usize, Option<usize>, Option<usize>)> = Vec::with_capacity(g.edges.len());
    for (i, e) in g.edges.iter().enumerate() {
        let resolve = |end: End, incoming: bool| -> Option<(usize, Option<usize>)> {
            match end {
                End::Node(v) => (v < g.nodes.len()).then_some((v, None)),
                End::Cluster(c) => {
                    if c >= nc {
                        return None;
                    }
                    if let Some(b) = box_of[c] {
                        return Some((b, None));
                    }
                    let members = descendants(c);
                    let inside = |v: usize| members.contains(&v);
                    // Prefer a member with no edges from (into) the cluster's
                    // own nodes: it sits on the box's near side.
                    let free = members.iter().copied().find(|&m| {
                        !g.edges.iter().enumerate().any(|(j, f)| {
                            j != i
                                && match (f.from, f.to) {
                                    (End::Node(a), End::Node(b)) => {
                                        if incoming {
                                            b == m && inside(a)
                                        } else {
                                            a == m && inside(b)
                                        }
                                    }
                                    _ => false,
                                }
                        })
                    });
                    Some((free.or(members.first().copied())?, Some(c)))
                }
            }
        };
        let (a, ca) = resolve(e.from, false)?;
        let (b, cb) = resolve(e.to, true)?;
        ends.push((a, b, ca, cb));
    }
    let has_label = |e: &Edge| e.line != Line::Invisible && e.label.iter().any(|l| !l.is_empty());
    let label_size = |e: &Edge| {
        let (w, h) = measure(&e.label, th.small, false);
        (w + 10., h + 4.)
    };
    // Self-loops need room beside their node.
    let mut loop_room = vec![0f64; items.len()];
    for (i, e) in g.edges.iter().enumerate() {
        let (a, b, ca, cb) = ends[i];
        if a == b && ca.is_none() && cb.is_none() {
            let (lw, lh) = if has_label(e) { label_size(e) } else { (0., 0.) };
            let room = LOOP + 8. + if horiz { lh } else { lw };
            loop_room[a] = loop_room[a].max(room);
        }
    }

    let titles: Vec<(f64, f64)> = g.clusters.iter().map(|c| measure(&c.title, th.small, true)).collect();
    let lnodes: Vec<layout::NodeIn> = items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let (mut w, mut h) = (it.w, it.h);
            if horiz {
                h += 2. * loop_room[i];
            } else {
                w += 2. * loop_room[i];
            }
            layout::NodeIn { w, h, cluster: it.cluster.filter(|&c| has_nodes[c]) }
        })
        .collect();
    let ledges: Vec<layout::EdgeIn> = g
        .edges
        .iter()
        .zip(&ends)
        .map(|(e, &(a, b, _, _))| layout::EdgeIn {
            from: a,
            to: b,
            minlen: e.minlen.max(1),
            weight: 1.,
            label: has_label(e).then(|| label_size(e)),
        })
        .collect();
    let lclusters: Vec<layout::ClusterIn> = g
        .clusters
        .iter()
        .enumerate()
        .map(|(c, cl)| {
            let untitled = cl.title.iter().all(|t| t.is_empty());
            layout::ClusterIn {
                parent: cl.parent.filter(|&p| has_nodes[p]),
                title_w: if untitled { 0. } else { titles[c].0 },
                title_h: if untitled { 0. } else { titles[c].1 },
            }
        })
        .collect();
    let sp = layout::Spacing { node: 36., rank: 48., edge: 14., cluster_pad: 14., cluster_gap: 10. };
    let lay = layout::layout(g.dir, &lnodes, &ledges, &lclusters, &sp)?;

    // Ports: (edge, is_from_end, where the edge goes next) grouped by
    // (node, side).
    let (vx, vy) = g.dir.vector();
    let along = |p: P| if horiz { p.1 } else { p.0 };
    let mut groups: Vec<Vec<(usize, bool, f64)>> = vec![Vec::new(); 2 * items.len()];
    for (i, e) in lay.edges.iter().enumerate() {
        let (a, b, _, _) = ends[i];
        if a == b || g.edges[i].line == Line::Invisible {
            continue;
        }
        let fwd = !e.reversed;
        let next_from = e.points.first().copied().unwrap_or(lay.nodes[b]);
        let next_to = e.points.last().copied().unwrap_or(lay.nodes[a]);
        groups[2 * a + usize::from(!fwd)].push((i, true, along(next_from)));
        groups[2 * b + usize::from(fwd)].push((i, false, along(next_to)));
    }
    let mut ports: Vec<[P; 2]> = vec![[(0., 0.); 2]; lay.edges.len()];
    for (k, grp) in groups.iter_mut().enumerate() {
        if grp.is_empty() {
            continue;
        }
        let v = k / 2;
        let side = if k % 2 == 0 { 1. } else { -1. };
        grp.sort_by(|x, y| x.2.total_cmp(&y.2).then(x.0.cmp(&y.0)));
        let it = &items[v];
        let extent = if horiz { it.h } else { it.w };
        let avail = extent * spread(it.shape);
        let n = grp.len();
        let c = lay.nodes[v];
        // Each port as close to its edge's next bend as the side allows
        // (straight edges between near-aligned nodes), then spread apart.
        let gap = if n > 1 { (avail / (n - 1) as f64).min(20.) } else { 0. };
        let half = avail / 2.;
        let mut offs: Vec<f64> = grp.iter().map(|a| (a.2 - along(c)).clamp(-half, half)).collect();
        for j in 1..n {
            offs[j] = offs[j].max(offs[j - 1] + gap);
        }
        if let Some(last) = offs.last_mut() {
            *last = last.min(half);
        }
        for j in (0..n.saturating_sub(1)).rev() {
            offs[j] = offs[j].min(offs[j + 1] - gap);
        }
        let poly = outline(it.shape, c, it.w, it.h);
        for (j, &(ei, is_from, _)) in grp.iter().enumerate() {
            let off = offs[j];
            let origin = if horiz { (c.0, c.1 + off) } else { (c.0 + off, c.1) };
            let dir = (vx * side, vy * side);
            let p = ray_hit(&poly, origin, dir).or_else(|| ray_hit(&poly, c, dir)).unwrap_or(c);
            ports[ei][usize::from(!is_from)] = p;
        }
    }

    let mut labels: Vec<(P, usize)> = Vec::new();
    let mut paths: Vec<Option<EdgePath>> = Vec::with_capacity(g.edges.len());
    for (i, e) in g.edges.iter().enumerate() {
        if e.line == Line::Invisible {
            paths.push(None);
            continue;
        }
        let le = &lay.edges[i];
        let (a, b, ca, cb) = ends[i];
        if a == b && ca.is_none() && cb.is_none() {
            let segs = self_loop(lay.nodes[a], items[a].w, items[a].h, horiz);
            if has_label(e) {
                let lp = segs[0].at(0.5);
                let (lw, lh) = label_size(e);
                let at = if horiz { (lp.0, lp.1 + lh / 2. + 2.) } else { (lp.0 + lw / 2. + 2., lp.1) };
                labels.push((at, i));
            }
            paths.push(Some(EdgePath { segs, from: a, to: b, from_cluster: None, to_cluster: None }));
            continue;
        }
        let mut pts: Vec<P> = Vec::with_capacity(le.points.len() + 2);
        pts.push(ports[i][0]);
        pts.extend_from_slice(&le.points);
        pts.push(ports[i][1]);
        let mut segs: Vec<Seg> = pts.windows(2).map(|w| rank_curve(w[0], w[1], horiz)).collect();
        // (An edge between a cluster and its own member isn't clipped.)
        let to_cluster = cb.filter(|&c| lay.clusters[c].is_some_and(|r| clip_end(&mut segs, r)));
        let from_cluster = ca.filter(|&c| lay.clusters[c].is_some_and(|r| clip_start(&mut segs, r)));
        if has_label(e)
            && let Some(lp) = le.label
        {
            labels.push((lp, i));
        }
        paths.push(Some(EdgePath { segs, from: a, to: b, from_cluster, to_cluster }));
    }
    Some(Geom { items, centers: lay.nodes, clusters: lay.clusters, paths, labels, w: lay.width, h: lay.height })
}

/// Size of a node.
fn node_size(n: &Node, th: &Theme, horiz: bool) -> (f64, f64) {
    let (shape, label) = (n.shape, &n.label[..]);
    match &n.detail {
        Detail::Class { annotation, attrs, methods } => return class_layout(label, annotation.as_deref(), attrs, methods, th).size,
        Detail::Entity { rows } => return entity_layout(label, rows, th).size,
        Detail::None => {}
    }
    let empty = label.iter().all(|l| l.is_empty());
    let (tw, tht) = if empty { (0., th.font * LINE) } else { measure(label, th.font, false) };
    let (px, py) = (16., 10.);
    let h = tht + 2. * py;
    match shape {
        Shape::Rect | Shape::Round | Shape::State => ((tw + 2. * px).max(if empty { 40. } else { 0. }), h),
        Shape::Subroutine => (tw + 2. * px + 16., h),
        Shape::Stadium => (tw + 2. * px + h * 0.5, h),
        Shape::Circle | Shape::DoubleCircle => {
            // The label's corners must clear the circle.
            let r = ((tw / 2.).powi(2) + (tht / 2.).powi(2)).sqrt() + 8.;
            let d = (2. * r).max(40.) + if shape == Shape::DoubleCircle { 10. } else { 0. };
            (d, d)
        }
        Shape::Rhombus => {
            // tw'/W + th'/H <= 1 keeps the label inside; H = 0.6 W.
            let (a, b) = (tw + 14., tht + 8.);
            let w = (a + b / 0.6).max(64.);
            (w, (w * 0.6).max(40.))
        }
        Shape::Hexagon => (tw + 2. * px + h * 0.5, h),
        Shape::Flag => (tw + 2. * px + h * 0.3, h),
        Shape::LeanRight | Shape::LeanLeft | Shape::Trapezoid | Shape::TrapezoidAlt => (tw + 2. * px + h * 0.6, h),
        Shape::Cylinder => {
            let w = tw + 2. * px;
            let ry = cyl_ry(w);
            (w, h + 2. * ry)
        }
        Shape::Start => (14., 14.),
        Shape::End => (18., 18.),
        Shape::Fork => {
            if horiz {
                (6., 60.)
            } else {
                (60., 6.)
            }
        }
        Shape::Choice => (26., 26.),
        Shape::Box => (tw + 40., tht + 40.),
        Shape::Note => {
            let (tw, tht) = measure(label, th.small, false);
            (tw + 20., tht + 14.)
        }
        Shape::Class | Shape::Entity => (tw + 2. * px, h),
    }
}

/// A class box's size and the y of its two dividers (from its top).
struct ClassLayout {
    size: (f64, f64),
    dividers: [f64; 2],
}

fn class_layout(name: &[String], annotation: Option<&str>, attrs: &[Member], methods: &[Member], th: &Theme) -> ClassLayout {
    let (nw, nh) = measure(name, th.font, true);
    let ann = annotation.map(|a| crate::text::width(&format!("«{a}»"), th.small, false)).unwrap_or(0.);
    let row = MEMBER * LINE + 2.;
    let mw = attrs.iter().chain(methods).map(|m| crate::text::width(&m.text, MEMBER, false)).fold(0., f64::max);
    let header = 8. + if annotation.is_some() { th.small * LINE } else { 0. } + nh + 8.;
    let section = |n: usize| if n == 0 { 10. } else { 6. + n as f64 * row + 6. };
    let a = section(attrs.len());
    let m = section(methods.len());
    let w = (nw.max(ann) + 32.).max(mw + 24.).max(90.);
    ClassLayout { size: (w, header + a + m), dividers: [header, header + a] }
}

struct EntityLayout {
    size: (f64, f64),
    header: f64,
    row: f64,
    /// Column widths (type, name, keys, comment); 0 for unused columns.
    cols: [f64; 4],
}

fn entity_layout(name: &[String], rows: &[[String; 4]], th: &Theme) -> EntityLayout {
    let (nw, nh) = measure(name, th.font, true);
    let mut cols = [0f64; 4];
    for r in rows {
        for (k, c) in r.iter().enumerate() {
            if !c.is_empty() {
                cols[k] = cols[k].max(crate::text::width(c, MEMBER, k == 2) + 16.);
            }
        }
    }
    let header = nh + 16.;
    let row = MEMBER * LINE + 9.;
    let table: f64 = cols.iter().sum();
    let w = (nw + 40.).max(table).max(100.);
    if table > 0. && table < w {
        // Spare width goes to the last column used.
        if let Some(k) = (0..4).rev().find(|&k| cols[k] > 0.) {
            cols[k] += w - table;
        }
    }
    EntityLayout { size: (w, header + rows.len() as f64 * row), header, row, cols }
}

fn cyl_ry(w: f64) -> f64 {
    (w / 2. / (2.5 + w / 50.)).clamp(4., 11.)
}

/// How much of a node's side ports may spread over.
fn spread(shape: Shape) -> f64 {
    match shape {
        Shape::Rhombus | Shape::Choice => 0.3,
        Shape::Circle | Shape::DoubleCircle | Shape::Stadium | Shape::Cylinder => 0.45,
        Shape::Start | Shape::End => 0.,
        Shape::Fork => 0.8,
        _ => 0.6,
    }
}

fn rect_radius(shape: Shape, h: f64) -> f64 {
    match shape {
        Shape::Rect | Shape::Subroutine | Shape::Class | Shape::Entity | Shape::Note => 4.,
        Shape::Round => 12f64.min(h / 2.),
        Shape::State => 10f64.min(h / 2.),
        Shape::Stadium => h / 2.,
        Shape::Fork => 2.,
        Shape::Box => 8.,
        _ => 0.,
    }
}

/// The shape's outline as a polygon (curves flattened), for port placement.
fn outline(shape: Shape, c: P, w: f64, h: f64) -> Vec<P> {
    let (x0, y0, x1, y1) = (c.0 - w / 2., c.1 - h / 2., c.0 + w / 2., c.1 + h / 2.);
    let (cx, cy) = c;
    match shape {
        Shape::Circle | Shape::DoubleCircle | Shape::Start | Shape::End => ellipse_poly(c, w / 2., h / 2., 0., std::f64::consts::TAU, 40),
        Shape::Rhombus | Shape::Choice => vec![(cx, y0), (x1, cy), (cx, y1), (x0, cy)],
        Shape::Hexagon => {
            let k = h / 4.;
            vec![(x0 + k, y0), (x1 - k, y0), (x1, cy), (x1 - k, y1), (x0 + k, y1), (x0, cy)]
        }
        Shape::Flag => vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0 + h * 0.3, cy)],
        Shape::LeanRight => {
            let k = h * 0.3;
            vec![(x0 + k, y0), (x1, y0), (x1 - k, y1), (x0, y1)]
        }
        Shape::LeanLeft => {
            let k = h * 0.3;
            vec![(x0, y0), (x1 - k, y0), (x1, y1), (x0 + k, y1)]
        }
        Shape::Trapezoid => {
            let k = h * 0.3;
            vec![(x0 + k, y0), (x1 - k, y0), (x1, y1), (x0, y1)]
        }
        Shape::TrapezoidAlt => {
            let k = h * 0.3;
            vec![(x0, y0), (x1, y0), (x1 - k, y1), (x0 + k, y1)]
        }
        Shape::Cylinder => {
            let ry = cyl_ry(w);
            let rx = w / 2.;
            let pi = std::f64::consts::PI;
            let mut p = ellipse_poly((cx, y0 + ry), rx, ry, pi, 2. * pi, 16);
            p.extend(ellipse_poly((cx, y1 - ry), rx, ry, 0., pi, 16));
            p
        }
        _ => rounded_poly(x0, y0, w, h, rect_radius(shape, h)),
    }
}

fn ellipse_poly(c: P, rx: f64, ry: f64, a0: f64, a1: f64, n: usize) -> Vec<P> {
    (0..=n)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f64 / n as f64;
            (c.0 + rx * a.cos(), c.1 + ry * a.sin())
        })
        .collect()
}

fn rounded_poly(x: f64, y: f64, w: f64, h: f64, r: f64) -> Vec<P> {
    let r = r.min(w / 2.).min(h / 2.).max(0.);
    if r < 0.5 {
        return vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h)];
    }
    let pi = std::f64::consts::PI;
    let mut p = Vec::with_capacity(28);
    for (cx, cy, a0) in [(x + w - r, y + r, -pi / 2.), (x + w - r, y + h - r, 0.), (x + r, y + h - r, pi / 2.), (x + r, y + r, pi)] {
        p.extend(ellipse_poly((cx, cy), r, r, a0, a0 + pi / 2., 6));
    }
    p
}

/// First point where the ray `o + t·d` (t > 0) meets the polygon.
fn ray_hit(poly: &[P], o: P, d: P) -> Option<P> {
    let mut best: Option<f64> = None;
    let n = poly.len();
    for i in 0..n {
        let a = poly[i];
        let b = poly[(i + 1) % n];
        let e = (b.0 - a.0, b.1 - a.1);
        let den = d.0 * e.1 - d.1 * e.0;
        if den.abs() < 1e-12 {
            continue;
        }
        let w = (a.0 - o.0, a.1 - o.1);
        let t = (w.0 * e.1 - w.1 * e.0) / den;
        let s = (w.0 * d.1 - w.1 * d.0) / den;
        if t > 1e-9 && (-1e-9..=1. + 1e-9).contains(&s) && best.is_none_or(|bt| t < bt) {
            best = Some(t);
        }
    }
    best.map(|t| (o.0 + t * d.0, o.1 + t * d.1))
}

// ---------------------------------------------------------------- drawing

fn rounded_rect(o: &mut String, x: f64, y: f64, w: f64, h: f64, r: f64, style: &Style) {
    o.push_str("<rect x=\"");
    svg::num(o, x);
    o.push_str("\" y=\"");
    svg::num(o, y);
    o.push_str("\" width=\"");
    svg::num(o, w.max(0.));
    o.push_str("\" height=\"");
    svg::num(o, h.max(0.));
    o.push('"');
    if r > 0. {
        o.push_str(" rx=\"");
        svg::num(o, r.min(w / 2.).min(h / 2.).max(0.));
        o.push('"');
    }
    style.write(o);
    o.push_str("/>\n");
}

fn polygon(o: &mut String, pts: &[P], style: &Style) {
    let mut d = PathData::new();
    for (i, p) in pts.iter().enumerate() {
        if i == 0 {
            d.move_to(p.0, p.1);
        } else {
            d.line_to(p.0, p.1);
        }
    }
    d.close();
    path(o, &d.d, style);
}

fn path(o: &mut String, d: &str, style: &Style) {
    o.push_str("<path d=\"");
    o.push_str(d);
    o.push('"');
    style.write(o);
    o.push_str("/>\n");
}

fn circle(o: &mut String, c: P, r: f64, style: &Style) {
    o.push_str("<circle cx=\"");
    svg::num(o, c.0);
    o.push_str("\" cy=\"");
    svg::num(o, c.1);
    o.push_str("\" r=\"");
    svg::num(o, r);
    o.push('"');
    style.write(o);
    o.push_str("/>\n");
}

fn hline(o: &mut String, x0: f64, x1: f64, y: f64, color: Rgba) {
    let mut d = PathData::new();
    d.move_to(x0, y);
    d.line_to(x1, y);
    path(o, &d.d, &Style::stroke(color, 1.));
}

fn node_style(th: &Theme) -> Style {
    Style { fill: Some(th.node_fill), stroke: Some(th.node_stroke), stroke_width: 1., opacity: 1., round_joins: true, ..Default::default() }
}

fn cluster_box(o: &mut String, r: Rect, title: &[String], th: &Theme) {
    let st = Style { fill: Some(th.cluster_fill), stroke: Some(th.cluster_stroke), stroke_width: 1., opacity: 1., ..Default::default() };
    rounded_rect(o, r.x + 0.5, r.y + 0.5, r.w - 1., r.h - 1., 8., &st);
    cluster_title(o, r, title, th);
}

/// The title at the top of a cluster box, on a pad of the box's fill.
fn cluster_title(o: &mut String, r: Rect, title: &[String], th: &Theme) {
    if title.iter().all(|t| t.is_empty()) {
        return;
    }
    let (w, h) = measure(title, th.small, true);
    let cy = r.y + 5. + h / 2.;
    rounded_rect(o, r.x + r.w / 2. - w / 2. - 4., cy - h / 2., w + 8., h, 3., &Style::fill(th.cluster_fill));
    text_block(o, r.x + r.w / 2., cy, title, th.small, th.muted, 600, Anchor::Middle);
}

fn draw_node(o: &mut String, it: &Item, c: P, th: &Theme) {
    let (w, h) = (it.w, it.h);
    let (x0, y0) = (c.0 - w / 2., c.1 - h / 2.);
    let st = node_style(th);
    let mut text_at = c;
    match it.shape {
        Shape::Box => {
            cluster_box(o, Rect { x: x0, y: y0, w, h }, &it.label, th);
            return;
        }
        Shape::Class => {
            if let Detail::Class { annotation, attrs, methods } = &it.detail {
                draw_class(o, it, (x0, y0), annotation.as_deref(), attrs, methods, th);
            }
            return;
        }
        Shape::Entity => {
            if let Detail::Entity { rows } = &it.detail {
                draw_entity(o, it, (x0, y0), rows, th);
            }
            return;
        }
        Shape::Note => {
            let st = Style { fill: Some(th.note_fill), stroke: Some(th.note_stroke), stroke_width: 1., opacity: 1., ..Default::default() };
            rounded_rect(o, x0 + 0.5, y0 + 0.5, w - 1., h - 1., 4., &st);
            text_block(o, c.0, c.1, &it.label, th.small, th.text, 400, Anchor::Middle);
            return;
        }
        Shape::Start => {
            circle(o, c, w / 2., &Style::fill(th.text));
            return;
        }
        Shape::End => {
            let ring = Style { stroke: Some(th.text), stroke_width: 1.25, opacity: 1., ..Default::default() };
            circle(o, c, w / 2. - 0.6, &ring);
            circle(o, c, w / 2. - 4., &Style::fill(th.text));
            return;
        }
        Shape::Fork => {
            rounded_rect(o, x0, y0, w, h, 2., &Style::fill(th.text));
            return;
        }
        Shape::Circle => circle(o, c, w / 2., &st),
        Shape::DoubleCircle => {
            circle(o, c, w / 2., &st);
            circle(o, c, w / 2. - 4., &Style { fill: None, ..st.clone() });
        }
        Shape::Subroutine => {
            rounded_rect(o, x0 + 0.5, y0 + 0.5, w - 1., h - 1., rect_radius(it.shape, h), &st);
            let line = Style::stroke(th.node_stroke, 1.);
            for x in [x0 + 8.5, x0 + w - 8.5] {
                let mut d = PathData::new();
                d.move_to(x, y0 + 0.5);
                d.line_to(x, y0 + h - 0.5);
                path(o, &d.d, &line);
            }
        }
        Shape::Cylinder => {
            let ry = cyl_ry(w);
            let rx = w / 2.;
            let (x1, y1) = (x0 + w, y0 + h);
            let mut d = PathData::new();
            d.move_to(x0, y0 + ry);
            d.line_to(x0, y1 - ry);
            elliptic_arc(&mut d, rx, ry, x1, y1 - ry);
            d.line_to(x1, y0 + ry);
            elliptic_arc(&mut d, rx, ry, x0, y0 + ry);
            d.close();
            path(o, &d.d, &st);
            let mut d = PathData::new();
            d.move_to(x0, y0 + ry);
            elliptic_arc(&mut d, rx, ry, x1, y0 + ry);
            path(o, &d.d, &Style { fill: None, ..st.clone() });
            text_at.1 += ry;
        }
        Shape::Rect | Shape::Round | Shape::State | Shape::Stadium => {
            rounded_rect(o, x0 + 0.5, y0 + 0.5, w - 1., h - 1., rect_radius(it.shape, h), &st);
        }
        _ => {
            polygon(o, &outline(it.shape, c, w - 1., h - 1.), &st);
            if it.shape == Shape::Flag {
                text_at.0 += h * 0.15;
            }
        }
    }
    text_block(o, text_at.0, text_at.1, &it.label, th.font, th.text, 400, Anchor::Middle);
}

/// One line of member text, left-aligned, maybe italic / underlined.
#[allow(clippy::too_many_arguments)]
fn member_line(o: &mut String, x: f64, cy: f64, s: &str, size: f64, color: Rgba, italic: bool, underline: bool) {
    if s.is_empty() {
        return;
    }
    o.push_str("<text x=\"");
    svg::num(o, x);
    o.push_str("\" y=\"");
    svg::num(o, cy + 0.35 * size);
    o.push_str("\" font-size=\"");
    svg::num(o, size);
    o.push('"');
    if italic {
        o.push_str(" font-style=\"italic\"");
    }
    if underline {
        o.push_str(" text-decoration=\"underline\"");
    }
    svg::paint(o, "fill", Some(color));
    o.push('>');
    svg::esc(o, s);
    o.push_str("</text>\n");
}

fn draw_class(o: &mut String, it: &Item, at: P, annotation: Option<&str>, attrs: &[Member], methods: &[Member], th: &Theme) {
    let (x0, y0) = at;
    let lay = class_layout(&it.label, annotation, attrs, methods, th);
    rounded_rect(o, x0 + 0.5, y0 + 0.5, it.w - 1., it.h - 1., 4., &node_style(th));
    let cx = x0 + it.w / 2.;
    let mut y = y0 + 8.;
    if let Some(a) = annotation {
        let h = th.small * LINE;
        text_line(o, cx, y + h / 2., &format!("«{a}»"), th.small, th.muted, 400, Anchor::Middle);
        y += h;
    }
    let (_, nh) = measure(&it.label, th.font, true);
    text_block(o, cx, y + nh / 2., &it.label, th.font, th.text, 600, Anchor::Middle);
    let row = MEMBER * LINE + 2.;
    for (div, members) in lay.dividers.iter().zip([attrs, methods]) {
        hline(o, x0 + 0.5, x0 + it.w - 0.5, y0 + div + 0.5, th.node_stroke);
        let top = y0 + div + 6.;
        for (i, m) in members.iter().enumerate() {
            member_line(o, x0 + 12., top + row * (i as f64 + 0.5), &m.text, MEMBER, th.text, m.italic, m.underline);
        }
    }
}

fn draw_entity(o: &mut String, it: &Item, at: P, rows: &[[String; 4]], th: &Theme) {
    let (x0, y0) = at;
    let lay = entity_layout(&it.label, rows, th);
    let (w, h) = (it.w, it.h);
    // Rows on the page colour, every other one tinted; the name on the
    // usual node fill.
    rounded_rect(o, x0 + 0.5, y0 + 0.5, w - 1., h - 1., 4., &Style::fill(th.bg));
    let top = y0 + lay.header;
    for i in (1..rows.len()).step_by(2) {
        rounded_rect(o, x0 + 0.5, top + i as f64 * lay.row, w - 1., lay.row, 0., &Style::fill(th.cluster_fill));
    }
    let r = 4.;
    let (hx0, hy0, hx1) = (x0 + 0.5, y0 + 0.5, x0 + w - 0.5);
    let mut d = PathData::new();
    d.move_to(hx0, top);
    d.line_to(hx0, hy0 + r);
    d.arc_to(r, false, true, hx0 + r, hy0);
    d.line_to(hx1 - r, hy0);
    d.arc_to(r, false, true, hx1, hy0 + r);
    d.line_to(hx1, top);
    d.close();
    path(o, &d.d, &Style::fill(th.node_fill));
    text_block(o, x0 + w / 2., y0 + lay.header / 2., &it.label, th.font, th.text, 600, Anchor::Middle);
    if !rows.is_empty() {
        hline(o, x0 + 0.5, x0 + w - 0.5, top + 0.5, th.node_stroke);
        let mut x = x0;
        for (k, cw) in lay.cols.iter().enumerate() {
            if *cw == 0. {
                continue;
            }
            for (i, row) in rows.iter().enumerate() {
                let cy = top + lay.row * (i as f64 + 0.5);
                match k {
                    1 => member_line(o, x + 8., cy, &row[k], MEMBER, th.text, false, false),
                    2 => text_line(o, x + 8., cy, &row[k], MEMBER - 1., th.muted, 600, Anchor::Start),
                    3 => member_line(o, x + 8., cy, &row[k], MEMBER, th.muted, true, false),
                    _ => member_line(o, x + 8., cy, &row[k], MEMBER, th.muted, false, false),
                }
            }
            x += cw;
            if x < x0 + w - 1. {
                let mut d = PathData::new();
                d.move_to(x + 0.5, top + 1.);
                d.line_to(x + 0.5, y0 + h - 1.);
                path(o, &d.d, &Style::stroke(th.cluster_stroke, 1.));
            }
        }
    }
    rounded_rect(o, x0 + 0.5, y0 + 0.5, w - 1., h - 1., 4., &Style { fill: None, ..node_style(th) });
}

/// Half an ellipse (always sweeping through the larger y), to (x, y).
fn elliptic_arc(d: &mut PathData, rx: f64, ry: f64, x: f64, y: f64) {
    d.d.push('A');
    svg::num(&mut d.d, rx);
    d.d.push(' ');
    svg::num(&mut d.d, ry);
    d.d.push_str(" 0 0 0 ");
    svg::num(&mut d.d, x);
    d.d.push(' ');
    svg::num(&mut d.d, y);
}

// ---------------------------------------------------------------- edges

/// A cubic Bézier segment.
#[derive(Clone, Copy, Debug)]
struct Seg {
    p0: P,
    c1: P,
    c2: P,
    p1: P,
}

impl Seg {
    fn at(&self, t: f64) -> P {
        let u = 1. - t;
        let (a, b, c, d) = (u * u * u, 3. * u * u * t, 3. * u * t * t, t * t * t);
        (
            a * self.p0.0 + b * self.c1.0 + c * self.c2.0 + d * self.p1.0,
            a * self.p0.1 + b * self.c1.1 + c * self.c2.1 + d * self.p1.1,
        )
    }

    fn split(&self, t: f64) -> (Seg, Seg) {
        let lerp = |a: P, b: P| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        let (ab, bc, cd) = (lerp(self.p0, self.c1), lerp(self.c1, self.c2), lerp(self.c2, self.p1));
        let (abc, bcd) = (lerp(ab, bc), lerp(bc, cd));
        let m = lerp(abc, bcd);
        (Seg { p0: self.p0, c1: ab, c2: abc, p1: m }, Seg { p0: m, c1: bcd, c2: cd, p1: self.p1 })
    }

    fn len(&self) -> f64 {
        let mut l = 0.;
        let mut prev = self.p0;
        for i in 1..=16 {
            let p = self.at(i as f64 / 16.);
            l += dist(prev, p);
            prev = p;
        }
        l
    }

    /// Unit tangent at the end, pointing forward.
    fn end_dir(&self) -> P {
        for from in [self.c2, self.c1, self.p0] {
            if dist(from, self.p1) > 1e-6 {
                return unit((self.p1.0 - from.0, self.p1.1 - from.1));
            }
        }
        (0., 1.)
    }

    fn reversed(&self) -> Seg {
        Seg { p0: self.p1, c1: self.c2, c2: self.c1, p1: self.p0 }
    }
}

fn dist(a: P, b: P) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn unit(v: P) -> P {
    let l = (v.0 * v.0 + v.1 * v.1).sqrt();
    if l > 1e-12 { (v.0 / l, v.1 / l) } else { (0., 1.) }
}

/// A curve from `a` to `b` leaving and arriving along the rank axis.
fn rank_curve(a: P, b: P, horiz: bool) -> Seg {
    if horiz {
        let mx = (a.0 + b.0) / 2.;
        Seg { p0: a, c1: (mx, a.1), c2: (mx, b.1), p1: b }
    } else {
        let my = (a.1 + b.1) / 2.;
        Seg { p0: a, c1: (a.0, my), c2: (b.0, my), p1: b }
    }
}

/// Shortens the path by `len` at its end.
fn trim_end(segs: &mut Vec<Seg>, mut len: f64) {
    while let Some(last) = segs.last().copied() {
        let l = last.len();
        if l <= len && segs.len() > 1 {
            len -= l;
            segs.pop();
            continue;
        }
        if l <= len {
            // Too short to trim: keep a stub.
            return;
        }
        // Find t with length(t..1) = len.
        let (mut lo, mut hi) = (0., 1.);
        for _ in 0..24 {
            let mid = (lo + hi) / 2.;
            if last.split(mid).1.len() > len {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        if let Some(s) = segs.last_mut() {
            *s = last.split(lo).0;
        }
        return;
    }
}

fn trim_start(segs: &mut Vec<Seg>, len: f64) {
    let mut rev: Vec<Seg> = segs.iter().rev().map(Seg::reversed).collect();
    trim_end(&mut rev, len);
    *segs = rev.iter().rev().map(Seg::reversed).collect();
}

/// Cuts the path where it first enters `r` (its end is inside); false if
/// it starts inside.
fn clip_end(segs: &mut Vec<Seg>, r: Rect) -> bool {
    if segs.first().is_none_or(|s| r.contains(s.p0)) {
        return false;
    }
    for i in 0..segs.len() {
        let s = segs[i];
        let n = 24;
        let mut prev_t = 0.;
        for k in 1..=n {
            let t = k as f64 / n as f64;
            if r.contains(s.at(t)) {
                let (mut lo, mut hi) = (prev_t, t);
                for _ in 0..24 {
                    let mid = (lo + hi) / 2.;
                    if r.contains(s.at(mid)) {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
                segs[i] = s.split(lo).0;
                segs.truncate(i + 1);
                return true;
            }
            prev_t = t;
        }
    }
    false
}

/// Cuts the path where it last leaves `r` (its start is inside).
fn clip_start(segs: &mut Vec<Seg>, r: Rect) -> bool {
    let mut rev: Vec<Seg> = segs.iter().rev().map(Seg::reversed).collect();
    let clipped = clip_end(&mut rev, r);
    *segs = rev.iter().rev().map(Seg::reversed).collect();
    clipped
}

fn head_len(h: Head, width: f64) -> f64 {
    let k = if width > 1.5 { 1.35 } else { 1. };
    match h {
        Head::None | Head::One | Head::ZeroOne | Head::Many | Head::ZeroMany => 0.,
        Head::Arrow => ARROW_LEN * k,
        Head::Circle => 8. * k,
        Head::Cross => 4. * k,
        Head::Triangle => 12.,
        Head::Diamond | Head::DiamondOpen => 16.,
        Head::CircleOpen => 10.,
    }
}

/// An edge end at `tip`, the edge arriving along `dir`; `paper` fills
/// hollow heads (the colour behind the edge).
#[allow(clippy::too_many_arguments)]
fn draw_head(o: &mut String, h: Head, tip: P, dir: P, width: f64, color: Rgba, paper: Rgba) {
    let k = if width > 1.5 { 1.35 } else { 1. };
    let perp = (-dir.1, dir.0);
    let at = |back: f64, side: f64| (tip.0 - dir.0 * back + perp.0 * side, tip.1 - dir.1 * back + perp.1 * side);
    let thin = Style { stroke: Some(color), stroke_width: 1., opacity: 1., round_joins: true, ..Default::default() };
    let line = |o: &mut String, a: P, b: P| {
        let mut d = PathData::new();
        d.move_to(a.0, a.1);
        d.line_to(b.0, b.1);
        path(o, &d.d, &thin);
    };
    let hollow = Style { fill: Some(paper), stroke: Some(color), stroke_width: 1., opacity: 1., round_joins: true, ..Default::default() };
    let solid = Style { fill: Some(color), stroke: Some(color), stroke_width: 1., opacity: 1., round_joins: true, ..Default::default() };
    match h {
        Head::None => {}
        Head::Arrow => {
            let (l, hw) = (ARROW_LEN * k, ARROW_HALF * k);
            polygon(o, &[tip, at(l, hw), at(l, -hw)], &solid);
        }
        Head::Circle => circle(o, at(4. * k, 0.), 4. * k, &Style { fill: Some(color), opacity: 1., ..Default::default() }),
        Head::Cross => {
            let s = 4. * k;
            let st = Style { stroke: Some(color), stroke_width: 1.5 * k, opacity: 1., round_joins: true, ..Default::default() };
            for (a, b) in [((2. * s, s), (0., -s)), ((2. * s, -s), (0., s))] {
                let (pa, pb) = (at(a.0, a.1), at(b.0, b.1));
                let mut d = PathData::new();
                d.move_to(pa.0, pa.1);
                d.line_to(pb.0, pb.1);
                path(o, &d.d, &st);
            }
        }
        Head::Triangle => polygon(o, &[tip, at(12., 7.), at(12., -7.)], &hollow),
        Head::Diamond => polygon(o, &[tip, at(8., 5.5), at(16., 0.), at(8., -5.5)], &solid),
        Head::DiamondOpen => polygon(o, &[tip, at(8., 5.5), at(16., 0.), at(8., -5.5)], &hollow),
        Head::CircleOpen => circle(o, at(5., 0.), 5., &hollow),
        Head::One | Head::ZeroOne | Head::Many | Head::ZeroMany => {
            let many = matches!(h, Head::Many | Head::ZeroMany);
            if many {
                // The crow's foot spreads onto the entity.
                line(o, at(12., 0.), at(0., 7.));
                line(o, at(12., 0.), at(0., -7.));
            }
            let bar = if many { 16. } else { 7. };
            line(o, at(bar, 6.), at(bar, -6.));
            match h {
                Head::One => line(o, at(12., 6.), at(12., -6.)),
                Head::ZeroOne | Head::ZeroMany => circle(o, at(bar + 9., 0.), 4.5, &hollow),
                _ => {}
            }
        }
    }
}

fn edge_style(e: &Edge, width: f64, th: &Theme) -> Style {
    Style {
        stroke: Some(th.edge),
        stroke_width: width,
        dash: (e.line == Line::Dotted).then(|| "3,3".to_string()),
        opacity: 1.,
        round_joins: e.line != Line::Dotted,
        ..Default::default()
    }
}

fn draw_edge(o: &mut String, segs: &mut Vec<Seg>, e: &Edge, width: f64, th: &Theme, paper: [Rgba; 2]) {
    let (Some(first), Some(last)) = (segs.first().copied(), segs.last().copied()) else { return };
    let end_tip = last.p1;
    let end_dir = last.end_dir();
    let start_tip = first.p0;
    let start_dir = first.reversed().end_dir();
    if e.head != Head::None {
        trim_end(segs, head_len(e.head, width));
    }
    if e.tail != Head::None {
        trim_start(segs, head_len(e.tail, width));
    }
    let mut d = PathData::new();
    if let Some(s) = segs.first() {
        d.move_to(s.p0.0, s.p0.1);
    }
    for s in segs.iter() {
        let straight = (s.c1.0 - s.p0.0).abs() < 0.01 && (s.c2.0 - s.p1.0).abs() < 0.01 && (s.p0.0 - s.p1.0).abs() < 0.01
            || (s.c1.1 - s.p0.1).abs() < 0.01 && (s.c2.1 - s.p1.1).abs() < 0.01 && (s.p0.1 - s.p1.1).abs() < 0.01;
        if straight {
            d.line_to(s.p1.0, s.p1.1);
        } else {
            d.cubic_to(s.c1, s.c2, s.p1);
        }
    }
    let mut st = edge_style(e, width, th);
    st.fill = None;
    path(o, &d.d, &st);
    draw_head(o, e.head, end_tip, end_dir, width, th.edge, paper[1]);
    draw_head(o, e.tail, start_tip, start_dir, width, th.edge, paper[0]);
    for (text, tip, dir) in [(&e.end_labels[0], start_tip, start_dir), (&e.end_labels[1], end_tip, end_dir)] {
        if text.is_empty() {
            continue;
        }
        // Back from the node along the edge, then beside it.
        let tw = crate::text::width(text, th.small, false);
        let (bx, by) = (tip.0 - dir.0 * 14., tip.1 - dir.1 * 14.);
        let p = if dir.1.abs() >= dir.0.abs() { (bx + tw / 2. + 7., by) } else { (bx - dir.0 * tw / 2., by - 11.) };
        text_line(o, p.0, p.1, text, th.small, th.muted, 400, Anchor::Middle);
    }
}

/// A loop beside the node (right of it, or below in LR/RL layouts).
fn self_loop(c: P, w: f64, h: f64, horiz: bool) -> Vec<Seg> {
    if horiz {
        let y1 = c.1 + h / 2.;
        let dx = (w / 4.).min(12.);
        let (a, b) = ((c.0 - dx, y1), (c.0 + dx, y1));
        vec![Seg { p0: a, c1: (a.0 - 10., y1 + LOOP * 1.3), c2: (b.0 + 10., y1 + LOOP * 1.3), p1: b }]
    } else {
        let x1 = c.0 + w / 2.;
        let dy = (h / 4.).min(10.);
        let (a, b) = ((x1, c.1 - dy), (x1, c.1 + dy));
        vec![Seg { p0: a, c1: (x1 + LOOP * 1.3, a.1 - 10.), c2: (x1 + LOOP * 1.3, b.1 + 10.), p1: b }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_land_on_the_outline() {
        for shape in [Shape::Rect, Shape::Round, Shape::Stadium, Shape::Circle, Shape::Rhombus, Shape::Hexagon, Shape::Cylinder, Shape::LeanRight] {
            let poly = outline(shape, (100., 100.), 80., 40.);
            for off in [-20., 0., 15.] {
                let down = ray_hit(&poly, (100. + off, 100.), (0., 1.)).unwrap();
                assert!(down.1 > 100. && down.1 <= 120.001, "{shape:?} {off}: {down:?}");
                let up = ray_hit(&poly, (100. + off, 100.), (0., -1.)).unwrap();
                assert!(up.1 < 100. && up.1 >= 79.999, "{shape:?} {off}: {up:?}");
            }
        }
    }

    fn dist_to_poly(poly: &[P], p: P) -> f64 {
        let n = poly.len();
        (0..n)
            .map(|i| {
                let (a, b) = (poly[i], poly[(i + 1) % n]);
                let (ex, ey) = (b.0 - a.0, b.1 - a.1);
                let l2 = ex * ex + ey * ey;
                let t = if l2 > 0. { (((p.0 - a.0) * ex + (p.1 - a.1) * ey) / l2).clamp(0., 1.) } else { 0. };
                dist(p, (a.0 + t * ex, a.1 + t * ey))
            })
            .fold(f64::INFINITY, f64::min)
    }

    fn rect_of(geo: &Geom, i: usize) -> Rect {
        let (c, it) = (geo.centers[i], &geo.items[i]);
        Rect { x: c.0 - it.w / 2., y: c.1 - it.h / 2., w: it.w, h: it.h }
    }

    fn overlap(a: Rect, b: Rect, tol: f64) -> bool {
        a.x < b.x + b.w - tol && b.x < a.x + a.w - tol && a.y < b.y + b.h - tol && b.y < a.y + a.h - tol
    }

    fn inside(a: Rect, b: Rect) -> bool {
        a.x >= b.x - 0.01 && a.y >= b.y - 0.01 && a.x + a.w <= b.x + b.w + 0.01 && a.y + a.h <= b.y + b.h + 0.01
    }

    /// Layout invariants: nodes and labels don't overlap, edges start and end
    /// on their node's outline (or cluster box), clusters hold exactly their
    /// members, and the output is deterministic.
    fn check(g: &Graph, name: &str) {
        let th = Theme::new(false);
        let geo = geometry(g, &th).unwrap_or_else(|| panic!("{name}: no layout"));
        let n = geo.items.len();
        let rects: Vec<Rect> = (0..n).map(|i| rect_of(&geo, i)).collect();
        for i in 0..n {
            for j in i + 1..n {
                assert!(!overlap(rects[i], rects[j], 0.5), "{name}: nodes {i} and {j} overlap: {:?} {:?}", rects[i], rects[j]);
            }
            assert!(
                rects[i].x >= -0.01 && rects[i].y >= -0.01 && rects[i].x + rects[i].w <= geo.w + 0.01 && rects[i].y + rects[i].h <= geo.h + 0.01,
                "{name}: node {i} outside the canvas"
            );
        }
        for (k, p) in geo.paths.iter().enumerate() {
            let Some(p) = p else { continue };
            if p.from == p.to && p.from_cluster.is_none() && p.to_cluster.is_none() {
                continue;
            }
            let first = p.segs.first().unwrap().p0;
            let last = p.segs.last().unwrap().p1;
            for (pt, node, cl, end) in [(first, p.from, p.from_cluster, "start"), (last, p.to, p.to_cluster, "end")] {
                let d = match cl {
                    Some(c) => {
                        let r = geo.clusters[c].unwrap();
                        dist_to_poly(&[(r.x, r.y), (r.x + r.w, r.y), (r.x + r.w, r.y + r.h), (r.x, r.y + r.h)], pt)
                    }
                    None => {
                        let it = &geo.items[node];
                        dist_to_poly(&outline(it.shape, geo.centers[node], it.w, it.h), pt)
                    }
                };
                assert!(d < 0.75, "{name}: edge {k} {end} is {d:.2} px off its boundary");
            }
        }
        for &(lp, ei) in &geo.labels {
            let (w, h) = measure(&g.edges[ei].label, th.small, false);
            let lr = Rect { x: lp.0 - w / 2. - 4., y: lp.1 - h / 2. - 1., w: w + 8., h: h + 2. };
            for (i, r) in rects.iter().enumerate() {
                assert!(!overlap(lr, *r, 1.), "{name}: label of edge {ei} overlaps node {i}");
            }
        }
        for (c, r) in geo.clusters.iter().enumerate() {
            let Some(r) = r else { continue };
            for (i, node) in g.nodes.iter().enumerate() {
                let mut k = node.cluster;
                let mut member = false;
                while let Some(x) = k {
                    member |= x == c;
                    k = g.clusters[x].parent;
                }
                if member {
                    assert!(inside(rects[i], *r), "{name}: node {i} outside its cluster {c}");
                } else {
                    assert!(!overlap(rects[i], *r, 0.5), "{name}: node {i} inside cluster {c}");
                }
            }
        }
        let again = render(g, &th).unwrap().body;
        assert_eq!(render(g, &th).unwrap().body, again, "{name}: not deterministic");
    }

    fn samples() -> Vec<(String, Graph)> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/mermaid");
        let mut out = Vec::new();
        let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            let name = f.file_stem().unwrap().to_string_lossy().to_string();
            let src = super::super::Source::parse(&std::fs::read_to_string(&f).unwrap()).unwrap();
            if let Some((g, _)) = super::super::graph_of(&src.lines) {
                out.push((name, g));
            }
        }
        assert!(out.len() >= 8);
        out
    }

    #[test]
    fn sample_layouts_hold_invariants() {
        for (name, g) in samples() {
            for dir in [Dir::TB, Dir::BT, Dir::LR, Dir::RL] {
                check(&Graph { dir, ..g.clone() }, &format!("{name} {dir:?}"));
            }
        }
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }
    }

    fn random_graph(r: &mut Lcg, n: usize, m: usize) -> Graph {
        const SHAPES: [Shape; 10] = [Shape::Rect, Shape::Round, Shape::Rhombus, Shape::Circle, Shape::Stadium, Shape::Cylinder, Shape::Hexagon, Shape::LeanRight, Shape::Subroutine, Shape::State];
        let words = ["Build", "Test the thing", "Deploy", "x", "Review changes", "OK?", "Notify the on-call team", "Retry"];
        let mut clusters = vec![
            Cluster { id: "a".into(), title: vec!["Cluster A".into()], parent: None },
            Cluster { id: "b".into(), title: vec!["B".into()], parent: None },
            Cluster { id: "c".into(), title: vec!["A nested cluster title".into()], parent: Some(0) },
        ];
        if r.next().is_multiple_of(2) {
            clusters.truncate(2);
        }
        let nc = clusters.len();
        let nodes: Vec<Node> = (0..n)
            .map(|i| Node {
                id: format!("n{i}"),
                label: vec![words[r.next() % words.len()].to_string()],
                shape: SHAPES[r.next() % SHAPES.len()],
                cluster: if r.next().is_multiple_of(3) { Some(r.next() % nc) } else { None },
                detail: Detail::None,
            })
            .collect();
        let edges: Vec<Edge> = (0..m)
            .map(|_| {
                let (a, b) = (r.next() % n, r.next() % n);
                let end = |v: usize, r: &mut Lcg| if r.next().is_multiple_of(20) { End::Cluster(r.next() % nc) } else { End::Node(v) };
                Edge {
                    from: end(a, r),
                    to: end(b, r),
                    label: if r.next().is_multiple_of(3) { vec![words[r.next() % words.len()].to_string()] } else { Vec::new() },
                    line: [Line::Solid, Line::Dotted, Line::Thick][r.next() % 3],
                    head: Head::Arrow,
                    tail: Head::None,
                    minlen: 1 + usize::from(r.next().is_multiple_of(8)),
                    end_labels: Default::default(),
                }
            })
            .collect();
        Graph { dir: [Dir::TB, Dir::LR, Dir::BT, Dir::RL][r.next() % 4], nodes, edges, clusters }
    }

    #[test]
    fn random_layouts_hold_invariants() {
        let mut r = Lcg(7);
        for k in 0..120 {
            let n = 2 + r.next() % 30;
            let m = r.next() % (2 * n);
            let g = random_graph(&mut r, n, m);
            check(&g, &format!("random #{k}"));
        }
    }

    #[test]
    fn trimming_and_clipping() {
        let mut segs = vec![rank_curve((0., 0.), (40., 100.), false)];
        let full = segs[0].len();
        trim_end(&mut segs, 8.);
        assert!((full - segs[0].len() - 8.).abs() < 0.1);
        let mut segs = vec![rank_curve((0., 0.), (0., 50.), false), rank_curve((0., 50.), (30., 100.), false)];
        assert!(clip_end(&mut segs, Rect { x: -50., y: 70., w: 200., h: 100. }));
        let end = segs.last().unwrap().p1;
        assert!((end.1 - 70.).abs() < 0.01, "{end:?}");
        let mut segs = vec![rank_curve((0., 0.), (0., 100.), false)];
        assert!(clip_start(&mut segs, Rect { x: -50., y: -50., w: 100., h: 80. }));
        assert!((segs[0].p0.1 - 30.).abs() < 0.01 && segs[0].p1 == (0., 100.));
    }
}

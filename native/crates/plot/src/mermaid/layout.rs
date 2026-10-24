//! Layered ("Sugiyama") layout for flowcharts and state diagrams, in the
//! spirit of dot and dagre (which mermaid uses):
//!
//! 1. break cycles by reversing DFS back edges;
//! 2. rank nodes with network simplex (every edge length doubled, so each
//!    edge gets a middle rank for its label, as dagre does);
//! 3. split long edges into chains of dummy nodes, one per rank;
//! 4. order each rank with barycenter sweeps plus adjacent transpositions,
//!    keeping every cluster contiguous and sibling clusters in one order;
//! 5. give x coordinates with network simplex on dot's auxiliary graph
//!    (straight long edges, parents centred over children, cluster borders
//!    as variables so a cluster box never overlaps an outside node);
//! 6. stack the ranks, leaving room for cluster padding and titles.
//!
//! Everything is computed top-to-bottom; other directions transpose and/or
//! flip at the end.

use super::ns;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    TB,
    BT,
    LR,
    RL,
}

impl Dir {
    pub fn horizontal(self) -> bool {
        matches!(self, Dir::LR | Dir::RL)
    }

    /// Unit vector of increasing rank, in final coordinates.
    pub fn vector(self) -> (f64, f64) {
        match self {
            Dir::TB => (0., 1.),
            Dir::BT => (0., -1.),
            Dir::LR => (1., 0.),
            Dir::RL => (-1., 0.),
        }
    }
}

/// Gaps, in px.
#[derive(Clone, Debug)]
pub struct Spacing {
    /// Between neighbouring nodes in a rank.
    pub node: f64,
    /// Between ranks.
    pub rank: f64,
    /// Between an edge passing a rank and its neighbours.
    pub edge: f64,
    /// Inside a cluster box, around its contents.
    pub cluster_pad: f64,
    /// Outside a cluster box.
    pub cluster_gap: f64,
}

/// A node, sized in final (not layout) coordinates.
#[derive(Clone, Debug)]
pub struct NodeIn {
    pub w: f64,
    pub h: f64,
    pub cluster: Option<usize>,
}

#[derive(Clone, Debug)]
pub struct EdgeIn {
    pub from: usize,
    pub to: usize,
    /// Rank span (mermaid's `-->` is 1, `--->` 2, ...).
    pub minlen: usize,
    pub weight: f64,
    /// Label size (final coordinates); it gets a slot in the middle rank.
    pub label: Option<(f64, f64)>,
}

#[derive(Clone, Debug)]
pub struct ClusterIn {
    pub parent: Option<usize>,
    /// Title size; the title sits at the top of the box.
    pub title_w: f64,
    pub title_h: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn contains(&self, p: (f64, f64)) -> bool {
        p.0 > self.x && p.0 < self.x + self.w && p.1 > self.y && p.1 < self.y + self.h
    }
}

#[derive(Clone, Debug)]
pub struct EdgeOut {
    /// Bend points between the two nodes, from `from` to `to`: one per rank
    /// the edge crosses. Empty for self-loops.
    pub points: Vec<(f64, f64)>,
    /// Centre of the label slot.
    pub label: Option<(f64, f64)>,
    /// Drawn against the rank direction (a back edge of a cycle).
    pub reversed: bool,
}

#[derive(Clone, Debug)]
pub struct Layout {
    /// Node centres.
    pub nodes: Vec<(f64, f64)>,
    pub edges: Vec<EdgeOut>,
    /// Cluster boxes (`None` for a cluster without nodes).
    pub clusters: Vec<Option<Rect>>,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Real,
    Dummy,
    Label,
    Placeholder,
}

/// The layered graph: real nodes first, then dummies, labels and cluster
/// placeholders. Sizes are in layout coordinates (`w` across the rank).
struct Lg {
    rank: Vec<usize>,
    w: Vec<f64>,
    h: Vec<f64>,
    /// Half of the gap this node wants to its neighbours.
    hs: Vec<f64>,
    cluster: Vec<Option<usize>>,
    kind: Vec<Kind>,
    up: Vec<Vec<(usize, f64)>>,
    down: Vec<Vec<(usize, f64)>>,
}

impl Lg {
    fn push(&mut self, rank: usize, w: f64, h: f64, hs: f64, cluster: Option<usize>, kind: Kind) -> usize {
        self.rank.push(rank);
        self.w.push(w);
        self.h.push(h);
        self.hs.push(hs);
        self.cluster.push(cluster);
        self.kind.push(kind);
        self.up.push(Vec::new());
        self.down.push(Vec::new());
        self.rank.len() - 1
    }

    fn len(&self) -> usize {
        self.rank.len()
    }
}

/// Network simplex pivots allowed per solve; plenty for diagrams of a few
/// hundred nodes, and a hard stop for pathological inputs.
const MAX_PIVOTS: usize = 4000;

/// Dummy nodes allowed for long edges (two per rank crossed).
const MAX_DUMMIES: usize = 3000;

pub fn layout(dir: Dir, nodes: &[NodeIn], edges: &[EdgeIn], clusters: &[ClusterIn], sp: &Spacing) -> Option<Layout> {
    let n = nodes.len();
    let nc = clusters.len();
    let horiz = dir.horizontal();
    if edges.iter().any(|e| e.from >= n || e.to >= n) || nodes.iter().any(|v| v.cluster.is_some_and(|c| c >= nc)) {
        return None;
    }
    // Cluster ancestry, outermost first.
    let mut cpath: Vec<Vec<usize>> = Vec::with_capacity(nc);
    for c in 0..nc {
        let mut p = vec![c];
        let mut cur = clusters[c].parent;
        while let Some(q) = cur {
            if q >= nc || p.len() > nc {
                return None;
            }
            p.push(q);
            cur = clusters[q].parent;
        }
        p.reverse();
        cpath.push(p);
    }

    // 1-2. Acyclic orientation and ranks.
    let reversed = break_cycles(n, edges);
    let oriented = |i: usize| -> (usize, usize) {
        let e = &edges[i];
        if reversed[i] { (e.to, e.from) } else { (e.from, e.to) }
    };
    let ns_edges: Vec<ns::Edge> = (0..edges.len())
        .filter(|&i| edges[i].from != edges[i].to)
        .map(|i| {
            let (u, v) = oriented(i);
            ns::Edge { u, v, minlen: 2 * edges[i].minlen.clamp(1, 64) as i64, weight: edges[i].weight }
        })
        .collect();
    let raw = ns::solve(n, &ns_edges, false, MAX_PIVOTS)?;
    // Each connected component starts at rank 0.
    let mut comp: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for e in &ns_edges {
        let (a, b) = (find(&mut comp, e.u), find(&mut comp, e.v));
        if a != b {
            comp[a] = b;
        }
    }
    let mut cmin = vec![i64::MAX; n];
    for (v, &r) in raw.iter().enumerate() {
        let c = find(&mut comp, v);
        cmin[c] = cmin[c].min(r);
    }
    let rank: Vec<usize> = (0..n).map(|v| (raw[v] - cmin[find(&mut comp, v)]) as usize).collect();
    // Long edges become chains of dummy nodes; past this many the drawing
    // would be unreadable (and slow), so decline.
    let dummies: usize = (0..edges.len())
        .filter(|&i| edges[i].from != edges[i].to)
        .map(|i| {
            let (u, v) = oriented(i);
            rank[v].abs_diff(rank[u]).saturating_sub(1)
        })
        .sum();
    if dummies > MAX_DUMMIES {
        return None;
    }

    // 3. The layered graph.
    let mut lg = Lg { rank: vec![], w: vec![], h: vec![], hs: vec![], cluster: vec![], kind: vec![], up: vec![], down: vec![] };
    let lsize = |w: f64, h: f64| if horiz { (h, w) } else { (w, h) };
    for v in nodes {
        let (w, h) = lsize(v.w, v.h);
        lg.push(0, w, h, sp.node / 2., v.cluster, Kind::Real);
    }
    lg.rank.copy_from_slice(&rank);
    let lca = |a: Option<usize>, b: Option<usize>| -> Option<usize> {
        let (a, b) = (a?, b?);
        cpath[a].iter().zip(&cpath[b]).take_while(|(x, y)| x == y).last().map(|(x, _)| *x)
    };
    let mut chains: Vec<Vec<usize>> = Vec::with_capacity(edges.len());
    let mut label_node: Vec<Option<usize>> = vec![None; edges.len()];
    for (i, e) in edges.iter().enumerate() {
        if e.from == e.to {
            chains.push(Vec::new());
            continue;
        }
        let (u, v) = oriented(i);
        let (ru, rv) = (rank[u], rank[v]);
        if rv <= ru {
            return None;
        }
        let owner = lca(nodes[u].cluster, nodes[v].cluster);
        let label_rank = e.label.map(|_| {
            let r = ru + (rv - ru) / 2;
            if r % 2 == 0 && r > ru + 1 { r - 1 } else { r }
        });
        let mut chain = vec![u];
        for r in ru + 1..rv {
            let id = if Some(r) == label_rank {
                let (w, h) = e.label.map(|(w, h)| lsize(w, h)).unwrap_or((0., 0.));
                let id = lg.push(r, w, h, sp.edge / 2. + 4., owner, Kind::Label);
                label_node[i] = Some(id);
                id
            } else {
                lg.push(r, 0., 0., sp.edge / 2., owner, Kind::Dummy)
            };
            chain.push(id);
        }
        chain.push(v);
        for k in 1..chain.len() {
            let (a, b) = (chain[k - 1], chain[k]);
            let dummies = [a, b].iter().filter(|&&x| matches!(lg.kind[x], Kind::Dummy | Kind::Label)).count();
            let omega = [1., 2., 8.][dummies];
            let w = e.weight.max(0.) * omega;
            lg.down[a].push((b, w));
            lg.up[b].push((a, w));
        }
        chains.push(chain);
    }
    let nlayers = lg.rank.iter().copied().max().map_or(0, |m| m + 1);

    // Cluster spans, and a zero-size placeholder on every rank a cluster
    // spans without a node of its own, so its box keeps outside nodes out.
    let mut span: Vec<Option<(usize, usize)>> = vec![None; nc];
    let mut present = vec![vec![false; nlayers]; nc];
    for v in 0..lg.len() {
        if let Some(c) = lg.cluster[v] {
            for &a in &cpath[c] {
                let r = lg.rank[v];
                span[a] = Some(span[a].map_or((r, r), |(lo, hi)| (lo.min(r), hi.max(r))));
                present[a][r] = true;
            }
        }
    }
    let mut by_depth: Vec<usize> = (0..nc).collect();
    by_depth.sort_by_key(|&c| std::cmp::Reverse(cpath[c].len()));
    for &c in &by_depth {
        if let Some((lo, hi)) = span[c] {
            #[allow(clippy::needless_range_loop)]
            for r in lo..=hi {
                if !present[c][r] {
                    lg.push(r, 0., 0., 0., Some(c), Kind::Placeholder);
                    for &a in &cpath[c] {
                        present[a][r] = true;
                    }
                }
            }
        }
    }

    // 4. Ordering.
    let paths: Vec<&[usize]> = (0..lg.len()).map(|v| lg.cluster[v].map_or(&[][..], |c| &cpath[c][..])).collect();
    let layers = order(&lg, nlayers, &paths, nc);

    // 5. x coordinates.
    let band_side = |c: usize| if horiz { clusters[c].title_h + 6. } else { 0. };
    let (x, borders) = x_coords(&lg, &layers, &paths, &span, clusters, sp, horiz, &band_side)?;

    // 6. y coordinates.
    let band_top = |c: usize| if dir == Dir::TB { clusters[c].title_h + 6. } else { 0. };
    let band_bottom = |c: usize| if dir == Dir::BT { clusters[c].title_h + 6. } else { 0. };
    let mut rh = vec![0f64; nlayers];
    for v in 0..lg.len() {
        rh[lg.rank[v]] = rh[lg.rank[v]].max(lg.h[v]);
    }
    // Padding stacked by clusters that start (end) on the same rank.
    let mut stack_top = vec![0f64; nc];
    let mut stack_bottom = vec![0f64; nc];
    for &c in &by_depth {
        let Some((lo, hi)) = span[c] else { continue };
        let kids = (0..nc).filter(|&d| clusters[d].parent == Some(c));
        let (mut t, mut b) = (0f64, 0f64);
        for d in kids {
            if let Some((dlo, dhi)) = span[d] {
                if dlo == lo {
                    t = t.max(stack_top[d]);
                }
                if dhi == hi {
                    b = b.max(stack_bottom[d]);
                }
            }
        }
        stack_top[c] = t + sp.cluster_pad + band_top(c);
        stack_bottom[c] = b + sp.cluster_pad + band_bottom(c);
    }
    let mut top_extra = vec![0f64; nlayers];
    let mut bottom_extra = vec![0f64; nlayers];
    for c in 0..nc {
        if let Some((lo, hi)) = span[c] {
            top_extra[lo] = top_extra[lo].max(stack_top[c] + sp.cluster_gap);
            bottom_extra[hi] = bottom_extra[hi].max(stack_bottom[c] + sp.cluster_gap);
        }
    }
    // Edges that travel far sideways between two ranks get more room, so
    // their curves don't flatten into one another.
    let mut travel = vec![0f64; nlayers];
    for u in 0..lg.len() {
        for &(v, _) in &lg.down[u] {
            let r = lg.rank[u];
            travel[r] = travel[r].max((x[u] - x[v]).abs());
        }
    }
    let stack_ranks = |top_extra: &[f64], bottom_extra: &[f64]| -> Vec<f64> {
        let mut y = vec![0f64; nlayers];
        for r in 0..nlayers {
            y[r] = if r == 0 {
                top_extra[0] + rh[0] / 2.
            } else {
                let extra = ((travel[r - 1] - 2. * sp.rank) * 0.1).clamp(0., sp.rank);
                y[r - 1] + rh[r - 1] / 2. + sp.rank / 2. + extra + bottom_extra[r - 1] + top_extra[r] + rh[r] / 2.
            };
        }
        y
    };
    let mut y = stack_ranks(&top_extra, &bottom_extra);
    let ys = |y: &[f64]| -> Vec<f64> { (0..lg.len()).map(|v| y[lg.rank[v]]).collect() };
    let mut boxes = cluster_boxes(&lg, &ys(&y), &borders, &span, clusters, &by_depth, sp, &band_top, &band_bottom);
    if horiz {
        // The title runs along the rank axis: make room if it's longer than
        // the box.
        let mut grew = false;
        for c in 0..nc {
            if let (Some(b), Some((lo, hi))) = (boxes[c], span[c]) {
                let need = clusters[c].title_w + 2. * sp.cluster_pad;
                if b.h < need {
                    let d = (need - b.h) / 2.;
                    top_extra[lo] = top_extra[lo].max(stack_top[c] + sp.cluster_gap + d);
                    bottom_extra[hi] = bottom_extra[hi].max(stack_bottom[c] + sp.cluster_gap + d);
                    grew = true;
                }
            }
        }
        if grew {
            y = stack_ranks(&top_extra, &bottom_extra);
            boxes = cluster_boxes(&lg, &ys(&y), &borders, &span, clusters, &by_depth, sp, &band_top, &band_bottom);
        }
        for c in 0..nc {
            if let Some(b) = boxes[c].as_mut() {
                let need = clusters[c].title_w + 2. * sp.cluster_pad;
                if b.h < need {
                    b.y -= (need - b.h) / 2.;
                    b.h = need;
                }
            }
        }
    }
    let yv = ys(&y);

    // Bounds, then final coordinates.
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for v in 0..lg.len() {
        x0 = x0.min(x[v] - lg.w[v] / 2.);
        x1 = x1.max(x[v] + lg.w[v] / 2.);
        y0 = y0.min(yv[v] - lg.h[v] / 2.);
        y1 = y1.max(yv[v] + lg.h[v] / 2.);
    }
    for b in boxes.iter().flatten() {
        x0 = x0.min(b.x);
        x1 = x1.max(b.x + b.w);
        y0 = y0.min(b.y);
        y1 = y1.max(b.y + b.h);
    }
    if lg.len() == 0 {
        (x0, y0, x1, y1) = (0., 0., 0., 0.);
    }
    let (lw, lh) = (x1 - x0, y1 - y0);
    let to_final = |px: f64, py: f64| -> (f64, f64) {
        let (px, py) = (px - x0, py - y0);
        match dir {
            Dir::TB => (px, py),
            Dir::BT => (px, lh - py),
            Dir::LR => (py, px),
            Dir::RL => (lh - py, px),
        }
    };
    let pos = |v: usize| to_final(x[v], yv[v]);
    let out_nodes: Vec<(f64, f64)> = (0..n).map(pos).collect();
    let out_edges: Vec<EdgeOut> = (0..edges.len())
        .map(|i| {
            let chain = &chains[i];
            let mut points: Vec<(f64, f64)> = if chain.len() > 2 { chain[1..chain.len() - 1].iter().map(|&v| pos(v)).collect() } else { Vec::new() };
            if reversed[i] {
                points.reverse();
            }
            EdgeOut { points, label: label_node[i].map(pos), reversed: reversed[i] }
        })
        .collect();
    let out_clusters: Vec<Option<Rect>> = boxes
        .iter()
        .map(|b| {
            b.map(|b| {
                let (ax, ay) = to_final(b.x, b.y);
                let (bx, by) = to_final(b.x + b.w, b.y + b.h);
                Rect { x: ax.min(bx), y: ay.min(by), w: (ax - bx).abs(), h: (ay - by).abs() }
            })
        })
        .collect();
    let (width, height) = if horiz { (lh, lw) } else { (lw, lh) };
    Some(Layout { nodes: out_nodes, edges: out_edges, clusters: out_clusters, width, height })
}

/// Marks the edges to reverse. Edges are taken in declaration order and one
/// is reversed when it would close a cycle with those before it, so the
/// edge that loops back (`retry --> start`, usually written last) is the one
/// drawn against the flow. Big graphs use the back edges of a DFS instead.
fn break_cycles(n: usize, edges: &[EdgeIn]) -> Vec<bool> {
    let mut reversed = vec![false; edges.len()];
    if edges.len() > 600 {
        return dfs_back_edges(n, edges);
    }
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut seen = vec![0usize; n];
    let mut stack: Vec<usize> = Vec::new();
    for (i, e) in edges.iter().enumerate() {
        if e.from == e.to {
            continue;
        }
        // Is `from` already reachable from `to`?
        let stamp = i + 1;
        stack.clear();
        stack.push(e.to);
        seen[e.to] = stamp;
        let mut closes = false;
        while let Some(v) = stack.pop() {
            if v == e.from {
                closes = true;
                break;
            }
            for &w in &adj[v] {
                if seen[w] != stamp {
                    seen[w] = stamp;
                    stack.push(w);
                }
            }
        }
        if closes {
            reversed[i] = true;
            adj[e.to].push(e.from);
        } else {
            adj[e.from].push(e.to);
        }
    }
    reversed
}

fn dfs_back_edges(n: usize, edges: &[EdgeIn]) -> Vec<bool> {
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, e) in edges.iter().enumerate() {
        if e.from != e.to {
            out[e.from].push(i);
        }
    }
    let mut reversed = vec![false; edges.len()];
    // 0 unvisited, 1 on the stack, 2 done.
    let mut state = vec![0u8; n];
    for s in 0..n {
        if state[s] != 0 {
            continue;
        }
        state[s] = 1;
        let mut stack: Vec<(usize, usize)> = vec![(s, 0)];
        while let Some(top) = stack.last_mut() {
            let (v, i) = *top;
            if i < out[v].len() {
                top.1 += 1;
                let e = out[v][i];
                let w = edges[e].to;
                match state[w] {
                    0 => {
                        state[w] = 1;
                        stack.push((w, 0));
                    }
                    1 => reversed[e] = true,
                    _ => {}
                }
            } else {
                state[v] = 2;
                stack.pop();
            }
        }
    }
    reversed
}

// ---------------------------------------------------------------- ordering

fn order(lg: &Lg, nlayers: usize, paths: &[&[usize]], nc: usize) -> Vec<Vec<usize>> {
    let nn = lg.len();
    // Initial order: DFS from the top ranks down (dagre's initOrder).
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); nlayers];
    let mut visited = vec![false; nn];
    let mut starts: Vec<usize> = (0..nn).collect();
    starts.sort_by_key(|&v| lg.rank[v]);
    for s in starts {
        let mut stack = vec![s];
        while let Some(v) = stack.pop() {
            if visited[v] {
                continue;
            }
            visited[v] = true;
            layers[lg.rank[v]].push(v);
            for &(w, _) in lg.down[v].iter().rev() {
                if !visited[w] {
                    stack.push(w);
                }
            }
        }
    }
    let mut pos = vec![0usize; nn];
    let mut key = vec![0f64; nn];
    let set_pos = |layer: &[usize], pos: &mut [usize]| {
        for (i, &v) in layer.iter().enumerate() {
            pos[v] = i;
        }
    };
    for l in &layers {
        set_pos(l, &mut pos);
    }
    // Group clusters, ordering sibling clusters by their mean position.
    let mut cl_rank = vec![0f64; nc];
    {
        let mut acc = vec![(0f64, 0usize); nc];
        for l in &layers {
            let len = l.len().max(1) as f64;
            for &v in l {
                key[v] = (pos[v] as f64 + 0.5) / len;
                for &c in paths[v] {
                    acc[c].0 += key[v];
                    acc[c].1 += 1;
                }
            }
        }
        for c in 0..nc {
            if acc[c].1 > 0 {
                cl_rank[c] = acc[c].0 / acc[c].1 as f64;
            }
        }
        for l in layers.iter_mut() {
            let mut out = Vec::with_capacity(l.len());
            sort_layer(l, &key, paths, &cl_rank, 0, &mut out);
            *l = out;
            set_pos(l, &mut pos);
        }
    }
    let mut best = layers.clone();
    let mut best_cc = crossings(&layers, &pos, lg);
    if best_cc == 0 || nlayers < 2 {
        return best;
    }
    let mut stall = 0;
    for it in 0..24 {
        let down = it % 2 == 0;
        let mut acc = vec![(0f64, 0usize); nc];
        let ranks: Vec<usize> = if down { (1..nlayers).collect() } else { (0..nlayers - 1).rev().collect() };
        for r in ranks {
            let adj = if down { r - 1 } else { r + 1 };
            let adj_len = layers[adj].len().max(1) as f64;
            let own_len = layers[r].len().max(1) as f64;
            for &v in &layers[r] {
                let nb = if down { &lg.up[v] } else { &lg.down[v] };
                let (mut s, mut ws, mut cnt) = (0f64, 0f64, 0f64);
                let mut plain = 0f64;
                for &(u, w) in nb {
                    let p = (pos[u] as f64 + 0.5) / adj_len;
                    s += w * p;
                    ws += w;
                    plain += p;
                    cnt += 1.;
                }
                key[v] = if cnt == 0. {
                    (pos[v] as f64 + 0.5) / own_len
                } else if ws > 0. {
                    s / ws
                } else {
                    plain / cnt
                };
                for &c in paths[v] {
                    acc[c].0 += key[v];
                    acc[c].1 += 1;
                }
            }
            let mut out = Vec::with_capacity(layers[r].len());
            sort_layer(&layers[r], &key, paths, &cl_rank, 0, &mut out);
            layers[r] = out;
            set_pos(&layers[r], &mut pos);
        }
        for c in 0..nc {
            if acc[c].1 > 0 {
                cl_rank[c] = acc[c].0 / acc[c].1 as f64;
            }
        }
        transpose(&mut layers, &mut pos, lg);
        let cc = crossings(&layers, &pos, lg);
        if cc < best_cc {
            best_cc = cc;
            best = layers.clone();
            stall = 0;
        } else {
            stall += 1;
        }
        if best_cc == 0 || stall >= 4 {
            break;
        }
    }
    best
}

/// Sorts one rank by `key`, treating each cluster (at this nesting depth)
/// as one block at its members' mean key; sibling clusters keep the global
/// order `cl_rank` so their boxes can't cross between ranks.
fn sort_layer(layer: &[usize], key: &[f64], paths: &[&[usize]], cl_rank: &[f64], depth: usize, out: &mut Vec<usize>) {
    enum Item {
        Node(usize),
        Cluster(usize, Vec<usize>),
    }
    let mut items: Vec<(f64, usize, Item)> = Vec::new();
    for (i, &v) in layer.iter().enumerate() {
        let p = paths[v];
        if p.len() <= depth {
            items.push((key[v], i, Item::Node(v)));
        } else {
            let c = p[depth];
            let found = items.iter_mut().find_map(|it| match &mut it.2 {
                Item::Cluster(d, m) if *d == c => Some(m),
                _ => None,
            });
            match found {
                Some(m) => m.push(v),
                None => items.push((0., i, Item::Cluster(c, vec![v]))),
            }
        }
    }
    for it in items.iter_mut() {
        if let Item::Cluster(_, m) = &it.2 {
            it.0 = m.iter().map(|&v| key[v]).sum::<f64>() / m.len() as f64;
        }
    }
    items.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let slots: Vec<usize> = (0..items.len()).filter(|&i| matches!(items[i].2, Item::Cluster(..))).collect();
    if slots.len() > 1 {
        let mut cl: Vec<(f64, usize, Item)> = Vec::with_capacity(slots.len());
        for &s in slots.iter().rev() {
            cl.push(items.remove(s));
        }
        let id = |it: &(f64, usize, Item)| match it.2 {
            Item::Cluster(c, _) => c,
            Item::Node(_) => 0,
        };
        cl.sort_by(|a, b| cl_rank[id(a)].total_cmp(&cl_rank[id(b)]).then(id(a).cmp(&id(b))));
        for (s, it) in slots.into_iter().zip(cl) {
            items.insert(s, it);
        }
    }
    for (_, _, it) in items {
        match it {
            Item::Node(v) => out.push(v),
            Item::Cluster(_, m) => sort_layer(&m, key, paths, cl_rank, depth + 1, out),
        }
    }
}

/// Swaps neighbours (members of the same cluster) while that removes
/// crossings.
fn transpose(layers: &mut [Vec<usize>], pos: &mut [usize], lg: &Lg) {
    let pair = |pos: &[usize], v: usize, w: usize| -> usize {
        let mut c = 0;
        for (a, b) in [(&lg.up[v], &lg.up[w]), (&lg.down[v], &lg.down[w])] {
            for &(x, _) in a {
                for &(y, _) in b {
                    if pos[x] > pos[y] {
                        c += 1;
                    }
                }
            }
        }
        c
    };
    for _ in 0..8 {
        let mut improved = false;
        for l in layers.iter_mut() {
            for i in 0..l.len().saturating_sub(1) {
                let (v, w) = (l[i], l[i + 1]);
                if lg.cluster[v] != lg.cluster[w] {
                    continue;
                }
                if pair(pos, w, v) < pair(pos, v, w) {
                    l.swap(i, i + 1);
                    pos[v] = i + 1;
                    pos[w] = i;
                    improved = true;
                }
            }
        }
        if !improved {
            break;
        }
    }
}

/// Edge crossings between consecutive ranks (Barth, Jünger & Mutzel's
/// accumulator count).
fn crossings(layers: &[Vec<usize>], pos: &[usize], lg: &Lg) -> usize {
    let mut total = 0;
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    for r in 0..layers.len().saturating_sub(1) {
        pairs.clear();
        for &u in &layers[r] {
            for &(v, _) in &lg.down[u] {
                pairs.push((pos[u], pos[v]));
            }
        }
        pairs.sort_unstable();
        let m = layers[r + 1].len();
        let mut tree = vec![0usize; m + 1];
        for (k, &(_, pv)) in pairs.iter().enumerate() {
            // Earlier edges ending at or left of pv don't cross this one.
            let mut i = pv + 1;
            let mut le = 0;
            while i > 0 {
                le += tree[i];
                i &= i - 1;
            }
            total += k - le;
            let mut i = pv + 1;
            while i <= m {
                tree[i] += 1;
                i += i.isolate_lowest_one();
            }
        }
    }
    total
}

// ---------------------------------------------------------------- coordinates

/// x per layered node, and (left, right) borders per cluster.
type XCoords = (Vec<f64>, Vec<(f64, f64)>);

#[derive(Clone, Copy)]
enum Tok {
    Node(usize),
    Open(usize),
    Close(usize),
}

/// x for every layered node and (left, right) border for every cluster,
/// from network simplex on dot's auxiliary graph: each layered edge u–v
/// becomes a node n with edges n→u, n→v whose length is |x_u − x_v|, and
/// neighbours in a rank are chained with their minimum separation.
#[allow(clippy::too_many_arguments)]
fn x_coords(
    lg: &Lg,
    layers: &[Vec<usize>],
    paths: &[&[usize]],
    span: &[Option<(usize, usize)>],
    clusters: &[ClusterIn],
    sp: &Spacing,
    horiz: bool,
    band_side: &dyn Fn(usize) -> f64,
) -> Option<XCoords> {
    let nn = lg.len();
    let nc = clusters.len();
    let pad = sp.cluster_pad;
    let gap = sp.cluster_gap;
    let id = |t: Tok| match t {
        Tok::Node(v) => v,
        Tok::Open(c) => nn + 2 * c,
        Tok::Close(c) => nn + 2 * c + 1,
    };
    let sep = |a: Tok, b: Tok| -> f64 {
        let right = match a {
            Tok::Node(v) => lg.w[v] / 2.,
            _ => 0.,
        };
        let left = match b {
            Tok::Node(v) => lg.w[v] / 2.,
            _ => 0.,
        };
        let between = match (a, b) {
            (Tok::Node(u), Tok::Node(v)) => lg.hs[u] + lg.hs[v],
            (Tok::Node(u), Tok::Open(_)) => lg.hs[u] + gap,
            (Tok::Node(_), Tok::Close(_)) => pad,
            (Tok::Open(c), Tok::Node(_)) | (Tok::Open(c), Tok::Open(_)) => pad + band_side(c),
            (Tok::Open(c), Tok::Close(_)) => 2. * pad + band_side(c),
            (Tok::Close(_), Tok::Close(_)) => pad,
            (Tok::Close(_), Tok::Open(_)) => 2. * gap,
            (Tok::Close(_), Tok::Node(v)) => gap + lg.hs[v],
        };
        right + between + left
    };
    let mut es: Vec<ns::Edge> = Vec::new();
    for layer in layers {
        let mut prev: Option<Tok> = None;
        let mut open: Vec<usize> = Vec::new();
        let mut emit = |t: Tok, prev: &mut Option<Tok>| {
            if let Some(p) = *prev {
                es.push(ns::Edge { u: id(p), v: id(t), minlen: sep(p, t).ceil() as i64, weight: 0. });
            }
            *prev = Some(t);
        };
        for &v in layer {
            let p = paths[v];
            let k = open.iter().zip(p).take_while(|(a, b)| a == b).count();
            while open.len() > k {
                let c = open.pop().unwrap_or(0);
                emit(Tok::Close(c), &mut prev);
            }
            for &c in &p[k..] {
                emit(Tok::Open(c), &mut prev);
                open.push(c);
            }
            emit(Tok::Node(v), &mut prev);
        }
        while let Some(c) = open.pop() {
            emit(Tok::Close(c), &mut prev);
        }
    }
    for c in 0..nc {
        if span[c].is_some() {
            let min_w = if horiz { 2. * pad + band_side(c) } else { clusters[c].title_w + 2. * pad };
            // A small weight keeps the box snug around its contents.
            es.push(ns::Edge { u: nn + 2 * c, v: nn + 2 * c + 1, minlen: min_w.ceil() as i64, weight: 0.5 });
        }
    }
    let mut aux = nn + 2 * nc;
    for u in 0..nn {
        for &(v, w) in &lg.down[u] {
            es.push(ns::Edge { u: aux, v: u, minlen: 0, weight: w });
            es.push(ns::Edge { u: aux, v, minlen: 0, weight: w });
            aux += 1;
        }
    }
    let r = ns::solve(aux, &es, true, MAX_PIVOTS)?;
    let x: Vec<f64> = (0..nn).map(|v| r[v] as f64).collect();
    let borders = (0..nc).map(|c| (r[nn + 2 * c] as f64, r[nn + 2 * c + 1] as f64)).collect();
    Some((x, borders))
}

#[allow(clippy::too_many_arguments)]
fn cluster_boxes(
    lg: &Lg,
    y: &[f64],
    borders: &[(f64, f64)],
    span: &[Option<(usize, usize)>],
    clusters: &[ClusterIn],
    by_depth: &[usize],
    sp: &Spacing,
    band_top: &dyn Fn(usize) -> f64,
    band_bottom: &dyn Fn(usize) -> f64,
) -> Vec<Option<Rect>> {
    let nc = clusters.len();
    let mut ext: Vec<(f64, f64)> = vec![(f64::INFINITY, f64::NEG_INFINITY); nc];
    for (v, &yv) in y.iter().enumerate() {
        if let Some(c) = lg.cluster[v] {
            ext[c].0 = ext[c].0.min(yv - lg.h[v] / 2.);
            ext[c].1 = ext[c].1.max(yv + lg.h[v] / 2.);
        }
    }
    let mut boxes: Vec<Option<Rect>> = vec![None; nc];
    for &c in by_depth {
        if span[c].is_none() || !ext[c].0.is_finite() {
            continue;
        }
        let (mut t, mut b) = ext[c];
        for d in 0..nc {
            if clusters[d].parent == Some(c)
                && let Some(bx) = boxes[d]
            {
                t = t.min(bx.y);
                b = b.max(bx.y + bx.h);
            }
        }
        t -= sp.cluster_pad + band_top(c);
        b += sp.cluster_pad + band_bottom(c);
        let (l, r) = borders[c];
        boxes[c] = Some(Rect { x: l, y: t, w: r - l, h: b - t });
        if let Some(p) = clusters[c].parent {
            ext[p].0 = ext[p].0.min(t);
            ext[p].1 = ext[p].1.max(b);
        }
    }
    boxes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sp() -> Spacing {
        Spacing { node: 40., rank: 50., edge: 16., cluster_pad: 14., cluster_gap: 10. }
    }

    fn node(w: f64, h: f64, cluster: Option<usize>) -> NodeIn {
        NodeIn { w, h, cluster }
    }

    fn edge(from: usize, to: usize) -> EdgeIn {
        EdgeIn { from, to, minlen: 1, weight: 1., label: None }
    }

    fn overlap(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.w - 0.5 && b.x < a.x + a.w - 0.5 && a.y < b.y + b.h - 0.5 && b.y < a.y + a.h - 0.5
    }

    fn rects(l: &Layout, nodes: &[NodeIn]) -> Vec<Rect> {
        l.nodes.iter().zip(nodes).map(|(&(x, y), n)| Rect { x: x - n.w / 2., y: y - n.h / 2., w: n.w, h: n.h }).collect()
    }

    #[test]
    fn chain_is_straight_and_ranked() {
        let nodes = vec![node(80., 40., None), node(60., 40., None), node(100., 40., None)];
        let edges = vec![edge(0, 1), edge(1, 2)];
        let l = layout(Dir::TB, &nodes, &edges, &[], &sp()).unwrap();
        assert!((l.nodes[0].0 - l.nodes[1].0).abs() < 1. && (l.nodes[1].0 - l.nodes[2].0).abs() < 1.);
        assert!(l.nodes[0].1 < l.nodes[1].1 && l.nodes[1].1 < l.nodes[2].1);
        // Rank gap: 40 px nodes, 50 px between them.
        assert!((l.nodes[1].1 - l.nodes[0].1 - 90.).abs() < 1., "{:?}", l.nodes);
    }

    #[test]
    fn parent_is_centred_over_children() {
        let nodes = vec![node(60., 40., None), node(60., 40., None), node(60., 40., None)];
        let l = layout(Dir::TB, &nodes, &[edge(0, 1), edge(0, 2)], &[], &sp()).unwrap();
        let mid = (l.nodes[1].0 + l.nodes[2].0) / 2.;
        assert!((l.nodes[0].0 - mid).abs() <= 1., "{:?}", l.nodes);
    }

    #[test]
    fn cycles_and_directions() {
        let nodes: Vec<NodeIn> = (0..4).map(|_| node(50., 30., None)).collect();
        let edges = vec![edge(0, 1), edge(1, 2), edge(2, 3), edge(3, 1)];
        for dir in [Dir::TB, Dir::BT, Dir::LR, Dir::RL] {
            let l = layout(dir, &nodes, &edges, &[], &sp()).unwrap();
            assert!(l.edges[3].reversed);
            let (vx, vy) = dir.vector();
            let along = |p: (f64, f64)| p.0 * vx + p.1 * vy;
            assert!(along(l.nodes[0]) < along(l.nodes[1]) && along(l.nodes[2]) < along(l.nodes[3]));
            let r = rects(&l, &nodes);
            for i in 0..r.len() {
                for j in i + 1..r.len() {
                    assert!(!overlap(r[i], r[j]), "{dir:?}: {i} overlaps {j}");
                }
            }
        }
    }

    #[test]
    fn clusters_enclose_members_and_exclude_others() {
        // 0 → 1 → 2 inside cluster 0; 3 → 4 outside, 3 also → 1; 5 in a
        // nested cluster.
        let nodes = vec![
            node(70., 40., Some(0)),
            node(70., 40., Some(0)),
            node(70., 40., Some(0)),
            node(70., 40., None),
            node(70., 40., None),
            node(70., 40., Some(1)),
        ];
        let edges = vec![edge(0, 1), edge(1, 2), edge(3, 4), edge(3, 1), edge(0, 5), edge(5, 2)];
        let clusters = vec![
            ClusterIn { parent: None, title_w: 60., title_h: 16. },
            ClusterIn { parent: Some(0), title_w: 40., title_h: 16. },
        ];
        for dir in [Dir::TB, Dir::LR] {
            let l = layout(dir, &nodes, &edges, &clusters, &sp()).unwrap();
            let r = rects(&l, &nodes);
            let c0 = l.clusters[0].unwrap();
            let c1 = l.clusters[1].unwrap();
            for (i, n) in nodes.iter().enumerate() {
                let inside = |c: Rect| r[i].x >= c.x && r[i].x + r[i].w <= c.x + c.w && r[i].y >= c.y && r[i].y + r[i].h <= c.y + c.h;
                match n.cluster {
                    Some(0) => assert!(inside(c0) && !overlap(r[i], c1), "{dir:?} node {i}"),
                    Some(1) => assert!(inside(c1) && inside(c0), "{dir:?} node {i}"),
                    _ => assert!(!overlap(r[i], c0), "{dir:?} node {i} inside the cluster"),
                }
            }
            assert!(c1.x >= c0.x && c1.x + c1.w <= c0.x + c0.w && c1.y >= c0.y && c1.y + c1.h <= c0.y + c0.h);
        }
    }

    #[test]
    fn labels_get_their_own_slot() {
        let nodes = vec![node(60., 40., None), node(60., 40., None)];
        let mut e = edge(0, 1);
        e.label = Some((50., 18.));
        let l = layout(Dir::TB, &nodes, &[e], &[], &sp()).unwrap();
        let (lx, ly) = l.edges[0].label.unwrap();
        assert!((lx - l.nodes[0].0).abs() < 1.);
        assert!(ly - 9. > l.nodes[0].1 + 20. && ly + 9. < l.nodes[1].1 - 20.);
    }

    #[test]
    fn deterministic() {
        let nodes: Vec<NodeIn> = (0..12).map(|i| node(40. + (i * 7 % 30) as f64, 36., None)).collect();
        let edges: Vec<EdgeIn> = (0..20).map(|i| edge(i * 5 % 12, (i * 7 + 3) % 12)).collect();
        let a = layout(Dir::TB, &nodes, &edges, &[], &sp()).unwrap();
        let b = layout(Dir::TB, &nodes, &edges, &[], &sp()).unwrap();
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
    }
}

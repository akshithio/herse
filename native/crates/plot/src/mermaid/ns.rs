//! Network simplex (Gansner, Koutsofios, North & Vo, "A Technique for Drawing
//! Directed Graphs", 1993), the solver behind dot and dagre.
//!
//! It minimises `Σ weight · (rank[v] − rank[u])` subject to
//! `rank[v] − rank[u] ≥ minlen` for every edge `u → v`. The layered layout
//! uses it twice: to put nodes in ranks, and (on dot's auxiliary graph) to
//! give them x coordinates.

/// A constraint `rank[v] − rank[u] ≥ minlen`, costing `weight` per unit of
/// length.
#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub u: usize,
    pub v: usize,
    pub minlen: i64,
    pub weight: f64,
}

const NONE: usize = usize::MAX;
const EPS: f64 = 1e-9;

struct Ns {
    es: Vec<Edge>,
    /// Edge indices incident to each node.
    inc: Vec<Vec<usize>>,
    /// Tree edge indices incident to each node.
    tadj: Vec<Vec<usize>>,
    rank: Vec<i64>,
    tree: Vec<bool>,
    cut: Vec<f64>,
    low: Vec<usize>,
    lim: Vec<usize>,
    parent_edge: Vec<usize>,
    /// The node with each `lim` (a subtree is a contiguous run of these).
    by_lim: Vec<usize>,
    /// Scratch for DFS.
    stack: Vec<(usize, usize)>,
}

/// Leaving-edge candidates compared per pivot (graphviz's `searchsize`).
const SEARCH: usize = 30;

/// Ranks for `n` nodes (normalised so the smallest is 0), or `None` when the
/// constraints are cyclic. `balance` shifts subtrees whose position doesn't
/// change the cost to the middle of their slack (dot's `LR_balance`), which
/// centres a parent over its children when it's used for x coordinates.
/// At most `max_iter` pivots are made; the result is feasible either way.
pub fn solve(n: usize, edges: &[Edge], balance: bool, max_iter: usize) -> Option<Vec<i64>> {
    let ns = run(n, edges, balance, max_iter)?;
    let min = ns.rank[..n].iter().copied().min().unwrap_or(0);
    Some(ns.rank[..n].iter().map(|r| r - min).collect())
}

fn run(n: usize, edges: &[Edge], balance: bool, max_iter: usize) -> Option<Ns> {
    let mut es: Vec<Edge> = Vec::with_capacity(edges.len() + 4);
    for e in edges {
        if e.u >= n || e.v >= n {
            return None;
        }
        if e.u == e.v {
            if e.minlen > 0 {
                return None;
            }
            continue;
        }
        es.push(*e);
    }
    // One virtual root ties the components together (weight 0: it costs
    // nothing and never pulls anything).
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    for e in &es {
        let (a, b) = (find(&mut parent, e.u), find(&mut parent, e.v));
        if a != b {
            parent[a] = b;
        }
    }
    let mut reps: Vec<usize> = Vec::new();
    for v in 0..n {
        if find(&mut parent, v) == v {
            reps.push(v);
        }
    }
    let total = if reps.len() > 1 { n + 1 } else { n };
    if reps.len() > 1 {
        for &r in &reps {
            es.push(Edge { u: n, v: r, minlen: 0, weight: 0. });
        }
    }
    let m = es.len();
    let mut inc = vec![Vec::new(); total];
    for (i, e) in es.iter().enumerate() {
        inc[e.u].push(i);
        inc[e.v].push(i);
    }
    let rank = longest_path(total, &es, &inc)?;
    let mut ns = Ns {
        es,
        inc,
        tadj: vec![Vec::new(); total],
        rank,
        tree: vec![false; m],
        cut: vec![0.; m],
        low: vec![0; total],
        lim: vec![0; total],
        parent_edge: vec![NONE; total],
        by_lim: vec![0; total + 1],
        stack: Vec::with_capacity(total),
    };
    if total == 0 {
        return Some(ns);
    }
    ns.feasible_tree()?;
    ns.dfs_range(0, 1);
    let mut post: Vec<usize> = (0..total).collect();
    post.sort_by_key(|&v| ns.lim[v]);
    for &v in &post {
        ns.set_cut(v);
    }
    let mut start = 0;
    let mut cycle: Vec<usize> = Vec::new();
    for _ in 0..max_iter {
        let Some(e) = ns.leave_edge(&mut start) else { break };
        let Some(f) = ns.enter_edge(e) else { break };
        ns.exchange(e, f, &mut cycle);
    }
    if balance {
        ns.balance();
    }
    Some(ns)
}

/// Initial feasible ranks: each node as high as its predecessors allow.
fn longest_path(n: usize, es: &[Edge], inc: &[Vec<usize>]) -> Option<Vec<i64>> {
    let mut indeg = vec![0usize; n];
    for e in es {
        indeg[e.v] += 1;
    }
    let mut rank = vec![0i64; n];
    let mut queue: Vec<usize> = (0..n).filter(|&v| indeg[v] == 0).collect();
    let mut done = 0;
    while let Some(u) = queue.pop() {
        done += 1;
        for &ei in &inc[u] {
            let e = &es[ei];
            if e.u != u {
                continue;
            }
            rank[e.v] = rank[e.v].max(rank[u] + e.minlen);
            indeg[e.v] -= 1;
            if indeg[e.v] == 0 {
                queue.push(e.v);
            }
        }
    }
    (done == n).then_some(rank)
}

impl Ns {
    fn slack(&self, ei: usize) -> i64 {
        let e = &self.es[ei];
        self.rank[e.v] - self.rank[e.u] - e.minlen
    }

    fn other(&self, ei: usize, v: usize) -> usize {
        let e = &self.es[ei];
        if e.u == v { e.v } else { e.u }
    }

    fn add_tree_edge(&mut self, ei: usize) {
        self.tree[ei] = true;
        let e = self.es[ei];
        self.tadj[e.u].push(ei);
        self.tadj[e.v].push(ei);
    }

    fn remove_tree_edge(&mut self, ei: usize) {
        self.tree[ei] = false;
        let e = self.es[ei];
        for v in [e.u, e.v] {
            if let Some(k) = self.tadj[v].iter().position(|&x| x == ei) {
                self.tadj[v].remove(k);
            }
        }
    }

    /// Grows a tree of tight edges from node 0, shifting it by the smallest
    /// slack whenever it gets stuck.
    fn feasible_tree(&mut self) -> Option<()> {
        let n = self.rank.len();
        let mut in_tree = vec![false; n];
        in_tree[0] = true;
        let mut size = 1;
        let mut stack = vec![0];
        loop {
            while let Some(v) = stack.pop() {
                for k in 0..self.inc[v].len() {
                    let ei = self.inc[v][k];
                    let w = self.other(ei, v);
                    if !in_tree[w] && self.slack(ei) == 0 {
                        in_tree[w] = true;
                        size += 1;
                        self.add_tree_edge(ei);
                        stack.push(w);
                    }
                }
            }
            if size == n {
                return Some(());
            }
            let mut best: Option<(usize, i64)> = None;
            for (ei, e) in self.es.iter().enumerate() {
                if in_tree[e.u] != in_tree[e.v] {
                    let s = self.rank[e.v] - self.rank[e.u] - e.minlen;
                    if best.is_none_or(|(_, b)| s < b) {
                        best = Some((ei, s));
                    }
                }
            }
            let (ei, s) = best?;
            let e = self.es[ei];
            let delta = if in_tree[e.u] { s } else { -s };
            for (r, &t) in self.rank.iter_mut().zip(&in_tree) {
                if t {
                    *r += delta;
                }
            }
            stack.push(if in_tree[e.u] { e.u } else { e.v });
        }
    }

    /// Postorder numbers (`lim`), subtree minimums (`low`) and parent edges
    /// for the subtree under `root` (whose own parent edge stays), numbering
    /// from `first`.
    fn dfs_range(&mut self, root: usize, first: usize) {
        let mut next = first;
        let mut stack = std::mem::take(&mut self.stack);
        stack.clear();
        stack.push((root, 0));
        self.low[root] = next;
        while let Some(top) = stack.last_mut() {
            let (v, i) = *top;
            if i < self.tadj[v].len() {
                top.1 += 1;
                let ei = self.tadj[v][i];
                if ei == self.parent_edge[v] {
                    continue;
                }
                let w = self.other(ei, v);
                self.parent_edge[w] = ei;
                self.low[w] = next;
                stack.push((w, 0));
            } else {
                self.lim[v] = next;
                self.by_lim[next] = v;
                next += 1;
                stack.pop();
            }
        }
        self.stack = stack;
    }

    /// The cut value of `child`'s parent edge, from its other edges and its
    /// children's cut values (dagre's `calcCutValue`).
    fn set_cut(&mut self, child: usize) {
        let pe = self.parent_edge[child];
        if pe == NONE {
            return;
        }
        let child_is_tail = self.es[pe].u == child;
        let mut cut = self.es[pe].weight;
        for &ei in &self.inc[child] {
            if ei == pe {
                continue;
            }
            let e = &self.es[ei];
            let points_to_head = (e.u == child) == child_is_tail;
            cut += if points_to_head { e.weight } else { -e.weight };
            if self.tree[ei] {
                let c = self.cut[ei];
                cut += if points_to_head { -c } else { c };
            }
        }
        self.cut[pe] = cut;
    }

    /// The most negative cut value among the next few negative ones.
    fn leave_edge(&self, start: &mut usize) -> Option<usize> {
        let m = self.es.len();
        let mut best: Option<usize> = None;
        let mut found = 0;
        for k in 0..m {
            let ei = (*start + k) % m;
            if self.tree[ei] && self.cut[ei] < -EPS {
                if best.is_none_or(|b| self.cut[ei] < self.cut[b]) {
                    best = Some(ei);
                }
                found += 1;
                if found >= SEARCH {
                    *start = (ei + 1) % m;
                    return best;
                }
            }
        }
        best
    }

    /// The non-tree edge with the least slack that reconnects the two halves
    /// the tree splits into without `ei`, crossing in the direction that
    /// lets `ei`'s cut shrink.
    ///
    /// Only edges at the smaller side of the cut are looked at; ties go to
    /// the lowest edge index.
    fn enter_edge(&self, ei: usize) -> Option<usize> {
        let e = &self.es[ei];
        let (sub, flip) = if self.lim[e.u] > self.lim[e.v] { (e.v, true) } else { (e.u, false) };
        let (lo, hi) = (self.low[sub], self.lim[sub]);
        let desc = |x: usize| lo <= self.lim[x] && self.lim[x] <= hi;
        let mut best: Option<(usize, i64)> = None;
        for k in self.side(lo, hi) {
            let v = self.by_lim[k];
            for &fi in &self.inc[v] {
                let f = &self.es[fi];
                if flip == desc(f.u) && flip != desc(f.v) {
                    let s = self.rank[f.v] - self.rank[f.u] - f.minlen;
                    if best.is_none_or(|(bi, b)| s < b || (s == b && fi < bi)) {
                        best = Some((fi, s));
                    }
                }
            }
        }
        best.map(|b| b.0)
    }

    /// The `lim`s of the smaller side of a cut: the subtree `lo..=hi`, or
    /// everything else.
    fn side(&self, lo: usize, hi: usize) -> impl Iterator<Item = usize> + use<> {
        let n = self.rank.len();
        if 2 * (hi + 1 - lo) <= n { (lo..hi + 1).chain(0..0) } else { (1..lo).chain(hi + 1..n + 1) }
    }

    /// Moves the subtree `lo..=hi` by `d` relative to the rest (moving the
    /// rest when it's smaller: only differences matter).
    fn shift_subtree(&mut self, lo: usize, hi: usize, d: i64) {
        let inside = 2 * (hi + 1 - lo) <= self.rank.len();
        for k in self.side(lo, hi) {
            let v = self.by_lim[k];
            self.rank[v] += if inside { d } else { -d };
        }
    }

    /// Swaps tree edge `e` for `f`, updating ranks, numbering and cut values
    /// only where they change (graphviz's `update`).
    fn exchange(&mut self, e: usize, f: usize, cycle: &mut Vec<usize>) {
        // The subtree cut off by e moves so f becomes tight.
        let ee = self.es[e];
        let sub = if self.lim[ee.u] < self.lim[ee.v] { ee.u } else { ee.v };
        let (lo, hi) = (self.low[sub], self.lim[sub]);
        let fe = self.es[f];
        let delta = self.slack(f);
        if delta != 0 {
            let shift = if lo <= self.lim[fe.u] && self.lim[fe.u] <= hi { delta } else { -delta };
            self.shift_subtree(lo, hi, shift);
        }
        // The cycle f closes: the tree paths from f's ends up to their
        // lowest common ancestor.
        cycle.clear();
        let is_anc = |s: &Ns, a: usize, x: usize| s.low[a] <= s.lim[x] && s.lim[x] <= s.lim[a];
        let mut lca = fe.u;
        while !is_anc(self, lca, fe.v) {
            cycle.push(lca);
            lca = self.other(self.parent_edge[lca], lca);
        }
        let mut v = fe.v;
        while v != lca {
            cycle.push(v);
            v = self.other(self.parent_edge[v], v);
        }
        self.remove_tree_edge(e);
        self.add_tree_edge(f);
        let first = self.low[lca];
        self.dfs_range(lca, first);
        // Children before parents.
        cycle.sort_by_key(|&v| self.lim[v]);
        for &v in cycle.iter() {
            self.set_cut(v);
        }
        self.cut[e] = 0.;
    }

    /// dot's `LR_balance`: a subtree hanging off a zero-cut tree edge can
    /// move without changing the cost, so move it to the middle of its room.
    fn balance(&mut self) {
        for ei in 0..self.es.len() {
            if !self.tree[ei] || self.cut[ei].abs() > EPS {
                continue;
            }
            let Some(f) = self.enter_edge(ei) else { continue };
            let delta = self.slack(f);
            if delta <= 1 {
                continue;
            }
            let e = self.es[ei];
            let (sub, shift) = if self.lim[e.u] < self.lim[e.v] { (e.u, -(delta / 2)) } else { (e.v, delta / 2) };
            let (lo, hi) = (self.low[sub], self.lim[sub]);
            self.shift_subtree(lo, hi, shift);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(u: usize, v: usize, minlen: i64, weight: f64) -> Edge {
        Edge { u, v, minlen, weight }
    }

    fn cost(edges: &[Edge], r: &[i64]) -> f64 {
        edges.iter().map(|e| e.weight * (r[e.v] - r[e.u]) as f64).sum()
    }

    #[test]
    fn ranks_respect_minlen_and_shorten_edges() {
        // a → b → c → d, and a → d: longest path would leave a → d long
        // either way; x → d alone should sit right above d.
        let edges = [e(0, 1, 1, 1.), e(1, 2, 1, 1.), e(2, 3, 1, 1.), e(0, 3, 1, 1.), e(4, 3, 1, 1.)];
        let r = solve(5, &edges, false, 1000).unwrap();
        for ed in &edges {
            assert!(r[ed.v] - r[ed.u] >= ed.minlen);
        }
        assert_eq!(r[3] - r[4], 1, "{r:?}");
        assert_eq!(cost(&edges, &r), 7.);
    }

    #[test]
    fn cycles_are_rejected() {
        assert!(solve(2, &[e(0, 1, 1, 1.), e(1, 0, 1, 1.)], false, 100).is_none());
        assert!(solve(1, &[e(0, 0, 1, 1.)], false, 100).is_none());
    }

    #[test]
    fn disconnected_components() {
        let r = solve(4, &[e(0, 1, 2, 1.), e(2, 3, 3, 1.)], false, 100).unwrap();
        assert_eq!(r[1] - r[0], 2);
        assert_eq!(r[3] - r[2], 3);
    }

    /// At the end every tree edge is tight and, recomputed from scratch,
    /// no cut value is negative: the optimality certificate.
    #[test]
    fn incremental_updates_reach_an_optimum() {
        let mut seed = 99u64;
        let mut rnd = |k: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % k
        };
        for _ in 0..200 {
            let n = 2 + rnd(40);
            let mut edges = Vec::new();
            for _ in 0..rnd(3 * n) {
                let (a, b) = (rnd(n), rnd(n));
                if a < b {
                    edges.push(e(a, b, rnd(4) as i64, (1 + rnd(8)) as f64));
                }
            }
            let mut ns = run(n, &edges, false, 100_000).unwrap();
            for ed in &ns.es {
                assert!(ns.rank[ed.v] - ns.rank[ed.u] >= ed.minlen);
            }
            for ei in 0..ns.es.len() {
                if ns.tree[ei] {
                    assert_eq!(ns.slack(ei), 0);
                }
            }
            let incremental = ns.cut.clone();
            ns.parent_edge.iter_mut().for_each(|p| *p = NONE);
            ns.dfs_range(0, 1);
            let mut post: Vec<usize> = (0..ns.rank.len()).collect();
            post.sort_by_key(|&v| ns.lim[v]);
            for &v in &post {
                ns.set_cut(v);
            }
            for (ei, inc) in incremental.iter().enumerate() {
                if ns.tree[ei] {
                    assert!(ns.cut[ei] >= -EPS, "negative cut value at the end");
                    assert!((ns.cut[ei] - inc).abs() < 1e-6, "incremental cut value drifted");
                }
            }
        }
    }

    #[test]
    fn balance_centres_a_parent() {
        // dot's auxiliary graph for a parent p over children c1, c2 (100
        // apart): n1 → p, n1 → c1, n2 → p, n2 → c2, c1 → c2.
        let (p, c1, c2, n1, n2) = (0, 1, 2, 3, 4);
        let edges = [e(n1, p, 0, 1.), e(n1, c1, 0, 1.), e(n2, p, 0, 1.), e(n2, c2, 0, 1.), e(c1, c2, 100, 0.)];
        let r = solve(5, &edges, true, 100).unwrap();
        assert_eq!(r[c2] - r[c1], 100);
        assert_eq!(r[p] - r[c1], 50, "{r:?}");
    }
}

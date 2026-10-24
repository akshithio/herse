//! Quick-open fuzzy matching over workspace-relative paths (nucleo).
//!
//! Ranking: every query word must match the full path (fzf-style, path-aware
//! bonuses). The part of the score earned inside the file name is counted
//! twice, plus a bonus when *every* word matches inside the file name — so
//! `plot` ranks `demos/Plot.ipynb` above `plotly/Area.ipynb`, while
//! `bokeh area` still finds `bokeh/Area.ipynb`. Notebooks and recently opened
//! files get a small nudge. Ties: recency, then shorter path, then input order.
//!
//! `query` is incremental: when the new query extends the previous one, only
//! the previous matches are rescored.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str, Utf32String};
use std::cmp::Ordering;
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyHit {
    /// Index into the paths given to [`FuzzyIndex::new`].
    pub index: usize,
    pub score: u32,
    /// Byte offsets (into the path string) of the matched characters, sorted,
    /// for highlighting.
    pub positions: Vec<u32>,
}

const NAME_BONUS: u32 = 24;
const NOTEBOOK_BONUS: u32 = 8;
const RECENT_BONUS: u32 = 20;
const NOT_RECENT: u32 = u32::MAX;
#[cfg(windows)]
const SEPARATORS: &[char] = &['/', '\\'];
#[cfg(not(windows))]
const SEPARATORS: &[char] = &['/'];

pub struct FuzzyIndex {
    paths: Vec<String>,
    hay: Vec<Utf32String>,
    /// Character index where the file name starts within `hay`.
    name_start: Vec<u32>,
    notebook: Vec<bool>,
    recent_rank: Vec<u32>,
    recent_order: Vec<usize>,
    matcher: Matcher,
    last_query: String,
    last_matches: Vec<u32>,
}

impl FuzzyIndex {
    /// Builds an index over `paths` (typically [`crate::WalkEntry::rel`]).
    pub fn new(paths: Vec<String>) -> Self {
        let mut hay = Vec::with_capacity(paths.len());
        let mut name_start = Vec::with_capacity(paths.len());
        let mut notebook = Vec::with_capacity(paths.len());
        for p in &paths {
            let u = Utf32String::from(p.as_str());
            let start = match p.rfind(SEPARATORS) {
                None => 0,
                Some(b) if p.is_ascii() => b + 1,
                Some(b) => p[..=b].graphemes(true).count(),
            };
            name_start.push(start as u32);
            hay.push(u);
            notebook.push(p.len() >= 6 && p[p.len() - 6..].eq_ignore_ascii_case(".ipynb"));
        }
        let n = paths.len();
        FuzzyIndex {
            paths,
            hay,
            name_start,
            notebook,
            recent_rank: vec![NOT_RECENT; n],
            recent_order: Vec::new(),
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            last_query: String::new(),
            last_matches: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn path(&self, index: usize) -> &str {
        &self.paths[index]
    }

    pub fn paths(&self) -> &[String] {
        &self.paths
    }

    /// Most-recent-first list of paths (same form as the indexed paths);
    /// unknown paths are ignored. Recents come first for an empty query and
    /// get a small boost otherwise.
    pub fn set_recents<S: AsRef<str>>(&mut self, recents: &[S]) {
        self.recent_rank.iter_mut().for_each(|r| *r = NOT_RECENT);
        self.recent_order.clear();
        let lookup: HashMap<&str, usize> = self.paths.iter().enumerate().map(|(i, p)| (p.as_str(), i)).collect();
        for r in recents {
            if let Some(&i) = lookup.get(r.as_ref())
                && self.recent_rank[i] == NOT_RECENT
            {
                self.recent_rank[i] = self.recent_order.len() as u32;
                self.recent_order.push(i);
            }
        }
    }

    /// Returns up to `limit` best matches, best first. An empty (or
    /// all-whitespace) query lists recents first, then the rest in input order.
    pub fn query(&mut self, q: &str, limit: usize) -> Vec<FuzzyHit> {
        let pattern = Pattern::new(q, CaseMatching::Smart, Normalization::Smart, AtomKind::Fuzzy);
        if pattern.atoms.is_empty() {
            self.last_query.clear();
            self.last_matches.clear();
            return self.recents_first(limit);
        }

        let incremental = !self.last_query.is_empty() && q.starts_with(&self.last_query) && !self.last_query.ends_with('\\');
        let candidates: Vec<u32> = if incremental {
            std::mem::take(&mut self.last_matches)
        } else {
            (0..self.paths.len() as u32).collect()
        };

        let mut scored: Vec<(u32, u32)> = Vec::with_capacity(candidates.len().min(4096));
        for &i in &candidates {
            if let Some(s) = self.score(&pattern, i as usize) {
                scored.push((s, i));
            }
        }
        self.last_query.clear();
        self.last_query.push_str(q);
        self.last_matches = scored.iter().map(|&(_, i)| i).collect();
        self.last_matches.sort_unstable();

        let cmp = |a: &(u32, u32), b: &(u32, u32)| self.rank_cmp(*a, *b);
        if scored.len() > limit && limit > 0 {
            scored.select_nth_unstable_by(limit - 1, cmp);
            scored.truncate(limit);
        }
        scored.truncate(limit);
        scored.sort_unstable_by(cmp);

        scored
            .into_iter()
            .map(|(score, i)| FuzzyHit { index: i as usize, score, positions: self.positions(&pattern, i as usize) })
            .collect()
    }

    fn rank_cmp(&self, a: (u32, u32), b: (u32, u32)) -> Ordering {
        b.0.cmp(&a.0)
            .then_with(|| self.recent_rank[a.1 as usize].cmp(&self.recent_rank[b.1 as usize]))
            .then_with(|| self.paths[a.1 as usize].len().cmp(&self.paths[b.1 as usize].len()))
            .then_with(|| a.1.cmp(&b.1))
    }

    fn recents_first(&self, limit: usize) -> Vec<FuzzyHit> {
        let hit = |i: usize| FuzzyHit { index: i, score: 0, positions: Vec::new() };
        let mut out: Vec<FuzzyHit> = self.recent_order.iter().take(limit).map(|&i| hit(i)).collect();
        if out.len() < limit {
            out.extend(
                (0..self.paths.len()).filter(|&i| self.recent_rank[i] == NOT_RECENT).take(limit - out.len()).map(hit),
            );
        }
        out
    }

    fn score(&mut self, pattern: &Pattern, i: usize) -> Option<u32> {
        let hay = self.hay[i].slice(..);
        let mut total = 0u32;
        for atom in &pattern.atoms {
            total += atom.score(hay, &mut self.matcher)? as u32;
        }
        let ns = self.name_start[i] as usize;
        if ns == 0 {
            total = total * 2 + NAME_BONUS;
        } else {
            let name = hay.slice(ns..);
            let mut all = true;
            for atom in &pattern.atoms {
                match atom.score(name, &mut self.matcher) {
                    Some(s) => total += s as u32,
                    None => all = false,
                }
            }
            if all {
                total += NAME_BONUS;
            }
        }
        if self.notebook[i] {
            total += NOTEBOOK_BONUS;
        }
        let r = self.recent_rank[i];
        if r != NOT_RECENT {
            total += RECENT_BONUS.saturating_sub(r);
        }
        Some(total)
    }

    fn positions(&mut self, pattern: &Pattern, i: usize) -> Vec<u32> {
        let hay = self.hay[i].slice(..);
        let ns = self.name_start[i];
        let name: Utf32Str<'_> = hay.slice(ns as usize..);
        let mut out = Vec::new();
        let mut tmp = Vec::new();
        for atom in &pattern.atoms {
            tmp.clear();
            if ns > 0 && atom.indices(name, &mut self.matcher, &mut tmp).is_some() {
                out.extend(tmp.iter().map(|&p| p + ns));
            } else {
                tmp.clear();
                atom.indices(hay, &mut self.matcher, &mut tmp);
                out.extend_from_slice(&tmp);
            }
        }
        out.sort_unstable();
        out.dedup();
        let path = &self.paths[i];
        if !path.is_ascii() {
            let offsets: Vec<u32> = path.grapheme_indices(true).map(|(b, _)| b as u32).collect();
            for p in &mut out {
                *p = offsets.get(*p as usize).copied().unwrap_or(path.len() as u32);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(paths: &[&str]) -> FuzzyIndex {
        FuzzyIndex::new(paths.iter().map(|s| s.to_string()).collect())
    }

    fn top(idx: &mut FuzzyIndex, q: &str, n: usize) -> Vec<String> {
        idx.query(q, n).into_iter().map(|h| idx.path(h.index).to_string()).collect()
    }

    #[test]
    fn filename_beats_directory() {
        let mut idx = index(&[
            "plotly/Area.ipynb",
            "plotly/Bars.ipynb",
            "gallery/demos/Plot.ipynb",
            "matplotlib/Scatter_plot.ipynb",
            "docs/plotting_guide.md",
        ]);
        let got = top(&mut idx, "plot", 5);
        assert_eq!(got[0], "gallery/demos/Plot.ipynb");
        assert!(got.iter().position(|p| p == "matplotlib/Scatter_plot.ipynb").unwrap() < got.iter().position(|p| p == "plotly/Area.ipynb").unwrap());
        assert_eq!(got.len(), 5);

        // Directory + file words.
        let got = top(&mut idx, "plotly bars", 5);
        assert_eq!(got, ["plotly/Bars.ipynb"]);
        let got = top(&mut idx, "ply/area", 5);
        assert_eq!(got[0], "plotly/Area.ipynb");
    }

    #[test]
    fn prefers_contiguous_and_shorter() {
        let mut idx = index(&["a/b/c/data_cleaning_v2.ipynb", "data_cleaning.ipynb", "d/a/t/a.ipynb", "deep/learning/model.ipynb"]);
        let got = top(&mut idx, "dataclean", 4);
        assert_eq!(&got[..2], ["data_cleaning.ipynb", "a/b/c/data_cleaning_v2.ipynb"]);
        assert!(!got.contains(&"deep/learning/model.ipynb".to_string()));
    }

    #[test]
    fn positions_are_byte_offsets() {
        let mut idx = index(&["über/naïve.ipynb", "x/abc.ipynb"]);
        let h = &idx.query("abc", 1)[0];
        assert_eq!(h.positions, [2, 3, 4]);
        let h = &idx.query("naive", 1)[0];
        let p = idx.path(h.index).to_string();
        let s: String = h.positions.iter().map(|&b| p[b as usize..].chars().next().unwrap()).collect();
        assert_eq!(s, "naïve");
    }

    #[test]
    fn empty_query_lists_recents_first() {
        let mut idx = index(&["a.ipynb", "b.ipynb", "c.ipynb", "d.ipynb"]);
        idx.set_recents(&["c.ipynb", "missing.ipynb", "a.ipynb"]);
        assert_eq!(top(&mut idx, "", 10), ["c.ipynb", "a.ipynb", "b.ipynb", "d.ipynb"]);
        assert_eq!(top(&mut idx, "  ", 2), ["c.ipynb", "a.ipynb"]);
        // Recents break ties.
        assert_eq!(top(&mut idx, "ipynb", 1), ["c.ipynb"]);
    }

    #[test]
    fn incremental_matches_full_rescan() {
        let paths: Vec<String> = (0..500).map(|i| format!("dir{}/sub{}/notebook_{i}.ipynb", i % 7, i % 13)).collect();
        let mut inc = FuzzyIndex::new(paths.clone());
        let q = "dir3/note 12";
        for end in 1..=q.len() {
            let a = inc.query(&q[..end], 20);
            let mut fresh = FuzzyIndex::new(paths.clone());
            let b = fresh.query(&q[..end], 20);
            assert_eq!(a, b, "query {:?}", &q[..end]);
        }
        // Smart case: uppercase narrows.
        assert!(inc.query("NOTEBOOK", 10).is_empty());
    }
}

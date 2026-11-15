//! Client/host-side mirror of a remote UI tree with revision checking,
//! breadcrumbs and fuzzy search for the action palette.

use crate::fuzzy::{fold, Matcher, Prepared};
use crate::{DomError, Result};
use aether_protocol::{DomNode, DomRole, DomSnapshot, DomTreeDiff};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A ranked palette entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub node_id: u64,
    /// Higher is better. Interactive & enabled nodes carry [`ACTIONABLE_BONUS`].
    pub score: u32,
    /// Display label (name, or value/description/role when unnamed).
    pub label: String,
    pub role: DomRole,
    /// Ancestor names, outermost first, joined with `" › "`.
    pub breadcrumb: String,
    /// Char indices into `label` to highlight (empty when the match was in
    /// another field, e.g. the description).
    pub matched_indices: Vec<usize>,
}

/// Added to the score of interactive, enabled nodes so they always rank first.
pub const ACTIONABLE_BONUS: u32 = 1_000_000;
pub const BREADCRUMB_SEPARATOR: &str = " › ";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DomTree {
    loaded: bool,
    revision: u64,
    app_name: String,
    window_title: String,
    pid: u32,
    root: u64,
    nodes: HashMap<u64, DomNode>,
}

impl DomTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_snapshot(s: DomSnapshot) -> Self {
        let mut t = Self::new();
        t.apply_snapshot(s);
        t
    }

    /// Replace the whole tree.
    pub fn apply_snapshot(&mut self, s: DomSnapshot) {
        self.loaded = true;
        self.revision = s.revision;
        self.app_name = s.app_name;
        self.window_title = s.window_title;
        self.pid = s.pid;
        self.root = s.root;
        self.nodes = s.nodes.into_iter().map(|n| (n.id, n)).collect();
    }

    /// Apply an incremental update. Fails without modifying the tree when the
    /// diff does not start at our revision (or no snapshot was received yet);
    /// such errors satisfy [`DomError::needs_resync`] and the caller should
    /// send `DomMessage::Resync`.
    pub fn apply_diff(&mut self, d: &DomTreeDiff) -> Result<()> {
        if !self.loaded {
            return Err(DomError::NoSnapshot);
        }
        if d.base_revision != self.revision {
            return Err(DomError::RevisionMismatch { expected: self.revision, got: d.base_revision });
        }
        let new_root = d.root.unwrap_or(self.root);
        let root_ok = d.upserted.iter().any(|n| n.id == new_root)
            || (self.nodes.contains_key(&new_root) && !d.removed.contains(&new_root));
        if !root_ok {
            return Err(DomError::InvalidDiff(format!("root {new_root} missing after diff")));
        }
        for id in &d.removed {
            self.nodes.remove(id);
        }
        for n in &d.upserted {
            self.nodes.insert(n.id, n.clone());
        }
        self.root = new_root;
        if let Some(a) = &d.app_name {
            self.app_name.clone_from(a);
        }
        if let Some(w) = &d.window_title {
            self.window_title.clone_from(w);
        }
        self.revision = d.revision;
        Ok(())
    }

    pub fn node(&self, id: u64) -> Option<&DomNode> {
        self.nodes.get(&id)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn root(&self) -> Option<u64> {
        self.loaded.then_some(self.root)
    }

    pub fn app_name(&self) -> &str {
        &self.app_name
    }

    pub fn window_title(&self) -> &str {
        &self.window_title
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Ancestor ids, nearest first (cycle-safe).
    pub fn ancestors(&self, id: u64) -> Vec<u64> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut cur = self.nodes.get(&id).and_then(|n| n.parent);
        while let Some(p) = cur {
            if !seen.insert(p) {
                break;
            }
            out.push(p);
            cur = self.nodes.get(&p).and_then(|n| n.parent);
        }
        out
    }

    /// Named ancestors (excluding the node itself and the application root),
    /// outermost first, e.g. `"File › Open Recent"`.
    pub fn breadcrumb(&self, id: u64) -> String {
        let mut names: Vec<&str> = self
            .ancestors(id)
            .into_iter()
            .filter(|&a| a != self.root)
            .filter_map(|a| self.nodes.get(&a))
            .map(|n| n.name.trim())
            .filter(|s| !s.is_empty())
            .collect();
        names.reverse();
        names.join(BREADCRUMB_SEPARATOR)
    }

    /// Nodes in pre-order from the root, then unreachable nodes by id.
    pub fn preorder(&self) -> Vec<&DomNode> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut seen = HashSet::with_capacity(self.nodes.len());
        let mut stack = vec![self.root];
        while let Some(id) = stack.pop() {
            let Some(n) = self.nodes.get(&id) else { continue };
            if !seen.insert(id) {
                continue;
            }
            out.push(n);
            stack.extend(n.children.iter().rev());
        }
        if out.len() < self.nodes.len() {
            let mut rest: Vec<&DomNode> = self.nodes.values().filter(|n| !seen.contains(&n.id)).collect();
            rest.sort_by_key(|n| n.id);
            out.extend(rest);
        }
        out
    }

    /// Rebuild a snapshot (nodes in [`Self::preorder`] order).
    pub fn to_snapshot(&self) -> DomSnapshot {
        DomSnapshot {
            revision: self.revision,
            app_name: self.app_name.clone(),
            window_title: self.window_title.clone(),
            pid: self.pid,
            root: self.root,
            nodes: self.preorder().into_iter().cloned().collect(),
        }
    }

    /// Fuzzy search over name, value, description and shortcut.
    ///
    /// * case- and diacritics-insensitive ("salva" finds "Salva…", "perche" finds "Perché");
    /// * whitespace-separated tokens must all match (in any field);
    /// * interactive & enabled nodes rank above everything else;
    /// * an empty query lists actionable nodes in tree order.
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        if limit == 0 || !self.loaded {
            return Vec::new();
        }
        let folded_query = fold(query);
        let tokens: Vec<Vec<char>> =
            folded_query.split_whitespace().map(|t| t.chars().collect()).filter(|t: &Vec<char>| !t.is_empty()).collect();
        let order = self.preorder();
        if tokens.is_empty() {
            return order
                .into_iter()
                .filter(|n| n.id != self.root && actionable(n))
                .take(limit)
                .map(|n| self.hit(n, ACTIONABLE_BONUS, Vec::new()))
                .collect();
        }
        let whole: String = folded_query.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut matcher = Matcher::new();
        let mut hits: Vec<(u32, usize, SearchHit)> = Vec::new();
        for (pos, n) in order.into_iter().enumerate() {
            if n.id == self.root {
                continue;
            }
            let Some((raw, indices)) = score_node(&mut matcher, n, &tokens, &whole) else { continue };
            let score = raw.max(1) as u32 + if actionable(n) { ACTIONABLE_BONUS } else { 0 };
            let label_is_name = !n.name.trim().is_empty();
            let hit = self.hit(n, score, if label_is_name { indices } else { Vec::new() });
            hits.push((score, pos, hit));
        }
        hits.sort_by(|a, b| {
            b.0.cmp(&a.0).then(a.2.label.chars().count().cmp(&b.2.label.chars().count())).then(a.1.cmp(&b.1))
        });
        hits.into_iter().take(limit).map(|(_, _, h)| h).collect()
    }

    fn hit(&self, n: &DomNode, score: u32, matched_indices: Vec<usize>) -> SearchHit {
        SearchHit {
            node_id: n.id,
            score,
            label: label(n),
            role: n.role,
            breadcrumb: self.breadcrumb(n.id),
            matched_indices,
        }
    }
}

fn actionable(n: &DomNode) -> bool {
    n.enabled && (n.role.is_interactive() || n.role.accepts_text())
}

/// Display label for a node.
pub fn label(n: &DomNode) -> String {
    [Some(n.name.as_str()), n.value.as_deref(), n.description.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(n.native_role.as_str())
        .to_string()
}

/// Per-field penalties: name matches are worth most.
const PENALTY_VALUE: i32 = 12;
const PENALTY_DESCRIPTION: i32 = 16;
const PENALTY_SHORTCUT: i32 = 20;
const BONUS_EXACT: i32 = 80;
const BONUS_PREFIX: i32 = 40;

fn trim_trailing_punct(s: &str) -> &str {
    s.trim_end_matches(|c: char| !c.is_alphanumeric())
}

/// Score a node; returns raw score and highlighted indices into the name.
fn score_node(m: &mut Matcher, n: &DomNode, tokens: &[Vec<char>], whole: &str) -> Option<(i32, Vec<usize>)> {
    let name = Prepared::new(&n.name);
    let others: Vec<(Prepared, i32)> = [
        (n.value.as_deref(), PENALTY_VALUE),
        (n.description.as_deref(), PENALTY_DESCRIPTION),
        (n.shortcut.as_deref(), PENALTY_SHORTCUT),
    ]
    .into_iter()
    .filter_map(|(s, p)| s.filter(|s| !s.trim().is_empty()).map(|s| (Prepared::new(s), p)))
    .collect();
    let mut total = 0;
    let mut indices = Vec::new();
    for t in tokens {
        let mut best: Option<(i32, Option<Vec<usize>>)> = None;
        if !name.is_empty()
            && let Some(fm) = m.score(t, &name)
        {
            best = Some((fm.score, Some(fm.indices)));
        }
        for (p, pen) in &others {
            if let Some(fm) = m.score(t, p) {
                let s = fm.score - pen;
                if best.as_ref().is_none_or(|b| s > b.0) {
                    best = Some((s, None));
                }
            }
        }
        let (s, idx) = best?;
        total += s;
        if let Some(idx) = idx {
            indices.extend(idx);
        }
    }
    // Whole-query bonuses on the name ("salva" == "Salva…").
    let folded_name: String = fold(&n.name).split_whitespace().collect::<Vec<_>>().join(" ");
    let core = trim_trailing_punct(&folded_name);
    if !whole.is_empty() {
        if core == trim_trailing_punct(whole) {
            total += BONUS_EXACT;
        } else if folded_name.starts_with(whole) {
            total += BONUS_PREFIX;
        }
    }
    indices.sort_unstable();
    indices.dedup();
    Some((total, indices))
}

/// Keep only nodes that are actionable or lead to one (plus the root);
/// children lists are filtered accordingly. Used for `interactive_only`
/// subscriptions.
pub fn prune_to_interactive(nodes: Vec<DomNode>, root: u64) -> Vec<DomNode> {
    let by_id: HashMap<u64, &DomNode> = nodes.iter().map(|n| (n.id, n)).collect();
    let mut keep: HashSet<u64> = HashSet::new();
    for n in &nodes {
        if n.role.is_interactive() || n.role.accepts_text() {
            // Mark the node and its ancestor chain.
            let mut cur = Some(n.id);
            let mut guard = 0;
            while let Some(id) = cur {
                if !keep.insert(id) || guard > nodes.len() {
                    break;
                }
                guard += 1;
                cur = by_id.get(&id).and_then(|x| x.parent);
            }
        }
    }
    keep.insert(root);
    nodes
        .into_iter()
        .filter(|n| keep.contains(&n.id))
        .map(|mut n| {
            n.children.retain(|c| keep.contains(c));
            n
        })
        .collect()
}

/// Give unnamed rows / cells / list & tree items a name from their first
/// descendant text, so they are searchable ("Row" → "Invoice 2024.pdf").
pub fn derive_container_names(nodes: &mut [DomNode]) {
    let index: HashMap<u64, usize> = nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();
    let text_of = |n: &DomNode| -> Option<String> {
        let s = if n.role == DomRole::StaticText {
            n.value.as_deref().filter(|v| !v.trim().is_empty()).unwrap_or(n.name.as_str())
        } else if n.role == DomRole::TextField || n.role == DomRole::Image {
            n.name.as_str()
        } else {
            ""
        };
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    for i in 0..nodes.len() {
        let n = &nodes[i];
        if !n.name.trim().is_empty()
            || !matches!(n.role, DomRole::Row | DomRole::Cell | DomRole::ListItem | DomRole::TreeItem)
        {
            continue;
        }
        // Breadth-first over the subtree (bounded) for the first texts.
        let mut queue: std::collections::VecDeque<u64> = n.children.iter().copied().collect();
        let mut parts: Vec<String> = Vec::new();
        let mut visited = 0;
        while let Some(id) = queue.pop_front() {
            visited += 1;
            if visited > 64 || parts.len() >= 2 {
                break;
            }
            let Some(&ci) = index.get(&id) else { continue };
            let c = &nodes[ci];
            if let Some(t) = text_of(c) {
                parts.push(t);
            } else {
                queue.extend(c.children.iter().copied());
            }
        }
        if !parts.is_empty() {
            nodes[i].name = parts.join(" — ");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use aether_protocol::{DomAction, Rect};

    pub fn node(id: u64, parent: Option<u64>, role: DomRole, name: &str, children: &[u64]) -> DomNode {
        DomNode {
            id,
            parent,
            role,
            native_role: format!("{role:?}"),
            name: name.into(),
            value: None,
            description: None,
            shortcut: None,
            bounds: Rect::new(0, 0, 10, 10),
            actions: if role.is_interactive() { vec![DomAction::Press] } else { vec![] },
            enabled: true,
            focused: false,
            children: children.to_vec(),
        }
    }

    /// App(1) → MenuBar(2) → File(3) → Menu(4) → [Salva…(5) ⌘S, Salva con nome…(6), Chiudi(7) disabled]
    ///        → Window(10) → Toolbar(11) → [Perché?(12) button, "Salva" static text(13), Search(14)]
    pub fn sample() -> DomSnapshot {
        let mut nodes = vec![
            node(1, None, DomRole::Group, "TextEdit", &[2, 10]),
            node(2, Some(1), DomRole::MenuBar, "", &[3]),
            node(3, Some(2), DomRole::MenuItem, "File", &[4]),
            node(4, Some(3), DomRole::Menu, "", &[5, 6, 7]),
            node(5, Some(4), DomRole::MenuItem, "Salva…", &[]),
            node(6, Some(4), DomRole::MenuItem, "Salva con nome…", &[]),
            node(7, Some(4), DomRole::MenuItem, "Chiudi", &[]),
            node(10, Some(1), DomRole::Window, "Documento senza titolo", &[11]),
            node(11, Some(10), DomRole::Toolbar, "", &[12, 13, 14]),
            node(12, Some(11), DomRole::Button, "Perché?", &[]),
            node(13, Some(11), DomRole::StaticText, "Salva", &[]),
            node(14, Some(11), DomRole::SearchField, "", &[]),
        ];
        nodes[4].shortcut = Some("⌘S".into());
        nodes[6].enabled = false;
        nodes[11].description = Some("Cerca nel documento".into());
        DomSnapshot {
            revision: 1,
            app_name: "TextEdit".into(),
            window_title: "Documento senza titolo".into(),
            pid: 42,
            root: 1,
            nodes,
        }
    }

    #[test]
    fn lookup_and_breadcrumb() {
        let t = DomTree::from_snapshot(sample());
        assert_eq!(t.node(5).unwrap().name, "Salva…");
        assert_eq!(t.breadcrumb(5), "File");
        assert_eq!(t.breadcrumb(12), "Documento senza titolo");
        assert_eq!(t.breadcrumb(1), "");
        assert_eq!(t.ancestors(5), vec![4, 3, 2, 1]);
        assert_eq!(t.revision(), 1);
        assert_eq!(t.len(), 12);
        assert!(t.node(99).is_none());
    }

    #[test]
    fn search_ranks_interactive_exact_first() {
        let t = DomTree::from_snapshot(sample());
        let hits = t.search("salva", 10);
        let ids: Vec<u64> = hits.iter().map(|h| h.node_id).collect();
        // Exact menu item first, longer item next, static text last.
        assert_eq!(ids, vec![5, 6, 13]);
        assert_eq!(hits[0].matched_indices, vec![0, 1, 2, 3, 4]);
        assert_eq!(hits[0].breadcrumb, "File");
        assert!(hits[0].score > ACTIONABLE_BONUS);
        assert!(hits[2].score < ACTIONABLE_BONUS);
    }

    #[test]
    fn search_accents_tokens_and_fields() {
        let t = DomTree::from_snapshot(sample());
        let h = t.search("perche", 5);
        assert_eq!(h[0].node_id, 12);
        assert_eq!(h[0].matched_indices, vec![0, 1, 2, 3, 4, 5]);
        // Multi-token, out of order.
        let h = t.search("nome salva", 5);
        assert_eq!(h[0].node_id, 6);
        // Description match (search field has no name): label falls back to description.
        let h = t.search("cerca", 5);
        assert_eq!(h[0].node_id, 14);
        assert_eq!(h[0].label, "Cerca nel documento");
        assert!(h[0].matched_indices.is_empty());
        // Shortcut field.
        let h = t.search("⌘s", 5);
        assert_eq!(h[0].node_id, 5);
        // Disabled item ranks below actionable ones.
        let h = t.search("chiudi", 5);
        assert_eq!(h[0].node_id, 7);
        assert!(h[0].score < ACTIONABLE_BONUS);
        assert!(t.search("zzzz", 5).is_empty());
        assert_eq!(t.search("salva", 1).len(), 1);
    }

    #[test]
    fn empty_query_lists_actionable() {
        let t = DomTree::from_snapshot(sample());
        let ids: Vec<u64> = t.search("  ", 50).iter().map(|h| h.node_id).collect();
        assert_eq!(ids, vec![3, 5, 6, 12, 14]);
    }

    #[test]
    fn diff_revision_checks() {
        let mut t = DomTree::new();
        let d = DomTreeDiff {
            base_revision: 1,
            revision: 2,
            upserted: vec![],
            removed: vec![],
            root: None,
            app_name: None,
            window_title: None,
        };
        assert!(t.apply_diff(&d).unwrap_err().needs_resync());
        t.apply_snapshot(sample());
        let bad = DomTreeDiff { base_revision: 7, ..d.clone() };
        let e = t.apply_diff(&bad).unwrap_err();
        assert!(e.needs_resync());
        assert!(matches!(e, DomError::RevisionMismatch { expected: 1, got: 7 }));
        let rm_root = DomTreeDiff { removed: vec![1], ..d.clone() };
        assert!(t.apply_diff(&rm_root).is_err());
        assert_eq!(t.revision(), 1, "failed diff must not modify the tree");
        t.apply_diff(&d).unwrap();
        assert_eq!(t.revision(), 2);
    }

    #[test]
    fn prune_keeps_paths_to_interactive() {
        let s = sample();
        let pruned = prune_to_interactive(s.nodes, 1);
        let ids: HashSet<u64> = pruned.iter().map(|n| n.id).collect();
        assert!(!ids.contains(&13), "static text dropped");
        for id in [1, 2, 3, 4, 5, 6, 7, 10, 11, 12, 14] {
            assert!(ids.contains(&id), "{id}");
        }
        let toolbar = pruned.iter().find(|n| n.id == 11).unwrap();
        assert_eq!(toolbar.children, vec![12, 14]);
    }

    #[test]
    fn container_names_from_text() {
        let mut nodes = vec![
            node(1, None, DomRole::Table, "", &[2]),
            node(2, Some(1), DomRole::Row, "", &[3, 5]),
            node(3, Some(2), DomRole::Cell, "", &[4]),
            node(4, Some(3), DomRole::StaticText, "", &[]),
            node(5, Some(2), DomRole::Cell, "", &[6]),
            node(6, Some(5), DomRole::StaticText, "12 KB", &[]),
        ];
        nodes[3].value = Some("Fattura.pdf".into());
        derive_container_names(&mut nodes);
        assert_eq!(nodes[1].name, "Fattura.pdf — 12 KB");
        assert_eq!(nodes[2].name, "Fattura.pdf");
        assert_eq!(nodes[0].name, "", "tables are not renamed");
    }

    #[test]
    fn snapshot_roundtrip_order() {
        let s = sample();
        let t = DomTree::from_snapshot(s.clone());
        let back = t.to_snapshot();
        assert_eq!(back, s, "sample is already in pre-order");
    }
}

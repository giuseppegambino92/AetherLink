//! Id-keyed structural diff between two snapshots.

use aether_protocol::{DomNode, DomSnapshot, DomTreeDiff};
use std::collections::{HashMap, HashSet};

/// Compute the update that turns `old` into `new`.
///
/// Nodes are matched by id; a node is upserted when it is new or when any
/// property (including its children list) changed, and removed when its id
/// disappeared. `DomTreeDiff` cannot carry a pid, so when `old.pid !=
/// new.pid` (the frontmost app changed) send a full snapshot instead — see
/// [`needs_snapshot`].
pub fn diff(old: &DomSnapshot, new: &DomSnapshot) -> DomTreeDiff {
    let old_by_id: HashMap<u64, &DomNode> = old.nodes.iter().map(|n| (n.id, n)).collect();
    let new_ids: HashSet<u64> = new.nodes.iter().map(|n| n.id).collect();
    let upserted: Vec<DomNode> =
        new.nodes.iter().filter(|n| old_by_id.get(&n.id).is_none_or(|o| *o != *n)).cloned().collect();
    let mut removed: Vec<u64> = old.nodes.iter().map(|n| n.id).filter(|id| !new_ids.contains(id)).collect();
    removed.sort_unstable();
    removed.dedup();
    DomTreeDiff {
        base_revision: old.revision,
        revision: new.revision,
        upserted,
        removed,
        root: (old.root != new.root).then_some(new.root),
        app_name: (old.app_name != new.app_name).then(|| new.app_name.clone()),
        window_title: (old.window_title != new.window_title).then(|| new.window_title.clone()),
    }
}

/// Whether a diff cannot express the change (different process).
pub fn needs_snapshot(old: &DomSnapshot, new: &DomSnapshot) -> bool {
    old.pid != new.pid
}

/// True when the diff carries no change besides the revision bump.
pub fn is_noop(d: &DomTreeDiff) -> bool {
    d.upserted.is_empty() && d.removed.is_empty() && d.root.is_none() && d.app_name.is_none() && d.window_title.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::DomTree;
    use aether_protocol::{DomAction, DomRole, Rect};

    /// xorshift64* — deterministic, dependency-free.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }
        fn chance(&mut self, pct: u64) -> bool {
            self.below(100) < pct
        }
        fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
            &v[self.below(v.len() as u64) as usize]
        }
    }

    const WORDS: &[&str] = &[
        "Salva", "Apri", "Chiudi", "Perché", "Città", "Modifica", "Vista", "Finestra", "Aiuto", "Élan", "Straße",
        "Copy", "Paste", "Undo", "Redo", "Zoom", "Mostra", "Più", "Così", "naïve",
    ];
    const ROLES: &[DomRole] = &[
        DomRole::Button,
        DomRole::Group,
        DomRole::StaticText,
        DomRole::MenuItem,
        DomRole::TextField,
        DomRole::CheckBox,
        DomRole::Row,
        DomRole::Link,
    ];

    fn random_node(rng: &mut Rng, id: u64, parent: Option<u64>) -> DomNode {
        let role = *rng.pick(ROLES);
        DomNode {
            id,
            parent,
            role,
            native_role: format!("AX{role:?}"),
            name: format!("{} {}", rng.pick(WORDS), rng.below(10)),
            value: rng.chance(30).then(|| rng.pick(WORDS).to_string()),
            description: rng.chance(20).then(|| rng.pick(WORDS).to_string()),
            shortcut: rng.chance(10).then(|| format!("⌘{}", (b'A' + rng.below(26) as u8) as char)),
            bounds: Rect::new(rng.below(2000) as i32, rng.below(1000) as i32, rng.below(300) as u32, 20),
            actions: if rng.chance(50) { vec![DomAction::Press] } else { vec![] },
            enabled: rng.chance(85),
            focused: rng.chance(5),
            children: vec![],
        }
    }

    /// Random tree with consistent parent/children links, nodes in BFS order.
    fn random_tree(rng: &mut Rng, size: usize, next_id: &mut u64) -> DomSnapshot {
        let mut nodes: Vec<DomNode> = Vec::new();
        let root = *next_id;
        *next_id += 1;
        nodes.push(random_node(rng, root, None));
        for _ in 1..size {
            let pi = rng.below(nodes.len() as u64) as usize;
            let pid = nodes[pi].id;
            let id = *next_id;
            *next_id += 1;
            nodes[pi].children.push(id);
            nodes.push(random_node(rng, id, Some(pid)));
        }
        DomSnapshot { revision: 1, app_name: "App".into(), window_title: "Win".into(), pid: 7, root, nodes }
    }

    fn subtree(nodes: &HashMap<u64, DomNode>, id: u64, out: &mut Vec<u64>) {
        out.push(id);
        if let Some(n) = nodes.get(&id) {
            for &c in &n.children {
                subtree(nodes, c, out);
            }
        }
    }

    /// Apply random edits: property changes, subtree removal, insertion, re-parenting.
    fn mutate(rng: &mut Rng, s: &DomSnapshot, next_id: &mut u64) -> DomSnapshot {
        let mut map: HashMap<u64, DomNode> = s.nodes.iter().map(|n| (n.id, n.clone())).collect();
        let mut root = s.root;
        let edits = rng.below(12);
        for _ in 0..edits {
            let ids: Vec<u64> = map.keys().copied().collect();
            let id = *rng.pick(&ids);
            match rng.below(9) {
                0 => map.get_mut(&id).unwrap().name = format!("{} {}", rng.pick(WORDS), rng.below(100)),
                1 => map.get_mut(&id).unwrap().value = rng.chance(50).then(|| rng.pick(WORDS).to_string()),
                2 => {
                    let n = map.get_mut(&id).unwrap();
                    n.enabled = !n.enabled;
                }
                3 => map.get_mut(&id).unwrap().bounds.x += 1,
                4 if id != root => {
                    // remove subtree
                    let mut gone = Vec::new();
                    subtree(&map, id, &mut gone);
                    let parent = map[&id].parent;
                    for g in gone {
                        map.remove(&g);
                    }
                    if let Some(p) = parent.and_then(|p| map.get_mut(&p)) {
                        p.children.retain(|c| *c != id);
                    }
                }
                5 | 6 => {
                    // insert a child
                    let nid = *next_id;
                    *next_id += 1;
                    map.get_mut(&id).unwrap().children.push(nid);
                    map.insert(nid, random_node(rng, nid, Some(id)));
                }
                7 if id != root => {
                    // re-parent under a node outside its own subtree
                    let mut sub = Vec::new();
                    subtree(&map, id, &mut sub);
                    let candidates: Vec<u64> = map.keys().copied().filter(|k| !sub.contains(k)).collect();
                    if candidates.is_empty() {
                        continue;
                    }
                    let np = *rng.pick(&candidates);
                    let old_p = map[&id].parent;
                    if let Some(p) = old_p.and_then(|p| map.get_mut(&p)) {
                        p.children.retain(|c| *c != id);
                    }
                    map.get_mut(&np).unwrap().children.push(id);
                    map.get_mut(&id).unwrap().parent = Some(np);
                }
                8 if rng.chance(20) => {
                    // replace the root with a new wrapper
                    let nid = *next_id;
                    *next_id += 1;
                    let mut n = random_node(rng, nid, None);
                    n.children = vec![root];
                    map.get_mut(&root).unwrap().parent = Some(nid);
                    map.insert(nid, n);
                    root = nid;
                }
                _ => {}
            }
        }
        let tree = DomTree::from_snapshot(DomSnapshot {
            revision: s.revision + 1,
            app_name: if rng.chance(10) { "Other".into() } else { s.app_name.clone() },
            window_title: if rng.chance(20) { format!("Win {}", rng.below(5)) } else { s.window_title.clone() },
            pid: s.pid,
            root,
            nodes: map.into_values().collect(),
        });
        tree.to_snapshot()
    }

    #[test]
    fn roundtrip_property_random_trees() {
        let mut rng = Rng(0x00C0_FFEE_D00D_F00D);
        let mut next_id = 1;
        for round in 0..300 {
            let size = 1 + rng.below(if round % 10 == 0 { 400 } else { 40 }) as usize;
            let mut old = random_tree(&mut rng, size, &mut next_id);
            for _step in 0..5 {
                let new = mutate(&mut rng, &old, &mut next_id);
                let d = diff(&old, &new);
                // Round trip.
                let mut t = DomTree::from_snapshot(old.clone());
                t.apply_diff(&d).unwrap_or_else(|e| panic!("round {round}: {e}"));
                assert_eq!(t, DomTree::from_snapshot(new.clone()), "round {round}");
                assert_eq!(t.to_snapshot(), new);
                // Minimality: only changed/new nodes are upserted.
                let old_map: HashMap<u64, &DomNode> = old.nodes.iter().map(|n| (n.id, n)).collect();
                for u in &d.upserted {
                    assert!(old_map.get(&u.id).is_none_or(|o| *o != u), "unchanged node {} upserted", u.id);
                }
                let up: HashSet<u64> = d.upserted.iter().map(|n| n.id).collect();
                for n in &new.nodes {
                    if !up.contains(&n.id) {
                        assert_eq!(old_map.get(&n.id).copied(), Some(n));
                    }
                }
                old = new;
            }
        }
    }

    #[test]
    fn identical_snapshots_give_noop() {
        let mut rng = Rng(42);
        let mut next = 1;
        let a = random_tree(&mut rng, 50, &mut next);
        let mut b = a.clone();
        b.revision = 2;
        let d = diff(&a, &b);
        assert!(is_noop(&d));
        assert_eq!((d.base_revision, d.revision), (1, 2));
        assert!(!needs_snapshot(&a, &b));
        b.pid = 8;
        assert!(needs_snapshot(&a, &b));
    }

    #[test]
    fn property_change_only_upserts_that_node() {
        let s = crate::tree::tests::sample();
        let mut n = s.clone();
        n.revision = 2;
        n.nodes[4].enabled = false;
        let d = diff(&s, &n);
        assert_eq!(d.upserted.len(), 1);
        assert_eq!(d.upserted[0].id, 5);
        assert!(d.removed.is_empty() && d.root.is_none() && d.app_name.is_none());
    }
}

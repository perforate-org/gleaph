//! Minimal Darts-compatible double-array builder for the synthetic test fixture.
//!
//! Encoding derived from darts.h (taku910/mecab) and verified byte-for-byte against the
//! vendored `DoubleArrayTrie` walker:
//! - edge code = key byte + 1;
//! - a node's children occupy slots `base + code`, each slot's `check = base`;
//! - a terminating key contributes a code-0 child slot at index `base` with
//!   `base = -(value+1)` (the leaf/value marker the search's final check reads);
//! - `array[0] = { base: root_begin, check: 0 }`.

#[derive(Default, Clone)]
struct Node {
    children: Vec<(u32, Node)>, // (code = byte + 1; code 0 = terminal marker) sorted
    value: Option<u32>,
}

fn insert_keys(keys: &[(Vec<u8>, u32)]) -> Node {
    let mut root = Node::default();
    for (key, value) in keys {
        let mut node = &mut root;
        for &b in key {
            let code = b as u32 + 1;
            let idx = node.children.binary_search_by(|(c, _)| c.cmp(&code));
            let idx = match idx {
                Ok(i) => i,
                Err(i) => {
                    node.children.insert(i, (code, Node::default()));
                    i
                }
            };
            node = &mut node.children[idx].1;
        }
        // Terminal marker: a code-0 child carries the value (the Darts leaf convention —
        // verified against real ipadic: the search's final check reads slot[base] with
        // check == base and base = -(value+1)).
        assert!(
            node.children.iter().all(|(c, _)| *c != 0),
            "duplicate key in fixture"
        );
        node.children.push((0, Node { value: Some(*value), ..Default::default() }));
    }
    root
}

struct Builder {
    units: Vec<(i32, u32)>, // (base, check)
    used: Vec<bool>,
}

impl Builder {
    fn new() -> Self {
        Self {
            units: vec![(0, 0); 64],
            used: vec![false; 4096],
        }
    }

    fn ensure(&mut self, idx: usize) {
        while self.units.len() <= idx {
            self.units.push((0, 0));
        }
        while self.used.len() <= idx {
            self.used.push(false);
        }
    }

    fn free(&self, idx: usize) -> bool {
        idx < self.units.len() && self.units[idx].1 == 0
    }

    fn insert(&mut self, node: &Node) -> usize {
        let siblings: Vec<(u32, &Node)> = node.children.iter().map(|(c, n)| (*c, n)).collect();
        if siblings.is_empty() {
            return 0;
        }
        let first = siblings[0].0 as usize;
        let last = siblings[siblings.len() - 1].0 as usize;
        let mut begin = first.saturating_sub(1);
        loop {
            begin += 1;
            self.ensure(begin + last);
            if self.used.get(begin).copied().unwrap_or(false) {
                continue;
            }
            let mut ok = true;
            for (code, _) in &siblings {
                if !self.free(begin + *code as usize) {
                    ok = false;
                    break;
                }
            }
            if ok {
                break;
            }
        }
        self.ensure(begin + last);
        self.used[begin] = true;
        // Darts writes ALL check fields first, then bases/recursion (interleaving lets
        // a recursive insert allocate over a not-yet-written sibling slot).
        for (code, _) in &siblings {
            let slot = begin + *code as usize;
            self.ensure(slot);
            self.units[slot].1 = begin as u32;
        }
        for (code, child) in &siblings {
            let slot = begin + *code as usize;
            if child.children.is_empty() {
                // Terminal code-0 child: the value marker (value 0 encodes as base = -1).
                let value = child.value.unwrap_or(0);
                self.units[slot].0 = -(value as i32) - 1;
            } else {
                let h = self.insert(child);
                self.units[slot].0 = h as i32;
            }
        }
        begin
    }
}

/// Builds the Darts double-array trie for `keys` (must be sorted byte-wise, unique) in
/// the MeCab sys.dic layout (LE i32 base + u32 check, 8 bytes per unit).
pub fn build_double_array(keys: &[(Vec<u8>, u32)]) -> Vec<u8> {
    let root = insert_keys(keys);
    let mut builder = Builder::new();
    let begin = builder.insert(&root);
    builder.units[0].0 = begin as i32;
    let mut out = Vec::with_capacity(builder.units.len() * 8);
    for (base, check) in &builder.units {
        out.extend_from_slice(&base.to_le_bytes());
        out.extend_from_slice(&check.to_le_bytes());
    }
    out
}

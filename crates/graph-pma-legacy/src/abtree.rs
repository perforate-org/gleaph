use crate::layout::{LayoutError, LayoutResult};
use crate::prop_codec::{read_bytes, read_u32, read_u8, write_bytes, write_u32, write_u8};

const MAX_KEYS_PER_NODE: usize = 8;
const MIN_KEYS_PER_NODE: usize = MAX_KEYS_PER_NODE / 2;
const MIN_LEAF_ENTRIES: usize = MIN_KEYS_PER_NODE;
const MIN_INTERNAL_CHILDREN: usize = MIN_KEYS_PER_NODE + 1;

type NodeId = usize;
type Entry = (Vec<u8>, Vec<u8>);

#[derive(Clone, Debug)]
enum Node {
    Leaf { entries: Vec<Entry> },
    Internal { keys: Vec<Vec<u8>>, children: Vec<NodeId> },
}

impl Default for Node {
    fn default() -> Self {
        Self::Leaf { entries: Vec::new() }
    }
}

#[derive(Clone, Debug)]
pub struct AbTree {
    root: NodeId,
    nodes: Vec<Node>,
}

impl Default for AbTree {
    fn default() -> Self {
        Self {
            root: 0,
            nodes: vec![Node::default()],
        }
    }
}

impl AbTree {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_u32(&mut out, self.root as u32);
        write_u32(&mut out, self.nodes.len() as u32);
        for node in &self.nodes {
            encode_node(node, &mut out);
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> LayoutResult<Self> {
        let mut cursor = 0;
        let root = read_u32(bytes, &mut cursor)? as usize;
        let len = read_u32(bytes, &mut cursor)? as usize;
        let mut nodes = Vec::with_capacity(len);
        for _ in 0..len {
            nodes.push(decode_node(bytes, &mut cursor)?);
        }
        if nodes.is_empty() || root >= nodes.len() {
            return Err(LayoutError::InvalidPayload);
        }
        Ok(Self { root, nodes })
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.get_in_node(self.root, key)
    }

    pub fn insert(&mut self, key: Vec<u8>, value: Vec<u8>) -> Option<Vec<u8>> {
        let outcome = self.insert_into_node(self.root, key, value);
        if let Some((separator, right_child)) = outcome.split {
            let old_root = self.root;
            let new_root = self.alloc_node(Node::Internal {
                keys: vec![separator],
                children: vec![old_root, right_child],
            });
            self.root = new_root;
        }
        outcome.old_value
    }

    pub fn remove(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let outcome = self.remove_from_node(self.root, key, true);
        if let Node::Internal { children, .. } = &self.nodes[self.root]
            && children.len() == 1
        {
            self.root = children[0];
        }
        outcome.old_value
    }

    pub fn scan_prefix(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut entries = Vec::new();
        self.collect_entries(self.root, &mut entries);
        entries
            .into_iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .collect()
    }

    fn alloc_node(&mut self, node: Node) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(node);
        id
    }

    fn get_in_node<'a>(&'a self, node_id: NodeId, key: &[u8]) -> Option<&'a [u8]> {
        match &self.nodes[node_id] {
            Node::Leaf { entries } => entries
                .binary_search_by(|(existing, _)| existing.as_slice().cmp(key))
                .ok()
                .map(|index| entries[index].1.as_slice()),
            Node::Internal { keys, children } => {
                let child_index = child_index(keys, key);
                self.get_in_node(children[child_index], key)
            }
        }
    }

    fn insert_into_node(&mut self, node_id: NodeId, key: Vec<u8>, value: Vec<u8>) -> InsertOutcome {
        match &mut self.nodes[node_id] {
            Node::Leaf { entries } => {
                match entries.binary_search_by(|(existing, _)| existing.cmp(&key)) {
                    Ok(index) => {
                        let old_value = std::mem::replace(&mut entries[index].1, value);
                        return InsertOutcome::replaced(Some(old_value));
                    }
                    Err(index) => entries.insert(index, (key, value)),
                }

                if entries.len() <= MAX_KEYS_PER_NODE {
                    return InsertOutcome::default();
                }

                let split_at = entries.len() / 2;
                let right_entries = entries.split_off(split_at);
                let separator = right_entries
                    .first()
                    .map(|(key, _)| key.clone())
                    .expect("split leaf must have right entries");
                let right_child = self.alloc_node(Node::Leaf {
                    entries: right_entries,
                });
                return InsertOutcome::split(separator, right_child);
            }
            Node::Internal { .. } => {}
        }

        let (child_index, child_id) = match &self.nodes[node_id] {
            Node::Internal { keys, children } => {
                let child_index = child_index(keys, &key);
                (child_index, children[child_index])
            }
            Node::Leaf { .. } => unreachable!("handled above"),
        };

        let outcome = self.insert_into_node(child_id, key, value);

        let split = {
            let Node::Internal { keys, children } = &mut self.nodes[node_id] else {
                unreachable!("node kind changed")
            };

            if let Some(first_key) = outcome.first_key_change.clone()
                && child_index > 0
            {
                keys[child_index - 1] = first_key;
            }

            if let Some((separator, right_child)) = outcome.split.clone() {
                keys.insert(child_index, separator);
                children.insert(child_index + 1, right_child);
            }

            if keys.len() <= MAX_KEYS_PER_NODE {
                None
            } else {
                let mid = keys.len() / 2;
                let separator = keys[mid].clone();
                let right_keys = keys.split_off(mid + 1);
                keys.pop();
                let right_children = children.split_off(mid + 1);
                Some((separator, right_keys, right_children))
            }
        };

        if let Some((separator, right_keys, right_children)) = split {
            let right_child = self.alloc_node(Node::Internal {
                keys: right_keys,
                children: right_children,
            });
            InsertOutcome {
                old_value: outcome.old_value,
                split: Some((separator, right_child)),
                first_key_change: None,
            }
        } else {
            InsertOutcome {
                old_value: outcome.old_value,
                split: None,
                first_key_change: None,
            }
        }
    }

    fn remove_from_node(&mut self, node_id: NodeId, key: &[u8], is_root: bool) -> RemoveOutcome {
        match &mut self.nodes[node_id] {
            Node::Leaf { entries } => {
                let Ok(index) = entries.binary_search_by(|(existing, _)| existing.as_slice().cmp(key)) else {
                    return RemoveOutcome::default();
                };
                let (_, old_value) = entries.remove(index);
                return RemoveOutcome {
                    old_value: Some(old_value),
                    underflow: !is_root && entries.len() < MIN_LEAF_ENTRIES,
                };
            }
            Node::Internal { .. } => {}
        }

        let (child_index, child_id) = match &self.nodes[node_id] {
            Node::Internal { keys, children } => {
                let child_index = child_index(keys, key);
                (child_index, children[child_index])
            }
            Node::Leaf { .. } => unreachable!("handled above"),
        };

        let outcome = self.remove_from_node(child_id, key, false);
        if outcome.old_value.is_none() {
            return outcome;
        }

        if outcome.underflow {
            self.rebalance_child(node_id, child_index);
        }
        self.recompute_internal_keys(node_id);
        let underflow = !is_root && self.child_count(node_id) < MIN_INTERNAL_CHILDREN;

        RemoveOutcome {
            old_value: outcome.old_value,
            underflow,
        }
    }

    fn first_key(&self, node_id: NodeId) -> Option<Vec<u8>> {
        match &self.nodes[node_id] {
            Node::Leaf { entries } => entries.first().map(|(key, _)| key.clone()),
            Node::Internal { children, .. } => children
                .first()
                .and_then(|child_id| self.first_key(*child_id)),
        }
    }

    fn collect_entries(&self, node_id: NodeId, out: &mut Vec<Entry>) {
        match &self.nodes[node_id] {
            Node::Leaf { entries } => out.extend(entries.iter().cloned()),
            Node::Internal { children, .. } => {
                for child_id in children {
                    self.collect_entries(*child_id, out);
                }
            }
        }
    }

    fn child_count(&self, node_id: NodeId) -> usize {
        match &self.nodes[node_id] {
            Node::Leaf { entries } => entries.len(),
            Node::Internal { children, .. } => children.len(),
        }
    }

    fn can_lend(&self, node_id: NodeId) -> bool {
        match &self.nodes[node_id] {
            Node::Leaf { entries } => entries.len() > MIN_LEAF_ENTRIES,
            Node::Internal { children, .. } => children.len() > MIN_INTERNAL_CHILDREN,
        }
    }

    fn rebalance_child(&mut self, parent_id: NodeId, child_index: usize) {
        let (left_id, child_id, right_id) = match &self.nodes[parent_id] {
            Node::Internal { children, .. } => (
                child_index.checked_sub(1).map(|index| children[index]),
                children[child_index],
                children.get(child_index + 1).copied(),
            ),
            Node::Leaf { .. } => return,
        };

        if let Some(left_id) = left_id
            && self.can_lend(left_id)
        {
            self.borrow_from_left(left_id, child_id);
            self.recompute_internal_keys(parent_id);
            return;
        }

        if let Some(right_id) = right_id
            && self.can_lend(right_id)
        {
            self.borrow_from_right(child_id, right_id);
            self.recompute_internal_keys(parent_id);
            return;
        }

        if left_id.is_some() {
            self.merge_children(parent_id, child_index - 1, child_index);
        } else if right_id.is_some() {
            self.merge_children(parent_id, child_index, child_index + 1);
        }
        self.recompute_internal_keys(parent_id);
    }

    fn borrow_from_left(&mut self, left_id: NodeId, target_id: NodeId) {
        let left = std::mem::take(&mut self.nodes[left_id]);
        let target = std::mem::take(&mut self.nodes[target_id]);
        match (left, target) {
            (
                Node::Leaf { entries: mut left_entries },
                Node::Leaf { entries: mut target_entries },
            ) => {
                let borrowed = left_entries.pop().expect("left sibling must have entry");
                target_entries.insert(0, borrowed);
                self.nodes[left_id] = Node::Leaf { entries: left_entries };
                self.nodes[target_id] = Node::Leaf {
                    entries: target_entries,
                };
            }
            (
                Node::Internal { children: mut left_children, .. },
                Node::Internal { children: mut target_children, .. },
            ) => {
                let borrowed = left_children.pop().expect("left sibling must have child");
                target_children.insert(0, borrowed);
                self.nodes[left_id] = Node::Internal {
                    keys: Vec::new(),
                    children: left_children,
                };
                self.nodes[target_id] = Node::Internal {
                    keys: Vec::new(),
                    children: target_children,
                };
                self.recompute_internal_keys(left_id);
                self.recompute_internal_keys(target_id);
            }
            (left, target) => {
                self.nodes[left_id] = left;
                self.nodes[target_id] = target;
                panic!("abtree sibling kind mismatch");
            }
        }
    }

    fn borrow_from_right(&mut self, target_id: NodeId, right_id: NodeId) {
        let target = std::mem::take(&mut self.nodes[target_id]);
        let right = std::mem::take(&mut self.nodes[right_id]);
        match (target, right) {
            (
                Node::Leaf { entries: mut target_entries },
                Node::Leaf { entries: mut right_entries },
            ) => {
                let borrowed = right_entries.remove(0);
                target_entries.push(borrowed);
                self.nodes[target_id] = Node::Leaf {
                    entries: target_entries,
                };
                self.nodes[right_id] = Node::Leaf { entries: right_entries };
            }
            (
                Node::Internal { children: mut target_children, .. },
                Node::Internal { children: mut right_children, .. },
            ) => {
                let borrowed = right_children.remove(0);
                target_children.push(borrowed);
                self.nodes[target_id] = Node::Internal {
                    keys: Vec::new(),
                    children: target_children,
                };
                self.nodes[right_id] = Node::Internal {
                    keys: Vec::new(),
                    children: right_children,
                };
                self.recompute_internal_keys(target_id);
                self.recompute_internal_keys(right_id);
            }
            (target, right) => {
                self.nodes[target_id] = target;
                self.nodes[right_id] = right;
                panic!("abtree sibling kind mismatch");
            }
        }
    }

    fn merge_children(&mut self, parent_id: NodeId, left_index: usize, right_index: usize) {
        let (left_id, right_id) = match &self.nodes[parent_id] {
            Node::Internal { children, .. } => (children[left_index], children[right_index]),
            Node::Leaf { .. } => return,
        };

        let left = std::mem::take(&mut self.nodes[left_id]);
        let right = std::mem::take(&mut self.nodes[right_id]);
        match (left, right) {
            (
                Node::Leaf { entries: mut left_entries },
                Node::Leaf { entries: right_entries },
            ) => {
                left_entries.extend(right_entries);
                self.nodes[left_id] = Node::Leaf {
                    entries: left_entries,
                };
                self.nodes[right_id] = Node::default();
            }
            (
                Node::Internal { children: mut left_children, .. },
                Node::Internal { children: right_children, .. },
            ) => {
                left_children.extend(right_children);
                self.nodes[left_id] = Node::Internal {
                    keys: Vec::new(),
                    children: left_children,
                };
                self.nodes[right_id] = Node::default();
                self.recompute_internal_keys(left_id);
            }
            (left, right) => {
                self.nodes[left_id] = left;
                self.nodes[right_id] = right;
                panic!("abtree sibling kind mismatch");
            }
        }

        if let Node::Internal { children, .. } = &mut self.nodes[parent_id] {
            children.remove(right_index);
        }
    }

    fn recompute_internal_keys(&mut self, node_id: NodeId) {
        let children = match &self.nodes[node_id] {
            Node::Internal { children, .. } => children.clone(),
            Node::Leaf { .. } => return,
        };
        let keys = children
            .iter()
            .skip(1)
            .filter_map(|child_id| self.first_key(*child_id))
            .collect();
        if let Node::Internal { keys: node_keys, .. } = &mut self.nodes[node_id] {
            *node_keys = keys;
        }
    }
}

#[derive(Default)]
struct InsertOutcome {
    old_value: Option<Vec<u8>>,
    split: Option<(Vec<u8>, NodeId)>,
    first_key_change: Option<Vec<u8>>,
}

impl InsertOutcome {
    fn replaced(old_value: Option<Vec<u8>>) -> Self {
        Self {
            old_value,
            split: None,
            first_key_change: None,
        }
    }

    fn split(separator: Vec<u8>, right_child: NodeId) -> Self {
        Self {
            old_value: None,
            split: Some((separator, right_child)),
            first_key_change: None,
        }
    }
}

#[derive(Default)]
struct RemoveOutcome {
    old_value: Option<Vec<u8>>,
    underflow: bool,
}

fn child_index(keys: &[Vec<u8>], key: &[u8]) -> usize {
    keys.partition_point(|separator| separator.as_slice() <= key)
}

fn encode_node(node: &Node, out: &mut Vec<u8>) {
    match node {
        Node::Leaf { entries } => {
            write_u8(out, 0);
            write_u32(out, entries.len() as u32);
            for (key, value) in entries {
                write_bytes(out, key);
                write_bytes(out, value);
            }
        }
        Node::Internal { keys, children } => {
            write_u8(out, 1);
            write_u32(out, keys.len() as u32);
            for key in keys {
                write_bytes(out, key);
            }
            write_u32(out, children.len() as u32);
            for child in children {
                write_u32(out, *child as u32);
            }
        }
    }
}

fn decode_node(bytes: &[u8], cursor: &mut usize) -> LayoutResult<Node> {
    match read_u8(bytes, cursor)? {
        0 => {
            let len = read_u32(bytes, cursor)? as usize;
            let mut entries = Vec::with_capacity(len);
            for _ in 0..len {
                entries.push((read_bytes(bytes, cursor)?, read_bytes(bytes, cursor)?));
            }
            Ok(Node::Leaf { entries })
        }
        1 => {
            let key_len = read_u32(bytes, cursor)? as usize;
            let mut keys = Vec::with_capacity(key_len);
            for _ in 0..key_len {
                keys.push(read_bytes(bytes, cursor)?);
            }
            let child_len = read_u32(bytes, cursor)? as usize;
            let mut children = Vec::with_capacity(child_len);
            for _ in 0..child_len {
                children.push(read_u32(bytes, cursor)? as usize);
            }
            if child_len != key_len + 1 && child_len != 0 {
                return Err(LayoutError::InvalidPayload);
            }
            Ok(Node::Internal { keys, children })
        }
        _ => Err(LayoutError::InvalidPayload),
    }
}

#[cfg(test)]
mod tests {
    use super::AbTree;

    #[test]
    fn inserts_and_reads_across_splits() {
        let mut tree = AbTree::default();
        for index in 0..32u8 {
            tree.insert(vec![index], vec![index + 1]);
        }
        for index in 0..32u8 {
            assert_eq!(tree.get(&[index]), Some(&[index + 1][..]));
        }
    }

    #[test]
    fn scan_prefix_returns_sorted_matching_entries() {
        let mut tree = AbTree::default();
        tree.insert(vec![1, 0], vec![10]);
        tree.insert(vec![1, 1], vec![11]);
        tree.insert(vec![2, 0], vec![20]);
        let entries = tree.scan_prefix(&[1]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, vec![1, 0]);
        assert_eq!(entries[1].0, vec![1, 1]);
    }

    #[test]
    fn remove_deletes_existing_key() {
        let mut tree = AbTree::default();
        tree.insert(vec![1], vec![10]);
        tree.insert(vec![2], vec![20]);
        assert_eq!(tree.remove(&[1]), Some(vec![10]));
        assert_eq!(tree.get(&[1]), None);
        assert_eq!(tree.get(&[2]), Some(&[20][..]));
    }

    #[test]
    fn repeated_removals_preserve_remaining_entries() {
        let mut tree = AbTree::default();
        for index in 0..32u8 {
            tree.insert(vec![index], vec![index + 1]);
        }
        for index in [3u8, 4, 5, 6, 7, 8, 15, 16, 17, 24] {
            assert_eq!(tree.remove(&[index]), Some(vec![index + 1]));
        }
        for index in 0..32u8 {
            if [3u8, 4, 5, 6, 7, 8, 15, 16, 17, 24].contains(&index) {
                assert_eq!(tree.get(&[index]), None);
            } else {
                assert_eq!(tree.get(&[index]), Some(&[index + 1][..]));
            }
        }
    }

    #[test]
    fn snapshot_round_trips_tree_structure() {
        let mut tree = AbTree::default();
        for index in 0..24u8 {
            tree.insert(vec![index], vec![index + 1]);
        }
        tree.remove(&[3]);
        tree.remove(&[17]);

        let restored = AbTree::from_bytes(&tree.to_bytes()).expect("decode tree");
        for index in 0..24u8 {
            match index {
                3 | 17 => assert_eq!(restored.get(&[index]), None),
                _ => assert_eq!(restored.get(&[index]), Some(&[index + 1][..])),
            }
        }
        let entries = restored.scan_prefix(&[1]);
        assert!(entries.iter().all(|(key, _)| key.starts_with(&[1])));
    }
}

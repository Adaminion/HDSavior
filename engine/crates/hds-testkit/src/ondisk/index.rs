//! Directory index structures: index entries, `$INDEX_ROOT`, `INDX` blocks
//! and the B-tree that distributes entries over them (notes section 9).

use super::{align8, apply_fixups, put_u16, put_u32, put_u64, FileRef, FIXUP_STRIDE};

/// Index entry flag: the entry has a sub-node VCN in its last 8 bytes.
pub const ENTRY_SUBNODE: u16 = 0x1;
/// Index entry flag: last entry of the node (carries no key).
pub const ENTRY_LAST: u16 = 0x2;

/// One index entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    /// Reference to the indexed file (its base record). Zero in end entries.
    pub child: FileRef,
    /// The key: a `$FILE_NAME` value in a directory index. Empty in end entries.
    pub key: Vec<u8>,
    /// VCN of the sub-node holding the entries that sort before this one.
    pub subnode: Option<u64>,
    /// End entry of a node.
    pub last: bool,
}

impl IndexEntry {
    /// An ordinary leaf entry.
    pub fn new(child: FileRef, key: Vec<u8>) -> Self {
        IndexEntry { child, key, subnode: None, last: false }
    }

    /// The end entry of a node, optionally pointing at a sub-node.
    pub fn end(subnode: Option<u64>) -> Self {
        IndexEntry { child: FileRef::NONE, key: Vec::new(), subnode, last: true }
    }

    /// Encoded length: header, key padded to 8, sub-node VCN if any.
    pub fn len(&self) -> usize {
        align8(0x10 + self.key.len()) + if self.subnode.is_some() { 8 } else { 0 }
    }

    /// Never true: even an end entry has a 16-byte header.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The entry as stored.
    pub fn encode(&self) -> Vec<u8> {
        let length = self.len();
        let mut b = vec![0u8; length];
        put_u64(&mut b, 0x00, self.child.raw());
        put_u16(&mut b, 0x08, length as u16);
        put_u16(&mut b, 0x0A, self.key.len() as u16);
        let mut flags = 0;
        if self.subnode.is_some() {
            flags |= ENTRY_SUBNODE;
        }
        if self.last {
            flags |= ENTRY_LAST;
        }
        put_u16(&mut b, 0x0C, flags);
        b[0x10..0x10 + self.key.len()].copy_from_slice(&self.key);
        if let Some(vcn) = self.subnode {
            put_u64(&mut b, length - 8, vcn);
        }
        b
    }
}

fn encode_entries(entries: &[IndexEntry]) -> Vec<u8> {
    entries.iter().flat_map(|e| e.encode()).collect()
}

fn entries_len(entries: &[IndexEntry]) -> usize {
    entries.iter().map(IndexEntry::len).sum()
}

/// Number of index-block VCNs one block occupies: clusters when the cluster
/// is not larger than the block, otherwise 512-byte units (notes section 9).
/// Block `i` of an index therefore has VCN `i * vcn_step` and lies at byte
/// `i * index_block_size` of the `$INDEX_ALLOCATION` stream.
pub fn vcn_step(index_block_size: u32, cluster_size: u32) -> u64 {
    if cluster_size <= index_block_size {
        (index_block_size / cluster_size) as u64
    } else {
        (index_block_size / 512) as u64
    }
}

/// `$INDEX_ROOT` value: 16-byte root header, 16-byte node header, entries.
/// `indexed_type` is 0x30 and `collation` 1 for a directory.
pub fn index_root_value(
    indexed_type: u32,
    collation: u32,
    index_block_size: u32,
    cluster_size: u32,
    entries: &[IndexEntry],
) -> Vec<u8> {
    let body = encode_entries(entries);
    let mut b = vec![0u8; 0x20 + body.len()];
    put_u32(&mut b, 0x00, indexed_type);
    put_u32(&mut b, 0x04, collation);
    put_u32(&mut b, 0x08, index_block_size);
    b[0x0C] = vcn_step(index_block_size, cluster_size) as u8;
    put_u32(&mut b, 0x10, 0x10);
    put_u32(&mut b, 0x14, 0x10 + body.len() as u32);
    put_u32(&mut b, 0x18, 0x10 + body.len() as u32);
    b[0x1C] = u8::from(entries.iter().any(|e| e.subnode.is_some()));
    b[0x20..].copy_from_slice(&body);
    b
}

/// Offset of the first entry of an `INDX` block, from the block start.
pub fn indx_entries_offset(index_block_size: u32) -> usize {
    align8(0x28 + 2 * (index_block_size as usize / FIXUP_STRIDE + 1))
}

/// Bytes available for entries (end entry included) in one `INDX` block.
pub fn indx_capacity(index_block_size: u32) -> usize {
    index_block_size as usize - indx_entries_offset(index_block_size)
}

/// One `INDX` block, fixups applied. Panics if the entries do not fit.
pub fn indx_block(index_block_size: u32, vcn: u64, lsn: u64, usn: u16, entries: &[IndexEntry]) -> Vec<u8> {
    let size = index_block_size as usize;
    let first = indx_entries_offset(index_block_size);
    let body = encode_entries(entries);
    assert!(first + body.len() <= size, "index entries ({} bytes) do not fit the block", body.len());
    let mut b = vec![0u8; size];
    b[0..4].copy_from_slice(b"INDX");
    put_u16(&mut b, 0x04, 0x28);
    put_u16(&mut b, 0x06, (size / FIXUP_STRIDE + 1) as u16);
    put_u64(&mut b, 0x08, lsn);
    put_u64(&mut b, 0x10, vcn);
    put_u32(&mut b, 0x18, (first - 0x18) as u32);
    put_u32(&mut b, 0x1C, (first - 0x18 + body.len()) as u32);
    put_u32(&mut b, 0x20, (size - 0x18) as u32);
    b[0x24] = u8::from(entries.iter().any(|e| e.subnode.is_some()));
    b[first..first + body.len()].copy_from_slice(&body);
    apply_fixups(&mut b, 0x28, usn);
    b
}

/// An index laid out as a B-tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    /// Entries of the `$INDEX_ROOT`, end entry included.
    pub root: Vec<IndexEntry>,
    /// Entries of each `INDX` block, end entry included. Block `i` has VCN
    /// `i * vcn_step`.
    pub blocks: Vec<Vec<IndexEntry>>,
}

impl Tree {
    /// Indexes of the blocks that are leaves (hold no sub-node pointers).
    pub fn leaf_blocks(&self) -> impl Iterator<Item = usize> + '_ {
        self.blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| b.iter().all(|e| e.subnode.is_none()))
            .map(|(i, _)| i)
    }
}

/// Distributes `entries` (sorted, without end entry and sub-node pointers)
/// over an `$INDEX_ROOT` with room for `root_capacity` bytes of entries and
/// `INDX` blocks with room for `block_capacity` bytes each.
///
/// Small indexes stay in the root. Otherwise the leaves are filled in order
/// and the entry between two leaves moves up as their separator: it is
/// stored in the parent node only, exactly as NTFS keeps it (notes
/// section 9: "a large directory's `$INDEX_ROOT` still holds separator
/// entries that are in no `INDX` block"). Further levels of index nodes are
/// added while the separators do not fit the root; a root too small for
/// even one separator ends up with just the pointer to a single index node.
///
/// `fill_percent` (1..=100) limits how full a block is packed; real
/// directories that grew by splitting are between half and completely full.
pub fn build_tree(
    entries: Vec<IndexEntry>,
    root_capacity: usize,
    block_capacity: usize,
    vcn_step: u64,
    fill_percent: u32,
    force_blocks: bool,
) -> Tree {
    assert!((1..=100).contains(&fill_percent), "fill_percent out of range");
    const END: usize = 0x10;
    const END_WITH_SUBNODE: usize = 0x18;
    if !force_blocks && entries_len(&entries) + END <= root_capacity {
        let mut root = entries;
        root.push(IndexEntry::end(None));
        return Tree { root, blocks: Vec::new() };
    }
    let largest = entries.iter().map(IndexEntry::len).max().unwrap_or(0) + 8;
    assert!(
        2 * largest + END_WITH_SUBNODE <= block_capacity,
        "index entries of {largest} bytes are too large for blocks of {block_capacity}"
    );
    assert!(END_WITH_SUBNODE <= root_capacity, "the index root has no room for its end entry");
    // A block is closed when the next entry would push it over this limit,
    // but it always accepts at least two entries.
    let limit = (block_capacity * fill_percent as usize / 100).max(2 * largest + END_WITH_SUBNODE);

    let mut blocks: Vec<Vec<IndexEntry>> = Vec::new();

    // Leaves.
    let mut children: Vec<usize> = Vec::new();
    let mut separators: Vec<IndexEntry> = Vec::new();
    let mut current: Vec<IndexEntry> = Vec::new();
    let mut current_len = END;
    let count = entries.len();
    for (i, entry) in entries.into_iter().enumerate() {
        if current.is_empty() || current_len + entry.len() <= limit {
            current_len += entry.len();
            current.push(entry);
            continue;
        }
        let is_final = i + 1 == count;
        // The entry that does not fit becomes the separator, unless nothing
        // would be left for the next leaf: then the last entry that did fit
        // moves up and this one starts the final leaf.
        let (separator, next) = if is_final {
            (current.pop().expect("a closed leaf holds at least two entries"), Some(entry))
        } else {
            (entry, None)
        };
        current.push(IndexEntry::end(None));
        children.push(blocks.len());
        blocks.push(std::mem::take(&mut current));
        separators.push(separator);
        current_len = END;
        if let Some(next) = next {
            current_len += next.len();
            current.push(next);
        }
    }
    current.push(IndexEntry::end(None));
    children.push(blocks.len());
    blocks.push(current);

    // Index nodes, level by level, until the separators fit the root.
    loop {
        let root_need: usize = separators.iter().map(|s| s.len() + 8).sum::<usize>() + END_WITH_SUBNODE;
        if root_need <= root_capacity {
            break;
        }
        let mut next_children: Vec<usize> = Vec::new();
        let mut next_separators: Vec<IndexEntry> = Vec::new();
        let mut node: Vec<IndexEntry> = Vec::new();
        let mut node_len = END_WITH_SUBNODE;
        let total = separators.len();
        for (j, mut separator) in std::mem::take(&mut separators).into_iter().enumerate() {
            let child = children[j];
            let len = separator.len() + 8;
            if node.is_empty() || node_len + len <= limit {
                separator.subnode = Some(child as u64 * vcn_step);
                node_len += len;
                node.push(separator);
                continue;
            }
            let is_final = j + 1 == total;
            if is_final {
                // Promote the last entry of the node instead; its child
                // becomes the node's end pointer.
                let mut promoted = node.pop().expect("a closed node holds at least two entries");
                let end_child = promoted.subnode.take().expect("node entries carry a child");
                node.push(IndexEntry::end(Some(end_child)));
                next_children.push(blocks.len());
                blocks.push(std::mem::take(&mut node));
                next_separators.push(promoted);
                separator.subnode = Some(child as u64 * vcn_step);
                node_len = END_WITH_SUBNODE + len;
                node.push(separator);
            } else {
                node.push(IndexEntry::end(Some(child as u64 * vcn_step)));
                next_children.push(blocks.len());
                blocks.push(std::mem::take(&mut node));
                next_separators.push(separator);
                node_len = END_WITH_SUBNODE;
            }
        }
        let last_child = *children.last().expect("there is always a last child");
        node.push(IndexEntry::end(Some(last_child as u64 * vcn_step)));
        next_children.push(blocks.len());
        blocks.push(node);
        children = next_children;
        separators = next_separators;
    }

    let mut root: Vec<IndexEntry> = Vec::with_capacity(separators.len() + 1);
    for (j, mut separator) in separators.into_iter().enumerate() {
        separator.subnode = Some(children[j] as u64 * vcn_step);
        root.push(separator);
    }
    let last_child = *children.last().expect("there is always a last child");
    root.push(IndexEntry::end(Some(last_child as u64 * vcn_step)));
    Tree { root, blocks }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([b[at], b[at + 1]])
    }
    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    fn entry(n: u32, key_len: usize) -> IndexEntry {
        let mut key = vec![0u8; key_len];
        key[..4].copy_from_slice(&n.to_be_bytes());
        IndexEntry::new(FileRef::new(1000 + n as u64, 1), key)
    }

    #[test]
    fn entry_layout() {
        let e = IndexEntry { child: FileRef::new(36, 1), key: vec![0xAA; 0x52], subnode: Some(8), last: false };
        let b = e.encode();
        // 0x10 header + 0x52 key = 0x62, padded to 0x68, plus the VCN
        assert_eq!(b.len(), 0x70);
        assert_eq!(&b[..8], &[36, 0, 0, 0, 0, 0, 1, 0]);
        assert_eq!(u16_at(&b, 8), 0x70);
        assert_eq!(u16_at(&b, 10), 0x52);
        assert_eq!(u16_at(&b, 12), ENTRY_SUBNODE);
        assert_eq!(&b[0x10..0x62], &[0xAA; 0x52][..]);
        assert_eq!(u64_at(&b, 0x68), 8);
        // end entries: 0x10 bytes, or 0x18 with a sub-node (notes section 9)
        let end = IndexEntry::end(None).encode();
        assert_eq!(end.len(), 0x10);
        assert_eq!(u16_at(&end, 12), ENTRY_LAST);
        assert_eq!(u16_at(&end, 10), 0);
        let end = IndexEntry::end(Some(3)).encode();
        assert_eq!(end.len(), 0x18);
        assert_eq!(u16_at(&end, 12), ENTRY_LAST | ENTRY_SUBNODE);
        assert_eq!(u64_at(&end, 0x10), 3);
    }

    #[test]
    fn vcn_units_for_every_cluster_size() {
        // notes section 9: blocks are numbered 0, 8, 16 for 512-byte and for
        // clusters of 8 KiB and more; 0, 4, 8 at 1 KiB; 0, 2, 4 at 2 KiB;
        // 0, 1, 2 at 4 KiB.
        assert_eq!(vcn_step(4096, 512), 8);
        assert_eq!(vcn_step(4096, 1024), 4);
        assert_eq!(vcn_step(4096, 2048), 2);
        assert_eq!(vcn_step(4096, 4096), 1);
        assert_eq!(vcn_step(4096, 8192), 8);
        assert_eq!(vcn_step(4096, 65536), 8);
        assert_eq!(vcn_step(4096, 2 * 1024 * 1024), 8);
    }

    #[test]
    fn index_root_header() {
        let entries = [entry(1, 0x50), IndexEntry::end(None)];
        let b = index_root_value(0x30, 1, 4096, 512, &entries);
        assert_eq!(u32_at(&b, 0), 0x30);
        assert_eq!(u32_at(&b, 4), 1);
        assert_eq!(u32_at(&b, 8), 4096);
        assert_eq!(b[0x0C], 8, "clusters per block at 512-byte clusters");
        assert_eq!(u32_at(&b, 0x10), 0x10, "first entry, relative to the node header");
        assert_eq!(u32_at(&b, 0x14), 0x10 + 0x60 + 0x10);
        assert_eq!(u32_at(&b, 0x18), u32_at(&b, 0x14), "a resident index is never larger than needed");
        assert_eq!(b[0x1C], 0, "small index");
        assert_eq!(b.len(), 0x20 + 0x60 + 0x10);
        let large = index_root_value(0x30, 1, 4096, 65536, &[IndexEntry::end(Some(0))]);
        assert_eq!(large[0x1C], 1, "large index: the end entry points at a block");
        assert_eq!(large[0x0C], 8, "512-byte units when the cluster is larger than the block");
        assert_eq!(large.len(), 0x38, "the 56 bytes of every freshly formatted root directory");
        assert_eq!(index_root_value(0x30, 1, 4096, 4096, &entries)[0x0C], 1);
    }

    #[test]
    fn indx_block_header_matches_the_measured_layout() {
        let entries = [entry(1, 0x50), entry(2, 0x52), IndexEntry::end(None)];
        let b = indx_block(4096, 16, 0x1234_5678, 9, &entries);
        assert_eq!(&b[..4], b"INDX");
        assert_eq!(u16_at(&b, 4), 0x28);
        assert_eq!(u16_at(&b, 6), 9);
        assert_eq!(u64_at(&b, 8), 0x1234_5678);
        assert_eq!(u64_at(&b, 0x10), 16);
        assert_eq!(u32_at(&b, 0x18), 0x28, "entries at 0x18 + 0x28 = 0x40");
        assert_eq!(u32_at(&b, 0x1C), 0x28 + 0x60 + 0x68 + 0x10);
        assert_eq!(u32_at(&b, 0x20), 0xFE8);
        assert_eq!(b[0x24], 0, "leaf");
        assert_eq!(&b[0x40..0x48], &[0xE9, 0x03, 0, 0, 0, 0, 1, 0], "first entry: record 1001, sequence 1");
        for block in 0..8 {
            assert_eq!(u16_at(&b, block * 512 + 510), 9);
        }
        assert_eq!(indx_capacity(4096), 4096 - 0x40);
        let node = indx_block(4096, 0, 0, 1, &[IndexEntry::end(Some(8))]);
        assert_eq!(node[0x24], 1, "index node");
    }

    /// Reads a tree back in key order by walking it the way a driver does.
    fn in_order(tree: &Tree, step: u64) -> Vec<Vec<u8>> {
        fn walk(tree: &Tree, node: &[IndexEntry], step: u64, out: &mut Vec<Vec<u8>>, depth: usize) -> usize {
            let mut leaf_depth = None;
            for e in node {
                if let Some(vcn) = e.subnode {
                    assert_eq!(vcn % step, 0);
                    let d = walk(tree, &tree.blocks[(vcn / step) as usize], step, out, depth + 1);
                    assert_eq!(*leaf_depth.get_or_insert(d), d, "all leaves at the same depth");
                } else {
                    assert_eq!(*leaf_depth.get_or_insert(depth), depth);
                }
                if !e.last {
                    out.push(e.key.clone());
                }
            }
            assert!(node.last().unwrap().last);
            assert_eq!(node.iter().filter(|e| e.last).count(), 1);
            leaf_depth.unwrap()
        }
        let mut out = Vec::new();
        walk(tree, &tree.root, step, &mut out, 0);
        out
    }

    fn check(count: u32, key_len: usize, root_capacity: usize, fill: u32, force: bool) -> Tree {
        let entries: Vec<IndexEntry> = (0..count).map(|n| entry(n, key_len)).collect();
        let keys: Vec<Vec<u8>> = entries.iter().map(|e| e.key.clone()).collect();
        let capacity = indx_capacity(4096);
        let tree = build_tree(entries, root_capacity, capacity, 8, fill, force);
        assert_eq!(in_order(&tree, 8), keys, "{count} entries: order and completeness");
        assert!(entries_len(&tree.root) <= root_capacity.max(0x18));
        for block in &tree.blocks {
            assert!(entries_len(block) <= capacity, "block overflow");
            // a node either has a child under every entry or under none
            let with = block.iter().filter(|e| e.subnode.is_some()).count();
            assert!(with == 0 || with == block.len());
        }
        // every block is referenced exactly once
        let mut refs: Vec<u64> = tree
            .root
            .iter()
            .chain(tree.blocks.iter().flatten())
            .filter_map(|e| e.subnode)
            .collect();
        refs.sort_unstable();
        let expected: Vec<u64> = (0..tree.blocks.len() as u64).map(|i| i * 8).collect();
        assert_eq!(refs, expected);
        tree
    }

    #[test]
    fn small_index_stays_in_the_root() {
        let tree = check(4, 0x5A, 700, 100, false);
        assert!(tree.blocks.is_empty());
        assert_eq!(tree.root.len(), 5);
        assert!(tree.root.iter().all(|e| e.subnode.is_none()));
        let tree = check(0, 0x5A, 700, 100, false);
        assert_eq!(tree.root.len(), 1);
    }

    #[test]
    fn forced_blocks_give_the_layout_of_a_fresh_root_directory() {
        let tree = check(12, 0x52, 700, 100, true);
        assert_eq!(tree.blocks.len(), 1);
        assert_eq!(tree.root, vec![IndexEntry::end(Some(0))]);
        let tree = check(0, 0x52, 700, 100, true);
        assert_eq!(tree.blocks, vec![vec![IndexEntry::end(None)]]);
    }

    #[test]
    fn separators_live_in_the_root_only() {
        let tree = check(100, 0x5A, 700, 100, false);
        assert!(tree.blocks.len() >= 3);
        let separators: Vec<&IndexEntry> = tree.root.iter().filter(|e| !e.last).collect();
        assert_eq!(separators.len(), tree.blocks.len() - 1);
        for s in &separators {
            assert!(
                !tree.blocks.iter().flatten().any(|e| e.key == s.key),
                "a separator must not be repeated in a block"
            );
        }
        assert_eq!(tree.leaf_blocks().count(), tree.blocks.len());
    }

    #[test]
    fn deep_trees_and_every_small_count() {
        // every count across the first few splits, with a root too small
        // for more than two separators
        for count in 0..260 {
            check(count, 0x5A, 0x18 + 2 * 0x78, 100, false);
        }
        // 3000 entries: three levels with this root
        let tree = check(3000, 0x5A, 0x18 + 2 * 0x78, 100, false);
        assert!(tree.leaf_blocks().count() < tree.blocks.len(), "index nodes exist");
        // partially filled blocks need more of them
        let full = check(1000, 0x5A, 700, 100, false);
        let half = check(1000, 0x5A, 700, 50, false);
        assert!(half.blocks.len() > full.blocks.len() * 3 / 2);
    }

    #[test]
    fn a_root_too_small_for_any_separator_points_at_one_index_node() {
        // what ntfs-3g does for its 3000-file directory: the root holds only
        // the end entry, an index node holds the separators
        let tree = check(100, 0x5A, 0x18, 100, false);
        assert_eq!(tree.root.len(), 1);
        assert!(tree.root[0].last && tree.root[0].subnode.is_some());
        assert_eq!(tree.leaf_blocks().count(), tree.blocks.len() - 1);
    }

    #[test]
    fn longest_possible_names_still_fit() {
        // a 255-unit name: key of 0x42 + 510 bytes
        let tree = check(40, 0x42 + 510, 700, 100, false);
        assert!(tree.blocks.len() > 5);
    }
}

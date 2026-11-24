//! `INDX` blocks and the index-entry walker, on hand-built blocks.

mod common;

use common::build::{
    index_block, index_entry, last_entry, mref, protect, put16, put32, put64, Name,
};
use hds_ntfs::{
    apply_fixups, parse_index_block, parse_index_block_fixed, probe_index_block, Error, FileRef,
    FixupStatus, IndexBlock, IndexBlockProbe, IndexEntries, Namespace, WalkEnd,
    INDEX_ENTRY_HAS_SUBNODE, INDEX_ENTRY_LAST,
};

const DIR: u64 = 0x0003_0000_0000_0040; // record 64, sequence 3

fn key(text: &str) -> Vec<u8> {
    Name::new(DIR, text, 1).value()
}

fn entry(child: u64, text: &str) -> Vec<u8> {
    index_entry(child, &key(text), 0, None)
}

/// Parses a block that is in its in-memory form.
fn parse(bytes: &[u8]) -> IndexBlock<'_> {
    parse_index_block_fixed(bytes, FixupStatus::Ok).expect("block must parse")
}

/// Number of file-name entries and how the walk ended.
fn walk(bytes: &[u8]) -> (usize, u32, WalkEnd) {
    let mut entries = parse(bytes).entries();
    let count = entries.by_ref().count();
    (count, entries.skipped(), entries.end().unwrap())
}

// ------------------------------------------------------------------ probe

/// The first sector of a block with the given header fields.
fn sector(allocated: u32, usa_offset: u16, usa_count: u16) -> Vec<u8> {
    let mut s = vec![0u8; 512];
    s[0..4].copy_from_slice(b"INDX");
    put16(&mut s, 0x04, usa_offset);
    put16(&mut s, 0x06, usa_count);
    put32(&mut s, 0x20, allocated);
    s
}

#[test]
fn probe_takes_the_size_from_the_node_header() {
    // What every volume has: 4096 bytes, array at 0x28, nine entries,
    // allocated size 0xFE8 counted from the node header at 0x18.
    assert_eq!(
        probe_index_block(&sector(0xFE8, 0x28, 9)),
        Some(IndexBlockProbe {
            block_size: 4096,
            usa_offset: 0x28,
            usa_count: 9
        })
    );
    for shift in 0..32u32 {
        let size = 1u32 << shift;
        let count = (size / 512 + 1).min(0xFFFF) as u16;
        let accepted = probe_index_block(&sector(size.wrapping_sub(0x18), 0x28, count));
        assert_eq!(
            accepted.is_some(),
            (512..=65_536).contains(&size),
            "size {size}"
        );
        if let Some(probe) = accepted {
            assert_eq!(probe.block_size, size);
        }
    }
    // 0x18 + allocated must be the power of two, not allocated itself.
    assert_eq!(probe_index_block(&sector(0x1000, 0x28, 9)), None);
    assert_eq!(probe_index_block(&sector(0xFE0, 0x28, 9)), None);
    assert_eq!(probe_index_block(&sector(0, 0x28, 1)), None);
    assert_eq!(
        probe_index_block(&sector(u32::MAX, 0x28, 9)),
        None,
        "0x18 + allocated overflows"
    );
    assert_eq!(probe_index_block(&sector(u32::MAX - 0x17, 0x28, 9)), None);
}

#[test]
fn probe_count_and_array_position() {
    for count in [0u16, 1, 8, 9, 10, 17, 0xFFFF] {
        assert_eq!(
            probe_index_block(&sector(0xFE8, 0x28, count)).is_some(),
            count == 9,
            "count {count}"
        );
    }
    // The array starts behind the fixed header (0x28) and ends before the
    // tail of the first block: offset + 18 <= 510.
    for (usa_offset, valid) in [
        (0u16, false),
        (0x18, false),
        (0x27, false),
        (0x28, true),
        (0x2A, true),
        (492, true),
        (493, false),
        (0xFFFF, false),
    ] {
        assert_eq!(
            probe_index_block(&sector(0xFE8, usa_offset, 9)).is_some(),
            valid,
            "offset {usa_offset:#x}"
        );
    }
}

#[test]
fn probe_needs_the_magic_and_the_header() {
    let good = sector(0xFE8, 0x28, 9);
    for position in 0..4 {
        let mut bad = good.clone();
        bad[position] ^= 0x20;
        assert_eq!(probe_index_block(&bad), None);
    }
    let mut file = good.clone();
    file[0..4].copy_from_slice(b"FILE");
    assert_eq!(probe_index_block(&file), None);
    assert_eq!(probe_index_block(&[0u8; 512]), None);
    assert_eq!(probe_index_block(&[]), None);
    assert!(probe_index_block(&good[..0x28]).is_some());
    assert_eq!(probe_index_block(&good[..0x27]), None);
}

// ------------------------------------------------------------------ block

#[test]
fn block_header_and_entries() {
    let entries = vec![
        entry(mref(70, 1), "alpha.txt"),
        index_entry(
            mref(71, 9),
            &Name {
                flags: 0x1000_0000,
                namespace: 3,
                ..Name::new(DIR, "beta", 3)
            }
            .value(),
            0,
            None,
        ),
        entry(mref(0x0000_FFFF_FFFF_FFFF, 0xFFFF), "gamma"),
        last_entry(None),
    ];
    let bytes = index_block(
        4096,
        0x0000_0000_0000_0003,
        0x0807_0605_0403_0201,
        0,
        &entries,
    );
    let block = parse(&bytes);
    assert_eq!(block.size(), 4096);
    assert_eq!(block.vcn(), 3);
    assert_eq!(block.lsn(), 0x0807_0605_0403_0201);
    assert_eq!(block.fixup(), FixupStatus::Ok);
    assert_eq!(block.bytes(), &bytes[..]);
    // Node header at 0x18, offsets relative to it: first entry at 0x40.
    assert_eq!(block.node().entries_offset, 0x28);
    assert_eq!(block.node().allocated_size, 0xFE8);
    let used: usize = entries.iter().map(Vec::len).sum();
    assert_eq!(block.node().used_size as usize, 0x28 + used);
    assert!(!block.node().has_subnodes());

    let mut walker = block.entries();
    let found: Vec<_> = walker.by_ref().collect();
    assert_eq!(walker.end(), Some(WalkEnd::Complete));
    assert_eq!(walker.skipped(), 0);
    assert_eq!(found.len(), 3);
    let mut offset = 0x40u32;
    for (index_entry, built) in found.iter().zip(&entries) {
        assert_eq!(index_entry.offset, offset);
        assert_eq!(&bytes[offset as usize..][..built.len()], &built[..]);
        offset += built.len() as u32;
        assert_eq!(index_entry.file_name.parent, FileRef::from_raw(DIR));
        assert_eq!(index_entry.flags, 0);
        assert_eq!(index_entry.subnode_vcn, None);
    }
    assert_eq!(found[0].child, FileRef::new(70, 1));
    assert!(found[0].file_name.name == "alpha.txt");
    assert_eq!(found[0].file_name.namespace, Namespace::Win32);
    assert!(!found[0].file_name.is_directory());
    assert_eq!(found[1].child, FileRef::new(71, 9));
    assert!(found[1].file_name.is_directory());
    assert_eq!(found[1].file_name.namespace, Namespace::Win32AndDos);
    assert_eq!(found[2].child.number(), 0x0000_FFFF_FFFF_FFFF);
    assert_eq!(found[2].child.sequence(), 0xFFFF);
    assert_eq!(block.owner(), Some(FileRef::new(64, 3)));
}

#[test]
fn entry_times_sizes_and_flags_come_from_the_key() {
    let name = Name {
        parent: DIR,
        times: [11, 22, 33, 44],
        allocated: 8192,
        real: 6230,
        flags: 0x0000_0420,
        reparse: 0xA000_000C,
        namespace: 0,
        name: "a_link".encode_utf16().collect(),
    };
    let bytes = index_block(
        4096,
        0,
        0,
        0,
        &[
            index_entry(mref(42, 1), &name.value(), 0, None),
            last_entry(None),
        ],
    );
    let found = parse(&bytes).entries().next().unwrap();
    let key = found.file_name;
    assert_eq!(
        (
            key.times.created,
            key.times.modified,
            key.times.mft_changed,
            key.times.accessed
        ),
        (11, 22, 33, 44)
    );
    assert_eq!((key.allocated_size, key.real_size), (8192, 6230));
    assert_eq!(key.flags, 0x0000_0420);
    // The reparse tag is copied into the index entry.
    assert_eq!(key.reparse_or_ea, 0xA000_000C);
    assert_eq!(key.namespace, Namespace::Posix);
}

#[test]
fn sub_node_vcns_are_the_last_eight_bytes_of_an_entry() {
    assert_eq!((INDEX_ENTRY_HAS_SUBNODE, INDEX_ENTRY_LAST), (1, 2));
    let entries = vec![
        index_entry(mref(70, 1), &key("m"), 0, Some(5)),
        index_entry(mref(71, 1), &key("t"), 0, Some(0x0000_0001_0000_0000)),
        last_entry(Some(9)),
    ];
    assert_eq!(entries[2].len(), 0x18);
    let bytes = index_block(4096, 0, 0, 1, &entries);
    let block = parse(&bytes);
    assert!(block.node().has_subnodes());
    let found: Vec<_> = block.entries().collect();
    assert_eq!(found.len(), 2);
    assert_eq!((found[0].flags, found[0].subnode_vcn), (1, Some(5)));
    assert_eq!(found[1].subnode_vcn, Some(0x0000_0001_0000_0000));
    assert!(found[0].file_name.name == "m" && found[1].file_name.name == "t");
    assert_eq!(walk(&bytes), (2, 0, WalkEnd::Complete));
}

#[test]
fn a_long_name_and_its_dos_alias_are_two_entries_for_one_file() {
    let entries = vec![
        index_entry(
            mref(36, 1),
            &Name::new(DIR, "System Volume Information", 1).value(),
            0,
            None,
        ),
        index_entry(mref(36, 1), &Name::new(DIR, "SYSTEM~1", 2).value(), 0, None),
        last_entry(None),
    ];
    let bytes = index_block(4096, 0, 0, 0, &entries);
    let found: Vec<_> = parse(&bytes).entries().collect();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].child, found[1].child);
    assert_eq!(
        (found[0].file_name.namespace, found[1].file_name.namespace),
        (Namespace::Win32, Namespace::Dos)
    );
}

#[test]
fn only_keys_that_are_file_names_are_returned() {
    let entries = vec![
        entry(mref(70, 1), "first"),
        // Keys of other indexes: $SII (4 bytes), $SDH (8), $O (16), $R (12).
        index_entry(0x0014_0018, &[1, 1, 0, 0], 0, None),
        index_entry(0x0014_0020, &[0xAA; 8], 0, None),
        index_entry(0x0038_0020, &[0xBB; 16], 0, None),
        index_entry(0, &[0xCC; 12], 0, None),
        // No key at all without the last flag.
        index_entry(mref(1, 1), &[], 0, None),
        entry(mref(71, 1), "second"),
        last_entry(None),
    ];
    let bytes = index_block(4096, 0, 0, 0, &entries);
    let mut walker = parse(&bytes).entries();
    let names: Vec<String> = walker
        .by_ref()
        .map(|e| e.file_name.name.to_string_lossy())
        .collect();
    assert_eq!(names, ["first", "second"]);
    assert_eq!(walker.skipped(), 5);
    assert_eq!(walker.end(), Some(WalkEnd::Complete));
}

#[test]
fn a_key_must_be_exactly_as_long_as_its_name_needs() {
    let good = key("name");
    assert_eq!(good.len(), 0x42 + 8);
    let mut longer = good.clone();
    longer.extend([0u8; 2]);
    let mut bad_namespace = good.clone();
    bad_namespace[0x41] = 4;
    let mut no_name = good.clone();
    no_name[0x40] = 0;
    let mut name_too_long = good.clone();
    name_too_long[0x40] = 5;
    for (bad, why) in [
        (&longer, "two spare bytes"),
        (&good[..good.len() - 2].to_vec(), "cut by one unit"),
        (&bad_namespace, "namespace 4"),
        (&no_name, "empty name"),
        (&name_too_long, "name longer than the key"),
        (&vec![0u8; 0x42], "fixed part only"),
        (&vec![0xFFu8; 0x60], "all ones"),
    ] {
        let bytes = index_block(
            4096,
            0,
            0,
            0,
            &[
                index_entry(mref(70, 1), bad, 0, None),
                entry(mref(71, 1), "ok"),
                last_entry(None),
            ],
        );
        let mut walker = parse(&bytes).entries();
        let names: Vec<String> = walker
            .by_ref()
            .map(|e| e.file_name.name.to_string_lossy())
            .collect();
        assert_eq!(names, ["ok"], "{why}");
        assert_eq!(
            (walker.skipped(), walker.end()),
            (1, Some(WalkEnd::Complete)),
            "{why}"
        );
    }
}

#[test]
fn the_entry_flagged_last_ends_the_walk_even_if_more_follows() {
    // Stale entries behind the end entry, still inside the used size.
    let entries = vec![
        entry(mref(70, 1), "live"),
        last_entry(None),
        entry(mref(71, 1), "stale"),
    ];
    let bytes = index_block(4096, 0, 0, 0, &entries);
    let mut walker = parse(&bytes).entries();
    assert!(walker.next().unwrap().file_name.name == "live");
    assert_eq!(walker.next(), None);
    assert_eq!(walker.end(), Some(WalkEnd::Complete));
    assert_eq!(walker.next(), None, "fused");
    // A last entry that carries a key is still the end.
    let bytes = index_block(
        4096,
        0,
        0,
        0,
        &[
            index_entry(mref(70, 1), &key("x"), INDEX_ENTRY_LAST, None),
            entry(mref(71, 1), "y"),
        ],
    );
    assert_eq!(walk(&bytes), (0, 0, WalkEnd::Complete));
}

#[test]
fn a_walk_without_end_entry_is_broken() {
    let bytes = index_block(
        4096,
        0,
        0,
        0,
        &[entry(mref(70, 1), "a"), entry(mref(71, 1), "b")],
    );
    assert_eq!(walk(&bytes), (2, 0, WalkEnd::Broken));
    // No entries at all.
    let bytes = index_block(4096, 0, 0, 0, &[]);
    assert_eq!(walk(&bytes), (0, 0, WalkEnd::Broken));
    assert_eq!(parse(&bytes).owner(), None);
}

/// A block `[good]["victim"][good][last]` whose second entry is damaged.
fn with_damaged_second(damage: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
    let mut victim = entry(mref(71, 1), "victim");
    damage(&mut victim);
    index_block(
        4096,
        0,
        0,
        0,
        &[
            entry(mref(70, 1), "good"),
            victim,
            entry(mref(72, 1), "after"),
            last_entry(None),
        ],
    )
}

#[test]
fn an_impossible_entry_stops_the_walk() {
    for (length, why) in [
        (0u16, "zero: must not loop"),
        (8, "shorter than an entry header"),
        (0x0F, "shorter than an entry header"),
        (0x5C, "not a multiple of 8"),
        (0x61, "not a multiple of 8"),
        (0x1000, "past the used size"),
        (0xFFF8, "huge"),
    ] {
        let bytes = with_damaged_second(|e| put16(e, 0x08, length));
        assert_eq!(walk(&bytes), (1, 0, WalkEnd::Broken), "{why}: {length:#x}");
    }
    // The key does not fit in the entry.
    let bytes = with_damaged_second(|e| put16(e, 0x0A, 0x51));
    assert_eq!(walk(&bytes), (1, 0, WalkEnd::Broken));
    let bytes = with_damaged_second(|e| put16(e, 0x0A, 0xFFFF));
    assert_eq!(walk(&bytes), (1, 0, WalkEnd::Broken));
    // A sub-node flag on an entry with no room for the VCN behind the key.
    let bytes = with_damaged_second(|e| put16(e, 0x0C, INDEX_ENTRY_HAS_SUBNODE));
    assert_eq!(walk(&bytes), (1, 0, WalkEnd::Broken));
    // A last entry with the sub-node flag but only 0x10 bytes.
    let mut short_last = last_entry(None);
    put16(
        &mut short_last,
        0x0C,
        INDEX_ENTRY_LAST | INDEX_ENTRY_HAS_SUBNODE,
    );
    let bytes = index_block(4096, 0, 0, 0, &[entry(mref(70, 1), "good"), short_last]);
    assert_eq!(walk(&bytes), (1, 0, WalkEnd::Broken));
    // Unknown flag bits are carried, not judged.
    let bytes = with_damaged_second(|e| put16(e, 0x0C, 0xFFF0));
    let flags: Vec<u16> = parse(&bytes).entries().map(|e| e.flags).collect();
    assert_eq!(flags, [0, 0xFFF0, 0]);
}

#[test]
fn the_used_size_bounds_the_walk() {
    let entries = vec![
        entry(mref(70, 1), "a"),
        entry(mref(71, 1), "b"),
        last_entry(None),
    ];
    let good = index_block(4096, 0, 0, 0, &entries);
    let first_len = entries[0].len() as u32;
    // Used size ends inside the second entry.
    let mut cut = good.clone();
    put32(&mut cut, 0x1C, 0x28 + first_len + 0x20);
    assert_eq!(walk(&cut), (1, 0, WalkEnd::Broken));
    // Used size ends right behind the first entry: no end entry.
    let mut cut = good.clone();
    put32(&mut cut, 0x1C, 0x28 + first_len);
    assert_eq!(walk(&cut), (1, 0, WalkEnd::Broken));
    // No room for a single entry header.
    let mut cut = good.clone();
    put32(&mut cut, 0x1C, 0x28 + 0x0F);
    assert_eq!(walk(&cut), (0, 0, WalkEnd::Broken));
    let mut cut = good.clone();
    put32(&mut cut, 0x1C, 0x28);
    assert_eq!(walk(&cut), (0, 0, WalkEnd::Broken));
}

#[test]
fn a_full_block_is_walked_to_its_last_byte() {
    // Entries of 0x60 bytes ("n" + 3 digits: a key of 0x4A bytes behind
    // the 0x10-byte entry header, rounded up to 8) until the end entry
    // occupies the last 16 bytes of the block.
    let mut entries = Vec::new();
    let room = 4096 - 0x40 - 0x10;
    for index in 0..room / 0x60 {
        entries.push(entry(mref(100 + index as u64, 1), &format!("n{index:03}")));
    }
    assert_eq!(entries[0].len(), 0x60);
    let spare = room % 0x60;
    // Grow the last name so that nothing is left over.
    let last = entries.pop().unwrap();
    let grown = "x".repeat(4 + spare / 2);
    entries.push(entry(mref(999, 1), &grown));
    assert_eq!(entries.last().unwrap().len(), last.len() + spare);
    entries.push(last_entry(None));
    let bytes = index_block(4096, 0, 0, 0, &entries);
    let block = parse(&bytes);
    assert_eq!(block.node().used_size, block.node().allocated_size);
    assert_eq!(walk(&bytes), (entries.len() - 1, 0, WalkEnd::Complete));
}

// ----------------------------------------------------------------- errors

#[test]
fn a_node_header_that_contradicts_the_block_is_refused() {
    let good = index_block(4096, 0, 0, 0, &[entry(mref(70, 1), "a"), last_entry(None)]);
    let refused = |mutate: &dyn Fn(&mut Vec<u8>)| {
        let mut bytes = good.clone();
        mutate(&mut bytes);
        parse_index_block_fixed(&bytes, FixupStatus::Ok).err()
    };
    assert_eq!(refused(&|_| {}), None);
    // Entries inside the node header.
    assert_eq!(
        refused(&|b| put32(b, 0x18, 0x0F)),
        Some(Error::BadIndexHeader)
    );
    assert_eq!(refused(&|b| put32(b, 0x18, 0)), Some(Error::BadIndexHeader));
    // Entries behind the used size.
    assert_eq!(
        refused(&|b| put32(b, 0x18, 0x1000)),
        Some(Error::BadIndexHeader)
    );
    assert_eq!(
        refused(&|b| put32(b, 0x18, u32::MAX)),
        Some(Error::BadIndexHeader)
    );
    // Used size beyond the allocated size.
    assert_eq!(
        refused(&|b| put32(b, 0x1C, 0xFE9)),
        Some(Error::BadIndexHeader)
    );
    assert_eq!(
        refused(&|b| put32(b, 0x1C, u32::MAX)),
        Some(Error::BadIndexHeader)
    );
    // Used size equal to the allocated size is the full block.
    assert_eq!(refused(&|b| put32(b, 0x1C, 0xFE8)), None);
    // Entries inside the update sequence array (0x28..0x3A in the block,
    // 0x10..0x22 from the node header): refused up to its end.
    assert_eq!(
        refused(&|b| put32(b, 0x18, 0x10)),
        Some(Error::BadIndexHeader)
    );
    assert_eq!(
        refused(&|b| put32(b, 0x18, 0x21)),
        Some(Error::BadIndexHeader)
    );
    assert_eq!(refused(&|b| put32(b, 0x18, 0x22)), None);
    // An entries offset that is not a multiple of 8 is not judged here.
    assert_eq!(refused(&|b| put32(b, 0x18, 0x2C)), None);
    // The probe's checks.
    assert_eq!(refused(&|b| b[0] = b'X'), Some(Error::BadMagic));
    assert_eq!(
        refused(&|b| put32(b, 0x20, 0x1FE8)),
        Some(Error::BadUsa),
        "8 KiB with nine array entries"
    );
    assert_eq!(
        refused(&|b| put32(b, 0x20, 0x1234)),
        Some(Error::BadIndexBlockSize)
    );
    assert_eq!(refused(&|b| put16(b, 0x04, 0x20)), Some(Error::BadUsa));
    assert_eq!(refused(&|b| b.truncate(4095)), Some(Error::TooShort));
    assert_eq!(refused(&|b| b.truncate(0x27)), Some(Error::TooShort));
}

#[test]
fn the_block_must_be_all_there_and_nothing_behind_it_is_touched() {
    let memory = index_block(4096, 7, 0, 0, &[entry(mref(70, 1), "a"), last_entry(None)]);
    let mut disk = memory.clone();
    protect(&mut disk, 0x0B0B);
    let pristine = disk.clone();
    assert_eq!(
        parse_index_block(&mut disk[..4095]).err(),
        Some(Error::TooShort)
    );
    assert_eq!(
        parse_index_block(&mut disk[..512]).err(),
        Some(Error::TooShort)
    );
    assert_eq!(parse_index_block(&mut []).err(), Some(Error::TooShort));
    assert_eq!(disk, pristine, "a refused buffer must not be modified");

    disk.extend([0x0B; 4096]);
    let block = parse_index_block(&mut disk).unwrap();
    assert_eq!(
        (block.size(), block.vcn(), block.fixup()),
        (4096, 7, FixupStatus::Ok)
    );
    assert!(disk[4096..].iter().all(|b| *b == 0x0B));
    assert_eq!(disk[0x40..4096], memory[0x40..]);
}

// ------------------------------------------------------------------ fixups

#[test]
fn entries_are_decoded_after_fixups() {
    // Enough entries that names lie across several 512-byte block ends.
    let mut entries: Vec<Vec<u8>> = (0..30)
        .map(|i| entry(mref(100 + i, 1), &format!("file-number-{i:02}.dat")))
        .collect();
    entries.push(last_entry(None));
    let memory = index_block(4096, 2, 77, 0, &entries);
    let mut disk = memory.clone();
    protect(&mut disk, 0xF1F2);

    // Without fixups an entry is hit by the number.
    let spoiled: Vec<String> = parse(&disk)
        .entries()
        .map(|e| e.file_name.name.to_string_lossy())
        .collect();
    let expected: Vec<String> = (0..30).map(|i| format!("file-number-{i:02}.dat")).collect();
    assert_ne!(spoiled, expected);

    let block = parse_index_block(&mut disk).unwrap();
    assert_eq!(block.fixup(), FixupStatus::Ok);
    let names: Vec<String> = block
        .entries()
        .map(|e| e.file_name.name.to_string_lossy())
        .collect();
    assert_eq!(names, expected);
    assert_eq!((block.vcn(), block.lsn()), (2, 77));
}

#[test]
fn a_torn_block_is_parsed_as_far_as_it_goes() {
    let mut entries: Vec<Vec<u8>> = (0..30)
        .map(|i| entry(mref(100 + i, 1), &format!("file-number-{i:02}.dat")))
        .collect();
    entries.push(last_entry(None));
    let mut disk = index_block(4096, 0, 0, 0, &entries);
    protect(&mut disk, 0x0033);
    put16(&mut disk, 5 * 512 - 2, 0x0032);
    put16(&mut disk, 8 * 512 - 2, 0x0032);
    let block = parse_index_block(&mut disk).unwrap();
    assert_eq!(
        block.fixup(),
        FixupStatus::Torn {
            first_bad_block: 4,
            bad_blocks: 2
        }
    );
    // The walk still runs; what it finds in the damaged blocks is the
    // caller's to distrust.
    assert_eq!(block.entries().count(), 30);
    assert_eq!(block.owner(), Some(FileRef::from_raw(DIR)));
}

#[test]
fn the_fixed_variant_equals_two_steps() {
    let memory = index_block(4096, 1, 2, 0, &[entry(mref(70, 1), "a"), last_entry(None)]);
    let mut disk = memory.clone();
    protect(&mut disk, 0x1111);
    let mut stepwise = disk.clone();
    let status = apply_fixups(&mut stepwise).unwrap();
    assert_eq!(status, FixupStatus::Ok);
    let by_steps = parse_index_block_fixed(&stepwise, status).unwrap();
    let mut at_once = disk.clone();
    let direct = parse_index_block(&mut at_once).unwrap();
    assert_eq!(by_steps.bytes(), direct.bytes());
    assert_eq!(by_steps.entries().count(), 1);
    let torn = FixupStatus::Torn {
        first_bad_block: 0,
        bad_blocks: 8,
    };
    assert_eq!(
        parse_index_block_fixed(&memory, torn).unwrap().fixup(),
        torn
    );
}

// ---------------------------------------------------------- other sizes

#[test]
fn blocks_of_other_sizes() {
    for size in [512usize, 1024, 8192, 65_536] {
        let entries = vec![
            entry(mref(70, 1), "a"),
            entry(mref(71, 1), "b"),
            last_entry(None),
        ];
        let memory = index_block(size, 16, 5, 0, &entries);
        let mut disk = memory.clone();
        protect(&mut disk, 0x7001);
        let probe = probe_index_block(&disk).unwrap();
        assert_eq!(probe.block_size as usize, size);
        assert_eq!(probe.usa_count as usize, size / 512 + 1);
        let block = parse_index_block(&mut disk).unwrap();
        assert_eq!(block.size() as usize, size);
        assert_eq!(block.vcn(), 16);
        let found: Vec<_> = block.entries().collect();
        assert_eq!(found.len(), 2, "size {size}");
        // The first entry follows the array, at the next multiple of 8.
        let first = (0x28 + 2 * (size / 512 + 1) + 7) & !7;
        assert_eq!(found[0].offset as usize, first);
    }
}

#[test]
fn the_vcn_is_handed_out_as_stored() {
    let mut bytes = index_block(4096, 0, 0, 0, &[last_entry(None)]);
    for vcn in [0u64, 8, 0x0000_0001_0000_0000, 1 << 63, u64::MAX] {
        put64(&mut bytes, 0x10, vcn);
        assert_eq!(parse(&bytes).vcn(), vcn);
    }
}

// ------------------------------------------------------------------ owner

fn entry_of(parent: u64, text: &str) -> Vec<u8> {
    index_entry(mref(200, 1), &Name::new(parent, text, 0).value(), 0, None)
}

#[test]
fn the_owner_is_the_parent_shared_by_most_entries() {
    let a = mref(10, 1);
    let b = mref(11, 4);
    let c = mref(12, 1);
    let owner = |parents: &[u64]| {
        let mut entries: Vec<Vec<u8>> = parents
            .iter()
            .enumerate()
            .map(|(i, p)| entry_of(*p, &format!("e{i}")))
            .collect();
        entries.push(last_entry(None));
        let bytes = index_block(4096, 0, 0, 0, &entries);
        parse(&bytes).owner().map(|r| (r.number(), r.sequence()))
    };
    assert_eq!(owner(&[]), None);
    assert_eq!(owner(&[a]), Some((10, 1)));
    assert_eq!(owner(&[a, a, a, a]), Some((10, 1)));
    assert_eq!(owner(&[a, b, b]), Some((11, 4)));
    assert_eq!(owner(&[b, a, a, c, a, b]), Some((10, 1)));
    // A plurality is enough.
    assert_eq!(owner(&[a, b, c, c]), Some((12, 1)));
    // Among equals the first one met.
    assert_eq!(owner(&[b, a]), Some((11, 4)));
    assert_eq!(owner(&[c, a, b, a, b, c]), Some((12, 1)));
    // The same record number with another sequence is another directory.
    assert_eq!(
        owner(&[mref(10, 2), mref(10, 1), mref(10, 2)]),
        Some((10, 2))
    );
}

#[test]
fn the_owner_count_follows_only_the_first_sixteen_distinct_parents() {
    // Sixteen parents once each, then a seventeenth three times: it is not
    // counted, and the first of the sixteen wins the tie.
    let mut entries: Vec<Vec<u8>> = (0..16)
        .map(|i| entry_of(mref(100 + i, 1), &format!("e{i}")))
        .collect();
    for i in 0..3 {
        entries.push(entry_of(mref(999, 1), &format!("late{i}")));
    }
    entries.push(last_entry(None));
    let bytes = index_block(4096, 0, 0, 0, &entries);
    assert_eq!(parse(&bytes).owner(), Some(FileRef::new(100, 1)));
    // Inside the first sixteen, counting works as usual.
    let mut entries: Vec<Vec<u8>> = (0..16)
        .map(|i| entry_of(mref(100 + i, 1), &format!("e{i}")))
        .collect();
    entries.push(entry_of(mref(115, 1), "again"));
    entries.push(last_entry(None));
    let bytes = index_block(4096, 0, 0, 0, &entries);
    assert_eq!(parse(&bytes).owner(), Some(FileRef::new(115, 1)));
}

// ----------------------------------------------------- the walker by itself

#[test]
fn the_walker_works_on_any_run_of_entries() {
    let mut raw = Vec::new();
    raw.extend(entry(mref(70, 1), "one"));
    let second_at = raw.len() as u32;
    raw.extend(entry(mref(71, 1), "two"));
    raw.extend(last_entry(None));
    let mut walker = IndexEntries::new(&raw, 1000);
    assert_eq!(walker.end(), None);
    let first = walker.next().unwrap();
    assert_eq!(first.offset, 1000);
    assert!(first.file_name.name == "one");
    let second = walker.next().unwrap();
    assert_eq!(second.offset, 1000 + second_at);
    assert_eq!(walker.next(), None);
    assert_eq!(walker.end(), Some(WalkEnd::Complete));
    assert!(WalkEnd::Complete.is_complete() && !WalkEnd::Broken.is_complete());

    let mut empty = IndexEntries::new(&[], 0);
    assert_eq!(empty.next(), None);
    assert_eq!(empty.end(), Some(WalkEnd::Broken));
    // Fifteen bytes are no entry header.
    let mut short = IndexEntries::new(&raw[..15], 0);
    assert_eq!(short.next(), None);
    assert_eq!(short.end(), Some(WalkEnd::Broken));
}

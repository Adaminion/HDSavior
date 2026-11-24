//! Feeds arbitrary bytes to every public entry point of the crate and
//! checks what must hold for *any* input: no panic (a panic fails the
//! test), results that stay inside the input, walks that make progress, and
//! the arithmetic promises the types document.

use hds_ntfs::{
    apply_fixups, attr_type, decode_runlist, filetime_is_valid, filetime_to_unix_ms,
    filetime_to_unix_time, fnv1a64, is_boot_sector_candidate, next_seq, parse_boot_sector,
    parse_file_record, parse_file_record_fixed, parse_index_block, parse_index_block_fixed,
    probe_file_record, probe_index_block, unix_ms_to_filetime, AttrBody, AttrChain,
    AttrListEntries, AttributeList, Extent, FileName, FileRecord, FileRef, FixupStatus, IndexBlock,
    IndexEntries, IndexRoot, Run, RunlistStatus, Runs, Utf16Name, MAX_RECORD_SIZE,
};

/// How deep the parsers got, so that a test can tell that its inputs were
/// not all turned away at the door.
#[derive(Default, Debug, Clone, Copy)]
pub struct Reached {
    pub inputs: u64,
    pub records: u64,
    pub attributes: u64,
    pub malformed: u64,
    pub names: u64,
    pub extents: u64,
    pub runs: u64,
    pub list_entries: u64,
    pub roots: u64,
    pub blocks: u64,
    pub entries: u64,
    pub boots: u64,
}

impl Reached {
    pub fn add(&mut self, other: &Reached) {
        self.inputs += other.inputs;
        self.records += other.records;
        self.attributes += other.attributes;
        self.malformed += other.malformed;
        self.names += other.names;
        self.extents += other.extents;
        self.runs += other.runs;
        self.list_entries += other.list_entries;
        self.roots += other.roots;
        self.blocks += other.blocks;
        self.entries += other.entries;
        self.boots += other.boots;
    }
}

/// Position of `inner` inside `outer`; panics when it is not inside.
fn position(outer: &[u8], inner: &[u8]) -> usize {
    let start = inner.as_ptr() as usize;
    let base = outer.as_ptr() as usize;
    assert!(
        start >= base && start + inner.len() <= base + outer.len(),
        "a returned slice lies outside its buffer"
    );
    start - base
}

fn check_runs(runlist: &[u8], scratch: &mut Vec<Run>, reached: &mut Reached) {
    let mut decoder = Runs::new(runlist);
    let mut total = 0u64;
    let mut count = 0usize;
    assert_eq!(decoder.status(), None);
    for run in &mut decoder {
        count += 1;
        assert!(
            count <= runlist.len() / 2,
            "more runs than the bytes can hold"
        );
        assert!(run.length >= 1, "empty run");
        if let Some(lcn) = run.lcn {
            assert!(
                lcn <= i64::MAX as u64 && lcn + run.length <= i64::MAX as u64,
                "run beyond 63 bits"
            );
        }
        total += run.length;
        assert!(total <= i64::MAX as u64, "runlist longer than 63 bits");
    }
    let status = decoder.status().expect("a finished decoder has a status");
    assert_eq!(decoder.clusters(), total);
    assert!(decoder.consumed() <= runlist.len());
    assert!(decoder.next().is_none(), "the decoder must stay finished");
    if status == RunlistStatus::Complete {
        assert_eq!(
            runlist[decoder.consumed() - 1],
            0,
            "complete means the terminator was consumed"
        );
    }
    // The vector form is the same decoder and appends.
    scratch.clear();
    scratch.push(Run {
        lcn: None,
        length: 1,
    });
    let decoded = decode_runlist(runlist, scratch);
    assert_eq!(scratch.len(), count + 1);
    assert_eq!(
        (
            decoded.status,
            decoded.runs,
            decoded.clusters,
            decoded.consumed
        ),
        (status, count, total, decoder.consumed())
    );
    reached.runs += count as u64;
}

fn check_extent(extent: &Extent<'_>, scratch: &mut Vec<Run>, reached: &mut Reached) {
    reached.extents += 1;
    assert_eq!(extent.sizes().is_some(), extent.start_vcn == 0);
    check_runs(extent.runlist, scratch, reached);
    let mut out = Vec::new();
    let decoded = extent.decode_runs(&mut out);
    assert_eq!(decoded.runs, extent.runs().count());
}

fn check_file_name(name: &FileName<'_>, reached: &mut Reached) {
    reached.names += 1;
    assert!(
        (1..=255).contains(&name.name.len()),
        "name length {}",
        name.name.len()
    );
    assert_eq!(name.name.as_bytes().len(), 2 * name.name.len());
    assert_eq!(name.name.units().count(), name.name.len());
    assert!(name.namespace.as_u8() <= 3);
    let _ = (
        name.is_directory(),
        name.parent.number(),
        name.times,
        name.reparse_or_ea,
    );
}

fn check_entries(
    container: &[u8],
    mut entries: IndexEntries<'_>,
    base: u32,
    reached: &mut Reached,
) {
    assert_eq!(entries.end(), None);
    let owner = entries.clone().owner();
    let mut previous: Option<u32> = None;
    let mut count = 0usize;
    for entry in &mut entries {
        count += 1;
        assert!(
            count <= container.len() / 0x58 + 1,
            "more entries than the bytes can hold"
        );
        assert!(
            previous.is_none_or(|p| entry.offset >= p + 0x58),
            "entries must advance"
        );
        previous = Some(entry.offset);
        assert!(entry.offset >= base);
        let at = (entry.offset - base) as usize;
        assert!(at + 0x58 <= container.len(), "entry outside its node");
        let name_at = position(container, entry.file_name.name.as_bytes());
        assert!(name_at >= at + 0x52, "the name lies behind the key header");
        check_file_name(&entry.file_name, reached);
        assert_eq!(entry.subnode_vcn.is_some(), entry.flags & 1 != 0);
        assert_eq!(entry.flags & 2, 0, "the last entry is never returned");
    }
    assert!(entries.end().is_some());
    assert!(entries.next().is_none());
    assert_eq!(owner.is_some(), count > 0);
    assert!(entries.skipped() as usize <= container.len() / 0x10);
    reached.entries += count as u64;
}

fn check_attr_list(value: &[u8], reached: &mut Reached) {
    let mut entries = AttrListEntries::new(value);
    let mut count = 0usize;
    for entry in &mut entries {
        count += 1;
        assert!(
            count <= value.len() / 0x1A,
            "more list entries than the bytes can hold"
        );
        assert!(entry.name.len() <= 255);
        if !entry.name.is_empty() {
            position(value, entry.name.as_bytes());
        }
        let _ = (
            entry.type_code,
            entry.start_vcn,
            entry.holder.number(),
            entry.attr_id,
        );
    }
    assert!(entries.end().is_some());
    assert!(entries.next().is_none());
    reached.list_entries += count as u64;
}

fn check_record(record: &FileRecord<'_>, scratch: &mut Vec<Run>, reached: &mut Reached) {
    reached.records += 1;
    let bytes = record.bytes();
    let header = *record.header();
    assert!(
        header.allocated_size.is_power_of_two() && (1024..=65_536).contains(&header.allocated_size)
    );
    assert_eq!(bytes.len(), header.allocated_size as usize);
    assert!(header.used_size <= header.allocated_size);
    assert!(header.attrs_offset % 8 == 0 && u32::from(header.attrs_offset) + 4 <= header.used_size);
    assert_eq!(header.record_number.is_some(), header.usa_offset >= 0x30);
    assert_eq!(u32::from(header.usa_count), header.allocated_size / 512 + 1);
    assert_eq!(record.is_extension(), !record.base().is_zero());
    assert_eq!(record.is_in_use(), record.flags() & 1 != 0);
    assert_eq!(record.is_directory(), record.flags() & 2 != 0);

    let mut next = u32::from(header.attrs_offset);
    let mut count = 0u32;
    let mut names = 0usize;
    for attr in record.attributes() {
        count += 1;
        reached.attributes += 1;
        assert!(
            count <= header.allocated_size / 0x18,
            "more attributes than the record can hold"
        );
        assert_eq!(attr.offset, next, "attributes follow each other");
        assert!(attr.length >= 0x18 && attr.length % 8 == 0);
        assert!(
            attr.offset + attr.length <= header.used_size,
            "attribute outside the used size"
        );
        next = attr.offset + attr.length;
        let range = attr.offset as usize..(attr.offset + attr.length) as usize;
        assert_ne!(attr.type_code, attr_type::END);
        assert!(attr.name.len() <= 255);
        if !attr.name.is_empty() {
            let at = position(bytes, attr.name.as_bytes());
            assert!(
                at >= range.start + 0x18 && at + attr.name.as_bytes().len() <= range.end,
                "name outside its attribute"
            );
        }
        assert_eq!(attr.is_unnamed(), attr.name.is_empty());
        assert_eq!(attr.is_named(""), attr.name.is_empty());
        let _ = (attr.is_encrypted(), attr.is_sparse());
        match attr.body {
            AttrBody::Malformed => {
                reached.malformed += 1;
                assert!(
                    attr.is_malformed() && attr.resident().is_none() && attr.extent().is_none()
                );
                assert!(!attr.is_compressed());
                assert!(attr.standard_information().is_none() && attr.file_name().is_none());
                assert!(attr.attribute_list().is_none() && attr.index_root().is_none());
                assert!(attr.volume_name().is_none() && attr.reparse_tag().is_none());
            }
            AttrBody::Resident(value) => {
                assert!(!attr.is_compressed(), "resident data is never compressed");
                assert_eq!(value.len() as usize, value.bytes.len());
                let at = value.offset as usize;
                assert!(
                    at >= range.start && at + value.bytes.len() <= range.end,
                    "value outside its attribute"
                );
                if !value.is_empty() {
                    assert_eq!(
                        position(bytes, value.bytes),
                        at,
                        "the offset is where the bytes are"
                    );
                    assert!(at >= range.start + 0x18);
                }
                if let Some(si) = attr.standard_information() {
                    assert!(attr.type_code == attr_type::STANDARD_INFORMATION && value.len() >= 48);
                    let _ = (si.times, si.flags);
                }
                if let Some(name) = attr.file_name() {
                    assert_eq!(attr.type_code, attr_type::FILE_NAME);
                    position(value.bytes, name.name.as_bytes());
                    check_file_name(&name, reached);
                    names += 1;
                }
                match attr.attribute_list() {
                    Some(AttributeList::Resident(_)) => {
                        assert_eq!(attr.type_code, attr_type::ATTRIBUTE_LIST);
                        check_attr_list(value.bytes, reached);
                    }
                    Some(AttributeList::NonResident(_)) => {
                        panic!("a resident list reported as non-resident")
                    }
                    None => assert_ne!(attr.type_code, attr_type::ATTRIBUTE_LIST),
                }
                if let Some(root) = attr.index_root() {
                    reached.roots += 1;
                    assert_eq!(attr.type_code, attr_type::INDEX_ROOT);
                    assert!(
                        root.node.entries_offset >= 0x10
                            && root.node.entries_offset <= root.node.used_size
                    );
                    assert!(0x10 + root.node.used_size as usize <= value.bytes.len());
                    check_entries(bytes, root.entries(), 0, reached);
                }
                if let Some(label) = attr.volume_name() {
                    assert_eq!(attr.type_code, attr_type::VOLUME_NAME);
                    assert_eq!(label.as_bytes().len(), value.bytes.len() & !1);
                }
                if let Some(tag) = attr.reparse_tag() {
                    assert_eq!(attr.type_code, attr_type::REPARSE_POINT);
                    assert_eq!(tag.to_le_bytes(), value.bytes[..4]);
                }
            }
            AttrBody::NonResident(extent) => {
                assert_eq!(attr.is_compressed(), attr.flags & 0x00FF != 0);
                assert!(attr.length >= 0x40);
                let at = position(bytes, extent.runlist);
                assert!(
                    at >= range.start + 0x40 && at + extent.runlist.len() == range.end,
                    "runlist outside its attribute"
                );
                check_extent(&extent, scratch, reached);
                match attr.attribute_list() {
                    Some(AttributeList::NonResident(list)) => assert_eq!(list, extent),
                    Some(AttributeList::Resident(_)) => {
                        panic!("a non-resident list reported as resident")
                    }
                    None => assert_ne!(attr.type_code, attr_type::ATTRIBUTE_LIST),
                }
                assert!(attr.standard_information().is_none() && attr.file_name().is_none());
                assert!(
                    attr.index_root().is_none()
                        && attr.volume_name().is_none()
                        && attr.reparse_tag().is_none()
                );
            }
        }
    }
    assert_eq!(
        count,
        record.attribute_count(),
        "the count and the walk must agree"
    );
    match record.attr_chain() {
        AttrChain::Complete => assert!(next + 4 <= header.used_size),
        AttrChain::Broken { offset } => assert_eq!(offset, next, "broken where the walk stopped"),
    }
    assert_eq!(record.is_never_used(), !record.is_in_use() && count == 0);
    assert_eq!(record.file_names().count(), names);
    let _ = record.standard_information();
    if let Some(data) = record.find(attr_type::DATA, "") {
        assert!(data.type_code == attr_type::DATA && data.is_unnamed());
    }
}

fn check_block(block: &IndexBlock<'_>, reached: &mut Reached) {
    reached.blocks += 1;
    let size = block.size();
    assert!(size.is_power_of_two() && (512..=65_536).contains(&size));
    assert_eq!(block.bytes().len(), size as usize);
    let node = block.node();
    assert_eq!(node.allocated_size + 0x18, size);
    assert!(
        node.entries_offset >= 0x10
            && node.entries_offset <= node.used_size
            && node.used_size <= node.allocated_size
    );
    check_entries(block.bytes(), block.entries(), 0, reached);
    let _ = (block.vcn(), block.lsn(), block.fixup(), block.owner());
}

/// Runs every public entry point on `input`.
pub fn exercise(input: &[u8], buffer: &mut Vec<u8>, scratch: &mut Vec<Run>, reached: &mut Reached) {
    reached.inputs += 1;

    // Boot sector.
    let boot = parse_boot_sector(input);
    assert!(boot.is_err() || is_boot_sector_candidate(input));
    if let Ok(boot) = boot {
        reached.boots += 1;
        assert!(matches!(boot.bytes_per_sector, 512 | 1024 | 2048 | 4096));
        assert_eq!(
            boot.cluster_size,
            boot.bytes_per_sector * boot.sectors_per_cluster
        );
        assert!(boot.cluster_size.is_power_of_two() && boot.cluster_size <= 2 << 20);
        for size in [boot.mft_record_size, boot.index_block_size] {
            assert!(size.is_power_of_two() && (512..=65_536).contains(&size));
        }
        assert!(boot.total_sectors >= 1 && boot.volume_size() <= i64::MAX as u64);
        assert_eq!(
            boot.volume_size(),
            boot.total_sectors * u64::from(boot.bytes_per_sector)
        );
        assert!(boot.mft_lcn < boot.total_clusters() && boot.mftmirr_lcn < boot.total_clusters());
        assert!(
            boot.mft_offset() < boot.volume_size() && boot.mftmirr_offset() < boot.volume_size()
        );
    }

    // Probes: whatever they accept, the parsers must be able to size.
    for max in [0, 1024, 4096, MAX_RECORD_SIZE, u32::MAX] {
        if let Some(probe) = probe_file_record(input, max) {
            assert!(input.starts_with(b"FILE"));
            assert!(probe.record_size.is_power_of_two() && probe.record_size >= 1024);
            assert!(probe.record_size <= max.min(MAX_RECORD_SIZE));
            assert_eq!(u32::from(probe.usa_count), probe.record_size / 512 + 1);
            assert!(
                probe.usa_offset >= 0x2A
                    && u32::from(probe.usa_offset) + 2 * u32::from(probe.usa_count) <= 510
            );
        }
    }
    if let Some(probe) = probe_index_block(input) {
        assert!(input.starts_with(b"INDX"));
        assert!(probe.block_size.is_power_of_two() && (512..=65_536).contains(&probe.block_size));
        assert_eq!(u32::from(probe.usa_count), probe.block_size / 512 + 1);
        assert!(
            probe.usa_offset >= 0x28
                && u32::from(probe.usa_offset) + 2 * u32::from(probe.usa_count) <= 510
        );
    }

    // Fixups alone: only block tails may change, and nothing on refusal.
    buffer.clear();
    buffer.extend_from_slice(input);
    match apply_fixups(buffer) {
        Ok(status) => {
            for (at, (now, was)) in buffer.iter().zip(input).enumerate() {
                assert!(now == was || at % 512 >= 510, "fixups changed byte {at}");
            }
            if let FixupStatus::Torn {
                first_bad_block,
                bad_blocks,
            } = status
            {
                let blocks = input.len() / 512;
                assert!(
                    bad_blocks >= 1
                        && usize::from(bad_blocks) <= blocks
                        && usize::from(first_bad_block) < blocks
                );
            }
        }
        Err(_) => assert_eq!(&buffer[..], input, "a refused block was modified"),
    }

    // FILE record, in place and from fixed-up bytes.
    buffer.clear();
    buffer.extend_from_slice(input);
    let parsed = parse_file_record(buffer).map(|record| {
        check_record(&record, scratch, reached);
        (
            *record.header(),
            record.fixup(),
            record.attribute_count(),
            record.attr_chain(),
        )
    });
    match parsed {
        Ok((header, _, _, _)) => {
            assert!(
                probe_file_record(input, MAX_RECORD_SIZE).is_some(),
                "parsed without passing the probe"
            );
            let size = header.allocated_size as usize;
            assert_eq!(
                &buffer[size..],
                &input[size..],
                "bytes behind the record were modified"
            );
        }
        Err(error) => {
            // Refused before the fixups, or with them applied to the
            // record's own bytes only.
            let touched = buffer.iter().zip(input).any(|(now, was)| now != was);
            assert!(
                !touched || probe_file_record(input, MAX_RECORD_SIZE).is_some(),
                "{error}: modified without a valid header"
            );
        }
    }
    if let Ok(record) = parse_file_record_fixed(input, FixupStatus::Ok) {
        check_record(&record, scratch, reached);
    }

    // INDX block, both ways.
    buffer.clear();
    buffer.extend_from_slice(input);
    if let Ok(block) = parse_index_block(buffer) {
        assert!(
            probe_index_block(input).is_some(),
            "parsed without passing the probe"
        );
        check_block(&block, reached);
    }
    if let Ok(block) = parse_index_block_fixed(input, FixupStatus::Ok) {
        check_block(&block, reached);
    }

    // The walkers and value decoders on the raw bytes.
    check_runs(input, scratch, reached);
    check_entries(input, IndexEntries::new(input, 0), 0, reached);
    check_attr_list(input, reached);
    if let Some(root) = IndexRoot::parse(input, 7) {
        reached.roots += 1;
        let mut previous = None;
        for entry in root.entries() {
            assert!(entry.offset >= 7 + 0x20 && previous.is_none_or(|p| entry.offset > p));
            previous = Some(entry.offset);
        }
    }
    if let Some(name) = FileName::parse(input) {
        check_file_name(&name, reached);
        position(input, name.name.as_bytes());
    }
    if let Some(name) = FileName::parse_exact(input) {
        assert_eq!(Some(name), FileName::parse(input));
        assert_eq!(input.len(), 0x42 + name.name.as_bytes().len());
    }

    // Names, hash, references, times.
    let name = Utf16Name::from_bytes(input);
    assert_eq!(name.len(), input.len() / 2);
    assert!(name.to_string_lossy().chars().count() <= name.len());
    let _ = (name.eq_str("$I30"), fnv1a64(input));
    if let Some(raw) = input.first_chunk::<8>().map(|b| u64::from_le_bytes(*b)) {
        let reference = FileRef::from_raw(raw);
        assert_eq!(
            FileRef::new(reference.number(), reference.sequence()),
            reference
        );
        assert!(reference.resolves_to(reference.sequence(), true));
        assert!(reference.resolves_to(next_seq(reference.sequence()), false));
        assert_eq!(filetime_to_unix_ms(raw).is_some(), filetime_is_valid(raw));
        assert_eq!(filetime_to_unix_time(raw).is_some(), filetime_is_valid(raw));
        if let Some(ms) = filetime_to_unix_ms(raw) {
            match unix_ms_to_filetime(ms) {
                Some(back) => assert!(back <= raw && raw - back < 10_000),
                // The first millisecond after 1601-01-01 rounds down to
                // FILETIME 0, which is "not set".
                None => assert!(raw < 10_000),
            }
        }
        let _ = unix_ms_to_filetime(raw.cast_signed());
    }
}

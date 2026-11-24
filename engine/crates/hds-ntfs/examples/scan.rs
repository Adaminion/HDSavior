//! Walks every 512-byte offset of a disk image the way the scanner does,
//! prints what the parsers find, and measures how fast they are.
//!
//! ```text
//! cargo run --release -p hds-ntfs --example scan -- IMAGE [--dump N] [--bench]
//! ```
//!
//! * the summary counts boot sectors, `FILE` records and `INDX` blocks by
//!   state, attributes by type, names by namespace, runs and index entries;
//! * `--dump N` also prints the first `N` records and index blocks that are
//!   not never-used records, one line per attribute;
//! * `--bench` replays the records found (at least 200 000 parses) and
//!   reports records per second on one thread, per stage.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::Instant;

use hds_ntfs::{
    attr_type, fnv1a64, is_boot_sector_candidate, parse_boot_sector, parse_file_record,
    parse_index_block, probe_file_record, probe_index_block, AttrBody, AttributeList, FileRecord,
    FixupStatus, IndexBlock, RunlistStatus, WalkEnd, MAX_RECORD_SIZE, SECTOR_SIZE,
};

#[derive(Default)]
struct Stats {
    boot_valid: u64,
    boot_invalid: u64,
    file_magic: u64,
    records: u64,
    records_refused: u64,
    never_used: u64,
    in_use: u64,
    directories: u64,
    extensions: u64,
    unnumbered: u64,
    torn: u64,
    broken_chains: u64,
    malformed_attributes: u64,
    attributes: BTreeMap<u32, u64>,
    standard_information_lengths: BTreeMap<u32, u64>,
    namespaces: [u64; 4],
    resident_data: u64,
    non_resident_data: u64,
    named_streams: u64,
    runs: u64,
    sparse_runs: u64,
    bad_runlists: u64,
    resident_lists: u64,
    non_resident_lists: u64,
    reparse_tags: BTreeMap<u32, u64>,
    end_marker_tails: BTreeMap<u32, u64>,
    indx_magic: u64,
    blocks: u64,
    blocks_refused: u64,
    blocks_torn: u64,
    blocks_broken: u64,
    block_entries: u64,
    foreign_entries: u64,
    root_entries: u64,
}

/// Everything the scanner would look at in one record.
fn visit_record(record: &FileRecord<'_>, stats: &mut Stats) {
    stats.records += 1;
    if record.is_never_used() {
        stats.never_used += 1;
        return;
    }
    stats.in_use += u64::from(record.is_in_use());
    stats.directories += u64::from(record.is_directory());
    stats.extensions += u64::from(record.is_extension());
    stats.unnumbered += u64::from(record.record_number().is_none());
    stats.torn += u64::from(record.fixup().is_torn());
    stats.broken_chains += u64::from(!record.attr_chain().is_complete());
    if record.attr_chain().is_complete() {
        // The 4 bytes behind the end marker: zero from formatters and
        // ntfs-3g, 0x11477982 from the Windows driver.
        let used = record.used_size() as usize;
        if let Some(tail) = used
            .checked_sub(4)
            .and_then(|at| record.bytes().get(at..used))
        {
            let tail = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
            *stats.end_marker_tails.entry(tail).or_default() += 1;
        }
    }
    for attr in record.attributes() {
        *stats.attributes.entry(attr.type_code).or_default() += 1;
        if attr.is_malformed() {
            stats.malformed_attributes += 1;
            continue;
        }
        match attr.type_code {
            attr_type::STANDARD_INFORMATION => {
                if let Some(value) = attr.resident() {
                    *stats
                        .standard_information_lengths
                        .entry(value.len())
                        .or_default() += 1;
                }
                black_box(attr.standard_information());
            }
            attr_type::FILE_NAME => {
                if let Some(name) = attr.file_name() {
                    stats.namespaces[usize::from(name.namespace.as_u8())] += 1;
                    black_box(name.name.as_bytes());
                }
            }
            attr_type::ATTRIBUTE_LIST => match attr.attribute_list() {
                Some(AttributeList::Resident(entries)) => {
                    stats.resident_lists += 1;
                    black_box(entries.count());
                }
                Some(AttributeList::NonResident(_)) => stats.non_resident_lists += 1,
                None => {}
            },
            attr_type::DATA | attr_type::INDEX_ALLOCATION | attr_type::BITMAP => {
                if attr.type_code == attr_type::DATA {
                    stats.named_streams += u64::from(!attr.is_unnamed());
                }
                match attr.body {
                    AttrBody::Resident(value) => {
                        stats.resident_data += u64::from(attr.type_code == attr_type::DATA);
                        black_box((value.offset, value.len()));
                    }
                    AttrBody::NonResident(extent) => {
                        stats.non_resident_data += u64::from(attr.type_code == attr_type::DATA);
                        let mut runs = extent.runs();
                        for run in &mut runs {
                            stats.runs += 1;
                            stats.sparse_runs += u64::from(run.is_sparse());
                            black_box(run);
                        }
                        stats.bad_runlists +=
                            u64::from(runs.status() != Some(RunlistStatus::Complete));
                    }
                    AttrBody::Malformed => {}
                }
            }
            attr_type::INDEX_ROOT => {
                if let Some(root) = attr.index_root() {
                    stats.root_entries += root.entries().count() as u64;
                }
            }
            attr_type::REPARSE_POINT => {
                if let Some(tag) = attr.reparse_tag() {
                    *stats.reparse_tags.entry(tag).or_default() += 1;
                }
            }
            _ => {}
        }
    }
}

fn visit_block(block: &IndexBlock<'_>, stats: &mut Stats) {
    stats.blocks += 1;
    stats.blocks_torn += u64::from(block.fixup().is_torn());
    let mut entries = block.entries();
    for entry in &mut entries {
        stats.block_entries += 1;
        black_box(entry);
    }
    stats.foreign_entries += u64::from(entries.skipped());
    stats.blocks_broken += u64::from(entries.end() != Some(WalkEnd::Complete));
    black_box(block.owner());
}

fn dump_record(offset: usize, hash: u64, record: &FileRecord<'_>) {
    let header = record.header();
    println!(
        "{offset:#012x} FILE #{} seq={} flags={:#06x} links={} base={} used={}/{} lsn={} fixup={:?} chain={:?} hash={hash:016x}",
        header
            .record_number
            .map_or_else(|| "?".to_string(), |number| number.to_string()),
        header.sequence,
        header.flags,
        header.link_count,
        header.base,
        header.used_size,
        header.allocated_size,
        header.lsn,
        record.fixup(),
        record.attr_chain(),
    );
    for attr in record.attributes() {
        print!(
            "      +{:<5} type={:#05x} id={:<3} flags={:#06x} name={:?} ",
            attr.offset, attr.type_code, attr.id, attr.flags, attr.name
        );
        match attr.body {
            AttrBody::Malformed => println!("MALFORMED"),
            AttrBody::Resident(value) => {
                print!("resident +{} len={}", value.offset, value.len());
                if let Some(si) = attr.standard_information() {
                    print!(" times={:?} file_attributes={:#x}", si.times, si.flags);
                }
                if let Some(name) = attr.file_name() {
                    print!(
                        " name={:?} {:?} parent={} file_attributes={:#x} size={}",
                        name.name, name.namespace, name.parent, name.flags, name.real_size
                    );
                }
                if let Some(AttributeList::Resident(entries)) = attr.attribute_list() {
                    for entry in entries {
                        print!(
                            " [{:#x} {:?} vcn={} in {} id={}]",
                            entry.type_code,
                            entry.name,
                            entry.start_vcn,
                            entry.holder,
                            entry.attr_id
                        );
                    }
                }
                if let Some(root) = attr.index_root() {
                    for entry in root.entries() {
                        print!(" [{} {:?}]", entry.child, entry.file_name.name);
                    }
                }
                if let Some(tag) = attr.reparse_tag() {
                    print!(" reparse_tag={tag:#010x}");
                }
                if let Some(label) = attr.volume_name() {
                    print!(" label={label:?}");
                }
                println!();
            }
            AttrBody::NonResident(extent) => {
                print!(
                    "non-resident vcn {}..{} sizes={:?} unit={} total={:?} runs:",
                    extent.start_vcn,
                    extent.last_vcn,
                    extent.sizes(),
                    extent.compression_unit,
                    extent.total_allocated
                );
                let mut runs = extent.runs();
                for run in (&mut runs).take(12) {
                    match run.lcn {
                        Some(lcn) => print!(" {lcn}+{}", run.length),
                        None => print!(" hole+{}", run.length),
                    }
                }
                let more = runs.by_ref().count();
                if more > 0 {
                    print!(" ... ({more} more)");
                }
                println!(" {:?}", runs.status());
            }
        }
    }
}

fn dump_block(offset: usize, block: &IndexBlock<'_>) {
    println!(
        "{offset:#012x} INDX vcn={} size={} lsn={} fixup={:?} owner={:?}",
        block.vcn(),
        block.size(),
        block.lsn(),
        block.fixup(),
        block.owner()
    );
    let mut entries = block.entries();
    for entry in &mut entries {
        println!(
            "      +{:<5} {} {:?} {:?} parent={} file_attributes={:#x}",
            entry.offset,
            entry.child,
            entry.file_name.name,
            entry.file_name.namespace,
            entry.file_name.parent,
            entry.file_name.flags
        );
    }
    println!(
        "      end={:?} skipped={}",
        entries.end(),
        entries.skipped()
    );
}

fn rate(count: u64, seconds: f64) -> String {
    format!("{:>12.0} records/s", count as f64 / seconds)
}

/// What the scanner takes from a record, and nothing else: every header
/// field, every attribute decoded according to its type, every run. The
/// values are folded into one number so that nothing is optimised away.
fn touch_record(record: &FileRecord<'_>) -> u64 {
    let header = record.header();
    let mut sum = header.lsn
        ^ u64::from(header.sequence)
        ^ u64::from(header.flags)
        ^ header.base.raw()
        ^ u64::from(header.record_number.unwrap_or(0))
        ^ u64::from(record.fixup().is_torn());
    for attr in record.attributes() {
        sum = sum.wrapping_add(u64::from(attr.id) ^ u64::from(attr.flags));
        match attr.type_code {
            attr_type::STANDARD_INFORMATION => {
                if let Some(si) = attr.standard_information() {
                    sum ^= si.times.created ^ si.times.mft_changed ^ u64::from(si.flags);
                }
            }
            attr_type::FILE_NAME => {
                if let Some(name) = attr.file_name() {
                    sum ^= name.parent.raw() ^ u64::from(name.flags) ^ name.name.len() as u64;
                    sum = sum.wrapping_add(u64::from(
                        name.name.as_bytes().first().copied().unwrap_or(0),
                    ));
                }
            }
            attr_type::ATTRIBUTE_LIST => {
                if let Some(AttributeList::Resident(entries)) = attr.attribute_list() {
                    for entry in entries {
                        sum ^= entry.holder.raw() ^ entry.start_vcn;
                    }
                }
            }
            attr_type::INDEX_ROOT => {
                if let Some(root) = attr.index_root() {
                    for entry in root.entries() {
                        sum ^= entry.child.raw() ^ entry.file_name.times.mft_changed;
                    }
                }
            }
            attr_type::REPARSE_POINT => sum ^= u64::from(attr.reparse_tag().unwrap_or(0)),
            _ => {}
        }
        match attr.body {
            AttrBody::Resident(value) => sum ^= u64::from(value.offset) ^ u64::from(value.len()),
            AttrBody::NonResident(extent) => {
                sum ^= extent.start_vcn ^ extent.real_size ^ extent.initialized_size;
                let mut runs = extent.runs();
                for run in &mut runs {
                    sum = sum.wrapping_add(run.length ^ run.lcn.unwrap_or(u64::MAX));
                }
                sum ^= u64::from(runs.status() == Some(RunlistStatus::Complete));
            }
            AttrBody::Malformed => {}
        }
    }
    sum
}

/// Replays the records until at least 200 000 have been parsed per stage.
fn bench(records: &[Vec<u8>]) {
    if records.is_empty() {
        println!("no records to measure");
        return;
    }
    let rounds = 200_000_usize.div_ceil(records.len()).max(3);
    let total = (rounds * records.len()) as u64;
    let bytes: usize = records.iter().map(Vec::len).sum();
    println!(
        "bench: {} records ({} bytes on average), {rounds} rounds, one thread",
        records.len(),
        bytes / records.len()
    );
    let mut scratch = vec![0u8; MAX_RECORD_SIZE as usize];

    let start = Instant::now();
    let mut hits = 0u64;
    for _ in 0..rounds {
        for raw in records {
            hits += u64::from(probe_file_record(black_box(raw), MAX_RECORD_SIZE).is_some());
        }
    }
    println!(
        "  probe only                        {}",
        rate(total, start.elapsed().as_secs_f64())
    );
    black_box(hits);

    let start = Instant::now();
    let mut sum = 0u64;
    for _ in 0..rounds {
        for raw in records {
            sum ^= fnv1a64(black_box(raw));
        }
    }
    println!(
        "  fnv1a64 only                      {}",
        rate(total, start.elapsed().as_secs_f64())
    );
    black_box(sum);

    let start = Instant::now();
    for _ in 0..rounds {
        for raw in records {
            let record = &mut scratch[..raw.len()];
            record.copy_from_slice(raw);
            black_box(record);
        }
    }
    let copy = start.elapsed().as_secs_f64();
    println!("  copy only                         {}", rate(total, copy));

    let start = Instant::now();
    let mut parsed = 0u64;
    for _ in 0..rounds {
        for raw in records {
            let record = &mut scratch[..raw.len()];
            record.copy_from_slice(raw);
            if let Ok(record) = parse_file_record(record) {
                parsed += u64::from(record.attribute_count());
            }
        }
    }
    println!(
        "  copy + fixups + header + count    {}",
        rate(total, start.elapsed().as_secs_f64())
    );
    black_box(parsed);

    let start = Instant::now();
    let mut sum = 0u64;
    for _ in 0..rounds {
        for raw in records {
            let record = &mut scratch[..raw.len()];
            record.copy_from_slice(raw);
            if let Ok(record) = parse_file_record(record) {
                sum ^= touch_record(&record);
            }
        }
    }
    println!(
        "  ... + every attribute, name, run  {}",
        rate(total, start.elapsed().as_secs_f64())
    );
    black_box(sum);

    let start = Instant::now();
    let mut sum = 0u64;
    for _ in 0..rounds {
        for raw in records {
            if probe_file_record(raw, MAX_RECORD_SIZE).is_none() {
                continue;
            }
            sum ^= fnv1a64(raw);
            let record = &mut scratch[..raw.len()];
            record.copy_from_slice(raw);
            if let Ok(record) = parse_file_record(record) {
                sum ^= touch_record(&record);
            }
        }
    }
    println!(
        "  whole scanner step (with the hash) {}",
        rate(total, start.elapsed().as_secs_f64())
    );
    black_box(sum);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: scan IMAGE [--dump N] [--bench]");
        std::process::exit(2);
    };
    let mut dump = 0usize;
    let mut run_bench = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dump" => dump = args.next().and_then(|n| n.parse().ok()).unwrap_or(20),
            "--bench" => run_bench = true,
            other => {
                eprintln!("unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }
    let image = match std::fs::read(&path) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            std::process::exit(1);
        }
    };

    let mut stats = Stats::default();
    let mut scratch = vec![0u8; MAX_RECORD_SIZE as usize];
    let mut found: Vec<Vec<u8>> = Vec::new();
    let mut hash_sum = 0u64;
    let start = Instant::now();
    for (index, sector) in image.chunks_exact(SECTOR_SIZE).enumerate() {
        let offset = index * SECTOR_SIZE;
        if is_boot_sector_candidate(sector) {
            match parse_boot_sector(sector) {
                Ok(boot) => {
                    stats.boot_valid += 1;
                    if dump > 0 {
                        println!("{offset:#012x} BOOT {boot:?}");
                    }
                }
                Err(_) => stats.boot_invalid += 1,
            }
        }
        if sector.starts_with(b"FILE") {
            stats.file_magic += 1;
            let raw = probe_file_record(sector, MAX_RECORD_SIZE)
                .and_then(|probe| image.get(offset..offset + probe.record_size as usize));
            let Some(raw) = raw else {
                stats.records_refused += 1;
                continue;
            };
            let hash = fnv1a64(raw);
            hash_sum ^= hash;
            let record = &mut scratch[..raw.len()];
            record.copy_from_slice(raw);
            match parse_file_record(record) {
                Ok(record) => {
                    visit_record(&record, &mut stats);
                    if run_bench {
                        found.push(raw.to_vec());
                    }
                    if dump > 0 && !record.is_never_used() {
                        dump -= 1;
                        dump_record(offset, hash, &record);
                    }
                }
                Err(_) => stats.records_refused += 1,
            }
        } else if sector.starts_with(b"INDX") {
            stats.indx_magic += 1;
            let raw = probe_index_block(sector)
                .and_then(|probe| image.get(offset..offset + probe.block_size as usize));
            let Some(raw) = raw else {
                stats.blocks_refused += 1;
                continue;
            };
            let block = &mut scratch[..raw.len()];
            block.copy_from_slice(raw);
            match parse_index_block(block) {
                Ok(block) => {
                    visit_block(&block, &mut stats);
                    if dump > 0 {
                        dump -= 1;
                        dump_block(offset, &block);
                    }
                }
                Err(_) => stats.blocks_refused += 1,
            }
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    black_box(hash_sum);

    println!(
        "{path}: {} bytes scanned in {seconds:.3} s ({:.0} MiB/s)",
        image.len(),
        image.len() as f64 / seconds / 1048576.0
    );
    println!(
        "boot sectors: {} valid, {} refused",
        stats.boot_valid, stats.boot_invalid
    );
    println!(
        "FILE: {} magics, {} parsed, {} refused; never used {}, in use {}, directories {}, extension records {}, without number {}, torn {}, broken chains {}, malformed attributes {}",
        stats.file_magic,
        stats.records,
        stats.records_refused,
        stats.never_used,
        stats.in_use,
        stats.directories,
        stats.extensions,
        stats.unnumbered,
        stats.torn,
        stats.broken_chains,
        stats.malformed_attributes
    );
    println!("attributes by type: {:#x?}", stats.attributes);
    println!(
        "$STANDARD_INFORMATION value lengths: {:?}",
        stats.standard_information_lengths
    );
    println!(
        "names by namespace: POSIX {}, WIN32 {}, DOS {}, WIN32+DOS {}",
        stats.namespaces[0], stats.namespaces[1], stats.namespaces[2], stats.namespaces[3]
    );
    println!(
        "$DATA: {} resident, {} non-resident, {} named; runs {} ({} sparse), faulty runlists {}",
        stats.resident_data,
        stats.non_resident_data,
        stats.named_streams,
        stats.runs,
        stats.sparse_runs,
        stats.bad_runlists
    );
    println!(
        "$ATTRIBUTE_LIST: {} resident, {} non-resident; reparse tags {:#x?}; bytes behind the end marker {:#x?}",
        stats.resident_lists, stats.non_resident_lists, stats.reparse_tags, stats.end_marker_tails
    );
    println!(
        "INDX: {} magics, {} parsed, {} refused, {} torn, {} without end entry; {} file-name entries, {} other keys; {} entries in index roots",
        stats.indx_magic,
        stats.blocks,
        stats.blocks_refused,
        stats.blocks_torn,
        stats.blocks_broken,
        stats.block_entries,
        stats.foreign_entries,
        stats.root_entries
    );
    if stats.torn > 0 || stats.blocks_torn > 0 {
        println!(
            "note: {:?} is what a torn structure reports",
            FixupStatus::Torn {
                first_bad_block: 1,
                bad_blocks: 1
            }
        );
    }
    if run_bench {
        bench(&found);
    }
}

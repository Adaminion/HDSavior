//! Robustness: arbitrary and hostile input must never make a parser panic,
//! read outside its buffer, overflow or run on forever.
//!
//! No external fuzzer: the inputs come from a fixed-seed generator, so every
//! run feeds the same bytes and a failure names the case that caused it.
//! Each input goes through `common::exercise`, which calls every public
//! entry point and checks the promises that hold for any bytes. Each test
//! runs under a watchdog, so that a walk that stops making progress fails
//! the test instead of hanging it.
//!
//! * random buffers, plain and with a forced valid header;
//! * mutations of the real structures in `tests/data/structures.bin` and
//!   of hand-built ones for what no fixture contains: random bit flips,
//!   truncations, 0x00 and 0xFF fills, swapped fields, huge lengths,
//!   damage aimed at attribute and entry headers;
//! * every single-bit flip of every one of these structures;
//! * every byte value at every header position of four real structures;
//! * every runlist of up to three bytes;
//! * with `HDS_FIXTURES` set: every sector of every fixture image, as it
//!   is and dressed up as a record and as an index block.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use common::build::{
    attr_list_entry, index_block, index_entry, index_root, last_entry, mref, protect, resident,
    standard_information, Boot, Name, NonResident, RecordBuilder, WINDOWS_END_TAIL,
};
use common::exercise::{exercise, Reached};
use common::rng::Rng;
use hds_ntfs::{
    parse_file_record, parse_index_block, probe_file_record, probe_index_block, AttrBody, Run,
    Runs, MAX_RECORD_SIZE,
};

/// Generous: the slowest of these tests takes a few seconds.
const BUDGET: Duration = Duration::from_secs(600);

/// Runs `body` on another thread and fails if it does not come back in
/// time. `body` reports the case it is working on through the counter, so
/// that a timeout or a panic can name it.
fn guarded<T: Send + 'static>(
    name: &str,
    body: impl FnOnce(&AtomicU64) -> T + Send + 'static,
) -> T {
    let case = Arc::new(AtomicU64::new(0));
    let (sender, receiver) = mpsc::channel();
    let worker = {
        let case = Arc::clone(&case);
        std::thread::spawn(move || {
            let result = body(&case);
            let _ = sender.send(());
            result
        })
    };
    match receiver.recv_timeout(BUDGET) {
        Ok(()) => worker.join().expect("worker finished"),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "{name}: no result after {BUDGET:?}; a parser does not terminate on case {}",
            case.load(Ordering::Relaxed)
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let failed_at = case.load(Ordering::Relaxed);
            match worker.join() {
                Ok(_) => unreachable!("the worker ended without reporting"),
                Err(panic) => {
                    eprintln!("{name}: failed on case {failed_at}");
                    std::panic::resume_unwind(panic)
                }
            }
        }
    }
}

/// The real structures of `tests/data/structures.bin`.
fn samples() -> Vec<Vec<u8>> {
    let image = std::fs::read(common::data_dir().join("structures.bin"))
        .expect("tests/data/structures.bin");
    let mut samples = Vec::new();
    let mut offset = 0;
    while offset < image.len() {
        let sector = &image[offset..offset + 512];
        let size = if let Some(probe) = probe_file_record(sector, MAX_RECORD_SIZE) {
            probe.record_size as usize
        } else if let Some(probe) = probe_index_block(sector) {
            probe.block_size as usize
        } else {
            512 // a boot sector
        };
        samples.push(image[offset..offset + size].to_vec());
        offset += size;
    }
    assert_eq!(samples.len(), 30);
    samples
}

/// Structures that no fixture contains (ntfs-3g does not write them),
/// built by hand: a resident attribute list, a reparse point, an index
/// root with sub-node entries, the old record layout, index blocks with
/// foreign keys and of other sizes, a boot sector with a shift-coded
/// cluster size.
fn hand_built_samples() -> Vec<Vec<u8>> {
    let times = [0x01D9_0000_0000_0001u64; 4];
    let si = resident(0x10, "", 0, 0, &standard_information(times, 0x20, 72));
    let name = |text: &str, namespace: u8| {
        resident(
            0x30,
            "",
            0,
            1,
            &Name::new(mref(5, 5), text, namespace).value(),
        )
    };
    let key = |text: &str| Name::new(mref(64, 1), text, 1).value();

    let mut list = Vec::new();
    list.extend(attr_list_entry(0x10, "", 0, mref(100, 1), 0));
    list.extend(attr_list_entry(0x30, "", 0, mref(101, 1), 2));
    list.extend(attr_list_entry(0x80, "", 0, mref(102, 1), 0));
    list.extend(attr_list_entry(0x80, "", 468, mref(103, 1), 0));
    list.extend(attr_list_entry(0x80, "stream", 0, mref(103, 1), 1));
    list.extend(attr_list_entry(0xA0, "$I30", 134, mref(104, 9), 3));
    let with_list = RecordBuilder::new(1024)
        .number(100)
        .attr(&si)
        .attr(&resident(0x20, "", 0, 1, &list))
        .attr(&name("listed.bin", 0))
        .attr(
            &NonResident {
                last_vcn: 467,
                allocated: 1_916_928,
                real: 1_900_000,
                initialized: 1_900_000,
                runlist: &[
                    0x21, 0x40, 0x00, 0x10, 0x01, 0x20, 0x31, 0x7F, 0xF0, 0xFF, 0xFF, 0x00,
                ],
                ..NonResident::default()
            }
            .build(),
        )
        .finish_on_disk(0x0101);

    let mut reparse = vec![0x0C, 0x00, 0x00, 0xA0, 0x20, 0x00, 0x00, 0x00];
    reparse.extend([0x41; 0x20]);
    let link = RecordBuilder::new(1024)
        .number(42)
        .end_tail(WINDOWS_END_TAIL)
        .attr(&si)
        .attr(&name("A_LINK~1", 2))
        .attr(&name("a_link with a long name", 1))
        .attr(&resident(0x80, "", 0, 3, b""))
        .attr(&resident(0x80, "WofCompressedData", 0, 4, &[0x5A; 40]))
        .attr(&resident(0xC0, "", 0, 5, &reparse))
        .finish_on_disk(0xBEEF);

    let root_entries = vec![
        index_entry(mref(70, 1), &key("alpha"), 0, Some(0)),
        index_entry(mref(71, 1), &key("ALPHA~1"), 0, Some(1)),
        index_entry(0x0014_0018, &[1, 2, 3, 4], 0, None),
        index_entry(mref(72, 3), &key("omega"), 0, Some(2)),
        last_entry(Some(3)),
    ];
    let directory = RecordBuilder::new(4096)
        .flags(3)
        .number(64)
        .attr(&si)
        .attr(&name("dir", 3))
        .attr(&resident(
            0x90,
            "$I30",
            0,
            2,
            &index_root(4096, 1, 1, &root_entries),
        ))
        .attr(
            &NonResident {
                type_code: 0xA0,
                name: "$I30",
                last_vcn: 3,
                allocated: 16_384,
                real: 16_384,
                initialized: 16_384,
                runlist: &[0x21, 0x04, 0x0D, 0x10, 0x00],
                ..NonResident::default()
            }
            .build(),
        )
        .attr(&resident(0xB0, "$I30", 0, 4, &[0x0F, 0, 0, 0, 0, 0, 0, 0]))
        .finish_on_disk(0x7777);

    let old_layout = RecordBuilder::old_layout(1024)
        .attr(&si)
        .attr(&name("NT4FILE.TXT", 3))
        .attr(
            &NonResident {
                flags: 0x8000,
                compression_unit: 4,
                last_vcn: 31,
                allocated: 131_072,
                real: 100_000,
                initialized: 50_000,
                total_allocated: Some(65_536),
                runlist: &[0x11, 0x10, 0x64, 0x01, 0x10, 0x00],
                ..NonResident::default()
            }
            .build(),
        )
        .finish_on_disk(3);

    let mut blocks = Vec::new();
    for size in [512usize, 4096, 16_384] {
        let mut entries: Vec<Vec<u8>> = Vec::new();
        let mut room = size - 0x100;
        let mut index = 0u64;
        while room > 0x100 {
            let entry = match index % 5 {
                3 => index_entry(0x0014_0018, &[index as u8; 16], 0, None),
                4 => index_entry(
                    mref(200 + index, 1),
                    &key(&format!("sub-{index}")),
                    0,
                    Some(index),
                ),
                _ => index_entry(
                    mref(200 + index, 1),
                    &key(&format!("entry-{index:04}.dat")),
                    0,
                    None,
                ),
            };
            room -= entry.len();
            entries.push(entry);
            index += 1;
        }
        entries.push(last_entry(None));
        let mut block = index_block(size, 8, 99, 1, &entries);
        protect(&mut block, 0x4242);
        blocks.push(block);
    }

    let boot = Boot {
        bytes_per_sector: 4096,
        sectors_per_cluster_raw: 0xF7,
        total_sectors: 1 << 30,
        mft_lcn: 2,
        mftmirr_lcn: 1,
        record_size_raw: 0xF4,
        index_size_raw: 0xF4,
        ..Boot::default()
    }
    .build();

    let mut samples = vec![with_list, link, directory, old_layout];
    samples.extend(blocks);
    samples.push(boot);
    samples
}

/// Offsets inside a sample where a header field lives that the parsers
/// act on: the structure header, every attribute header and every index
/// entry header. Found with the crate itself on the undamaged sample.
fn field_offsets(sample: &[u8]) -> Vec<usize> {
    let mut offsets: Vec<usize> = (0..0x48.min(sample.len())).collect();
    let mut copy = sample.to_vec();
    if let Ok(record) = parse_file_record(&mut copy) {
        for attr in record.attributes() {
            let at = attr.offset as usize;
            offsets.extend(at..at + 0x18);
            match attr.body {
                AttrBody::NonResident(_) => offsets.extend(at + 0x18..at + 0x48),
                AttrBody::Resident(value) => {
                    // The start of the value: index roots, names and
                    // attribute lists have their own headers there.
                    let start = value.offset as usize;
                    offsets.extend(start..start + value.bytes.len().min(0x60));
                    if let Some(root) = attr.index_root() {
                        for entry in root.entries() {
                            offsets.extend(entry.offset as usize..entry.offset as usize + 0x18);
                        }
                    }
                }
                AttrBody::Malformed => {}
            }
        }
        let end = record.used_size() as usize;
        offsets.extend(end.saturating_sub(8)..end);
    }
    let mut copy = sample.to_vec();
    if let Ok(block) = parse_index_block(&mut copy) {
        for entry in block.entries() {
            offsets.extend(entry.offset as usize..entry.offset as usize + 0x18);
            offsets.extend(entry.offset as usize + 0x50..entry.offset as usize + 0x54);
        }
    }
    offsets.retain(|offset| *offset < sample.len());
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

const HUGE: [u64; 12] = [
    0,
    1,
    0x7F,
    0x80,
    0xFF,
    0x7FFF,
    0xFFFF,
    0x7FFF_FFFF,
    0x8000_0000,
    0xFFFF_FFFF,
    0x7FFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFF,
];

/// Applies one random mutation to `bytes`.
fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>, fields: &[usize]) {
    if bytes.is_empty() {
        return;
    }
    let len = bytes.len();
    // Half of the damage is aimed at header fields (those that an earlier
    // truncation has left in place).
    let target = |rng: &mut Rng| {
        let field = if fields.is_empty() {
            len
        } else {
            *rng.pick(fields)
        };
        if field < len && rng.one_in(2) {
            field
        } else {
            rng.below(len)
        }
    };
    match rng.below(12) {
        0 => {
            let at = target(rng);
            bytes[at] ^= 1 << rng.below(8);
        }
        1 => {
            let at = target(rng);
            bytes[at] = rng.byte();
        }
        2 | 3 => {
            // A value that is too large (or zero) in a 1-, 2-, 4- or
            // 8-byte field.
            let at = target(rng);
            let width = [1usize, 2, 4, 8][rng.below(4)];
            let value = rng.pick(&HUGE).to_le_bytes();
            for (dst, src) in bytes[at..].iter_mut().zip(&value[..width]) {
                *dst = *src;
            }
        }
        4 => {
            // A slightly wrong value: off by a little in a 2-byte field.
            let at = target(rng);
            if at + 2 <= len {
                let value = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
                let delta = [1u16, 2, 7, 8, 16, 0x18, 0x40][rng.below(7)];
                let value = if rng.one_in(2) {
                    value.wrapping_add(delta)
                } else {
                    value.wrapping_sub(delta)
                };
                bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
            }
        }
        5 => {
            // Fill a range with 0x00.
            let at = target(rng);
            let end = (at + rng.range(1, 600)).min(len);
            bytes[at..end].fill(0x00);
        }
        6 => {
            // Fill a range with 0xFF.
            let at = target(rng);
            let end = (at + rng.range(1, 600)).min(len);
            bytes[at..end].fill(0xFF);
        }
        7 => {
            // Fill a range with noise.
            let at = target(rng);
            let end = (at + rng.range(1, 64)).min(len);
            rng.fill(&mut bytes[at..end]);
        }
        8 => {
            // Swap two fields of the same width.
            let width = [1usize, 2, 4, 8][rng.below(4)];
            let (a, b) = (target(rng), target(rng));
            if a + width <= len && b + width <= len && (a + width <= b || b + width <= a) {
                for i in 0..width {
                    bytes.swap(a + i, b + i);
                }
            }
        }
        9 => {
            // Copy a range over another place: structures shifted or
            // overlapping.
            let from = rng.below(len);
            let to = target(rng);
            let count = rng.range(1, 256).min(len - from).min(len - to);
            bytes.copy_within(from..from + count, to);
        }
        10 => {
            // Truncate.
            let keep = if rng.one_in(2) {
                rng.below(len)
            } else {
                len - rng.range(1, 16).min(len)
            };
            bytes.truncate(keep);
        }
        _ => {
            // Whole 512-byte block replaced: a torn or half-overwritten
            // structure.
            let block = rng.below(len.div_ceil(512));
            let range = block * 512..((block + 1) * 512).min(len);
            match rng.below(3) {
                0 => bytes[range].fill(0),
                1 => bytes[range].fill(0xFF),
                _ => rng.fill(&mut bytes[range]),
            }
        }
    }
}

#[test]
fn random_buffers() {
    let reached = guarded("random_buffers", |case| {
        let mut rng = Rng::new(0x4844_5361_7669_6F72);
        let (mut buffer, mut scratch, mut reached) = (Vec::new(), Vec::new(), Reached::default());
        let mut input = Vec::new();
        for index in 0..200_000u64 {
            case.store(index, Ordering::Relaxed);
            let len = match rng.below(8) {
                0 => rng.below(64),
                1 => 512,
                2 => 1024,
                3 => 4096,
                _ => rng.below(2100),
            };
            input.resize(len, 0);
            rng.fill(&mut input);
            // Mostly noise is not what a disk holds: thin some inputs out.
            if rng.one_in(3) {
                for byte in input.iter_mut() {
                    if rng.below(4) != 0 {
                        *byte = 0;
                    }
                }
            }
            exercise(&input, &mut buffer, &mut scratch, &mut reached);
        }
        reached
    });
    eprintln!("random_buffers: {reached:?}");
    assert_eq!(reached.inputs, 200_000);
}

/// A random buffer that starts like a record or an index block, so that
/// the parsers get past their header checks.
fn dressed(rng: &mut Rng, input: &mut Vec<u8>) {
    let size = [512usize, 1024, 1024, 1024, 2048, 4096, 4096, 8192][rng.below(8)];
    input.resize(size, 0);
    rng.fill(input);
    // Sparse noise makes small numbers, which is what lengths are.
    let density = [1usize, 2, 4, 16][rng.below(4)];
    for byte in input.iter_mut() {
        if rng.below(density) != 0 {
            *byte = if rng.one_in(8) {
                [0x10u8, 0x18, 0x20, 0x30, 0x40, 0x48, 0x80, 0xFF][rng.below(8)]
            } else {
                0
            };
        }
    }
    let count = (size / 512 + 1) as u16;
    let record = rng.one_in(2) && size >= 1024;
    input[0..4].copy_from_slice(if record { b"FILE" } else { b"INDX" });
    let usa = if record {
        [0x30u16, 0x2A][rng.below(2)]
    } else {
        0x28
    };
    input[4..6].copy_from_slice(&usa.to_le_bytes());
    input[6..8].copy_from_slice(&count.to_le_bytes());
    let first = ((usize::from(usa) + 2 * usize::from(count) + 7) & !7) as u32;
    if record {
        input[0x14..0x16].copy_from_slice(&(first as u16).to_le_bytes());
        let used = if rng.one_in(4) {
            rng.range(first as usize, size) as u32
        } else {
            size as u32
        };
        input[0x18..0x1C].copy_from_slice(&used.to_le_bytes());
        input[0x1C..0x20].copy_from_slice(&(size as u32).to_le_bytes());
        // Seed a chain of plausible attribute headers.
        let mut at = first as usize;
        while at + 0x48 < size && !rng.one_in(6) {
            let length = (rng.range(3, 30) * 8).min(size - at);
            let type_code = [0x10u32, 0x20, 0x30, 0x60, 0x80, 0x90, 0xA0, 0xB0, 0xC0][rng.below(9)];
            input[at..at + 4].copy_from_slice(&type_code.to_le_bytes());
            input[at + 4..at + 8].copy_from_slice(&(length as u32).to_le_bytes());
            let non_resident = rng.one_in(2) && length >= 0x48;
            input[at + 8] = u8::from(non_resident);
            input[at + 9] = if rng.one_in(3) { rng.below(6) as u8 } else { 0 };
            // Most of the time the header points where it should, so that
            // the noise behind it is read as a value or a runlist.
            if !rng.one_in(4) {
                let header = if non_resident {
                    [0x40u16, 0x48][rng.below(2)]
                } else {
                    0x18
                };
                input[at + 10..at + 12].copy_from_slice(&header.to_le_bytes());
                let content = header + 2 * u16::from(input[at + 9]);
                if non_resident {
                    input[at + 0x20..at + 0x22].copy_from_slice(&content.to_le_bytes());
                } else {
                    let room = (length as u32).saturating_sub(u32::from(content));
                    let value = if rng.one_in(8) {
                        room
                    } else {
                        rng.below(room as usize + 1) as u32
                    };
                    input[at + 0x10..at + 0x14].copy_from_slice(&value.to_le_bytes());
                    input[at + 0x14..at + 0x16].copy_from_slice(&content.to_le_bytes());
                }
            }
            at += length;
        }
        if at + 4 <= size && !rng.one_in(4) {
            input[at..at + 4].fill(0xFF);
        }
    } else {
        input[0x18..0x1C].copy_from_slice(&(first - 0x18).to_le_bytes());
        let used = if rng.one_in(4) {
            rng.below(size) as u32
        } else {
            size as u32 - 0x18
        };
        input[0x1C..0x20].copy_from_slice(&used.to_le_bytes());
        input[0x20..0x24].copy_from_slice(&(size as u32 - 0x18).to_le_bytes());
        // Seed a chain of plausible entry headers.
        let mut at = first as usize;
        while at + 0x60 < size && !rng.one_in(8) {
            let length = (rng.range(2, 20) * 8).min(size - at);
            input[at + 8..at + 10].copy_from_slice(&(length as u16).to_le_bytes());
            let key = if rng.one_in(2) {
                (length as u16).saturating_sub(0x10 + rng.below(8) as u16)
            } else {
                rng.below(200) as u16
            };
            input[at + 10..at + 12].copy_from_slice(&key.to_le_bytes());
            input[at + 12..at + 14].copy_from_slice(&(rng.below(4) as u16).to_le_bytes());
            if at + 0x52 < size {
                input[at + 0x50] = ((key as usize).saturating_sub(0x42) / 2) as u8;
                input[at + 0x51] = rng.below(5) as u8;
            }
            at += length;
        }
    }
}

#[test]
fn random_buffers_with_a_valid_header() {
    let reached = guarded("random_buffers_with_a_valid_header", |case| {
        let mut rng = Rng::new(0x0123_4567_89AB_CDEF);
        let (mut buffer, mut scratch, mut reached) = (Vec::new(), Vec::new(), Reached::default());
        let mut input = Vec::new();
        for index in 0..200_000u64 {
            case.store(index, Ordering::Relaxed);
            dressed(&mut rng, &mut input);
            exercise(&input, &mut buffer, &mut scratch, &mut reached);
        }
        reached
    });
    eprintln!("random_buffers_with_a_valid_header: {reached:?}");
    // The header checks were passed and the walks ran over the noise.
    assert!(reached.records > 50_000, "{reached:?}");
    assert!(reached.blocks > 50_000, "{reached:?}");
    assert!(
        reached.attributes > 100_000 && reached.malformed > 10_000,
        "{reached:?}"
    );
    assert!(reached.attributes > 2 * reached.malformed, "{reached:?}");
    assert!(
        reached.extents > 50_000 && reached.entries > 1_000,
        "{reached:?}"
    );
}

#[test]
fn mutated_structures() {
    let reached = guarded("mutated_structures", |case| {
        let mut samples = samples();
        samples.extend(hand_built_samples());
        assert_eq!(samples.len(), 38);
        let fields: Vec<Vec<usize>> = samples.iter().map(|sample| field_offsets(sample)).collect();
        let mut rng = Rng::new(0x6E74_6673_2D66_757A);
        let (mut buffer, mut scratch, mut reached) = (Vec::new(), Vec::new(), Reached::default());
        let mut input = Vec::new();
        let per_sample = 12_000u64;
        for (which, sample) in samples.iter().enumerate() {
            for round in 0..per_sample {
                case.store(which as u64 * 1_000_000 + round, Ordering::Relaxed);
                input.clear();
                input.extend_from_slice(sample);
                // One to four mutations on top of each other.
                for _ in 0..rng.range(1, 4) {
                    mutate(&mut rng, &mut input, &fields[which]);
                }
                exercise(&input, &mut buffer, &mut scratch, &mut reached);
            }
        }
        reached
    });
    eprintln!("mutated_structures: {reached:?}");
    assert_eq!(reached.inputs, 38 * 12_000);
    assert!(reached.list_entries > 50_000, "{reached:?}");
    // Most mutants are still parsed, deep into their attributes.
    assert!(reached.records > 150_000, "{reached:?}");
    assert!(reached.blocks > 20_000, "{reached:?}");
    assert!(reached.malformed > 5_000, "{reached:?}");
    assert!(
        reached.runs > 500_000 && reached.entries > 100_000,
        "{reached:?}"
    );
}

#[test]
fn every_single_bit_flip_of_every_structure() {
    let reached = guarded("every_single_bit_flip_of_every_structure", |case| {
        let (mut buffer, mut scratch, mut reached) = (Vec::new(), Vec::new(), Reached::default());
        let mut all = samples();
        all.extend(hand_built_samples());
        for (which, sample) in all.iter().enumerate() {
            let mut input = sample.clone();
            for bit in 0..sample.len() * 8 {
                case.store(which as u64 * 1_000_000 + bit as u64, Ordering::Relaxed);
                input[bit / 8] ^= 1 << (bit % 8);
                exercise(&input, &mut buffer, &mut scratch, &mut reached);
                input[bit / 8] ^= 1 << (bit % 8);
            }
            assert_eq!(&input, sample);
        }
        reached
    });
    eprintln!("every_single_bit_flip_of_every_structure: {reached:?}");
    // 47 616 bytes of real structures, 28 672 bytes built by hand.
    assert_eq!(reached.inputs, (47_616 + 28_672) * 8);
}

#[test]
fn every_byte_value_at_every_header_position() {
    let reached = guarded("every_byte_value_at_every_header_position", |case| {
        let samples = samples();
        // A 1 KiB directory record, the full extension record, a 4 KiB
        // record, an index block and a boot sector.
        let chosen = [7usize, 18, 21, 23, 27];
        assert_eq!(samples[7].len(), 1024);
        assert_eq!(samples[21].len(), 4096);
        assert!(samples[23].starts_with(b"INDX") && samples[27].len() == 512);
        let (mut buffer, mut scratch, mut reached) = (Vec::new(), Vec::new(), Reached::default());
        for which in chosen {
            let sample = &samples[which];
            let positions = field_offsets(sample);
            let mut input = sample.clone();
            for position in positions {
                let original = input[position];
                for value in 0..=255u8 {
                    if value == original {
                        continue;
                    }
                    case.store(
                        (which as u64) << 32 | (position as u64) << 8 | u64::from(value),
                        Ordering::Relaxed,
                    );
                    input[position] = value;
                    exercise(&input, &mut buffer, &mut scratch, &mut reached);
                }
                input[position] = original;
            }
        }
        reached
    });
    eprintln!("every_byte_value_at_every_header_position: {reached:?}");
    assert!(reached.inputs > 300_000, "{reached:?}");
}

#[test]
fn every_runlist_of_up_to_three_bytes() {
    let (count, complete) = guarded("every_runlist_of_up_to_three_bytes", |case| {
        let mut count = 0u64;
        let mut complete = 0u64;
        let mut check = |bytes: &[u8]| {
            let mut runs = Runs::new(bytes);
            let mut produced = 0usize;
            let mut clusters = 0u64;
            for run in &mut runs {
                produced += 1;
                assert!(run.length >= 1 && run.length <= i64::MAX as u64);
                assert!(run
                    .lcn
                    .is_none_or(|lcn| lcn + run.length <= i64::MAX as u64));
                clusters += run.length;
            }
            assert!(produced <= bytes.len() / 2);
            assert_eq!(runs.clusters(), clusters);
            assert!(runs.consumed() <= bytes.len());
            count += 1;
            complete += u64::from(runs.status().unwrap().is_complete());
        };
        check(&[]);
        for a in 0..=255u8 {
            check(&[a]);
            for b in 0..=255u8 {
                case.store(u64::from(a) << 8 | u64::from(b), Ordering::Relaxed);
                check(&[a, b]);
                for c in 0..=255u8 {
                    check(&[a, b, c]);
                }
            }
        }
        (count, complete)
    });
    assert_eq!(count, 1 + 256 + 65_536 + 16_777_216);
    // Complete lists: those starting with the terminator, and "one hole
    // then the terminator" (header 0x01, a length of 1..=127, 0x00).
    assert_eq!(complete, 1 + 256 + 65_536 + 127);
}

#[test]
fn every_sector_of_every_fixture_image() {
    let Some(dir) = common::fixtures_dir("every_sector_of_every_fixture_image") else {
        return;
    };
    let images = common::fixture_images(&dir);
    let mut total = Reached::default();
    for batch in images.chunks(4) {
        let results: Vec<Reached> = std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|path| {
                    scope.spawn(move || {
                        let name = path.display().to_string();
                        let image = std::fs::read(path).unwrap_or_else(|e| panic!("{name}: {e}"));
                        guarded(&name.clone(), move |case| sweep(&image, case))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("sweep failed"))
                .collect()
        });
        for (path, reached) in batch.iter().zip(results) {
            eprintln!("{}: {reached:?}", path.display());
            total.add(&reached);
        }
    }
    eprintln!("TOTAL: {total:?}");
    assert!(total.records > 0 && total.blocks > 0);
}

/// Every 512-byte offset of `image` three times over: the bytes as they
/// are, dressed up as a `FILE` record and dressed up as an `INDX` block.
fn sweep(image: &[u8], case: &AtomicU64) -> Reached {
    let (mut buffer, mut scratch, mut reached) =
        (Vec::new(), Vec::<Run>::new(), Reached::default());
    let mut input = Vec::new();
    for sector in 0..image.len() / 512 {
        case.store(sector as u64, Ordering::Relaxed);
        let offset = sector * 512;
        // As much as a structure starting here could need, up to 4 KiB.
        let window = &image[offset..(offset + 4096).min(image.len())];
        // All-zero windows (most of a sparse image) are covered by one.
        if sector != 0 && window.iter().all(|byte| *byte == 0) {
            continue;
        }
        exercise(window, &mut buffer, &mut scratch, &mut reached);

        // As a record of 1024 bytes, with the first attribute at a place
        // that moves from sector to sector.
        if window.len() >= 1024 {
            input.clear();
            input.extend_from_slice(&window[..1024]);
            input[0..4].copy_from_slice(b"FILE");
            input[4..8].copy_from_slice(&[0x30, 0x00, 0x03, 0x00]);
            let first = 0x38 + (sector % 100) as u16 * 8;
            input[0x14..0x16].copy_from_slice(&first.to_le_bytes());
            input[0x18..0x1C].copy_from_slice(&1024u32.to_le_bytes());
            input[0x1C..0x20].copy_from_slice(&1024u32.to_le_bytes());
            exercise(&input, &mut buffer, &mut scratch, &mut reached);
        }
        // As an index block of 4096 bytes.
        if window.len() == 4096 {
            input.clear();
            input.extend_from_slice(window);
            input[0..4].copy_from_slice(b"INDX");
            input[4..8].copy_from_slice(&[0x28, 0x00, 0x09, 0x00]);
            let first = 0x28 + (sector % 400) as u32 * 8;
            input[0x18..0x1C].copy_from_slice(&first.to_le_bytes());
            input[0x1C..0x20].copy_from_slice(&0xFE8u32.to_le_bytes());
            input[0x20..0x24].copy_from_slice(&0xFE8u32.to_le_bytes());
            exercise(&input, &mut buffer, &mut scratch, &mut reached);
        }
    }
    reached
}

//! Multi-sector transfer protection: `apply_fixups` on hand-built blocks.

mod common;

use common::build::{get16, protect, put16, put32};
use hds_ntfs::{apply_fixups, Error, FixupStatus};

/// A block of `size` bytes with a multi-sector header (update sequence
/// array at `usa_offset`) whose every byte is recognisable, in memory form.
fn block(magic: &[u8; 4], size: usize, usa_offset: usize) -> Vec<u8> {
    let mut bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    bytes[0..4].copy_from_slice(magic);
    put16(&mut bytes, 0x04, usa_offset as u16);
    put16(&mut bytes, 0x06, (size / 512 + 1) as u16);
    // The array itself holds nothing meaningful in memory.
    bytes[usa_offset..usa_offset + 2 * (size / 512 + 1)].fill(0);
    bytes
}

#[test]
fn a_protected_record_comes_back_byte_for_byte() {
    let original = block(b"FILE", 1024, 0x30);
    let mut disk = original.clone();
    protect(&mut disk, 0xBEEF);
    // On disk both 512-byte blocks end with the update sequence number.
    assert_eq!(get16(&disk, 510), 0xBEEF);
    assert_eq!(get16(&disk, 1022), 0xBEEF);
    assert_ne!(disk, original);

    assert_eq!(apply_fixups(&mut disk), Ok(FixupStatus::Ok));
    // Everything outside the array is the original again.
    assert_eq!(disk[..0x30], original[..0x30]);
    assert_eq!(disk[0x36..], original[0x36..]);
    assert_eq!(get16(&disk, 510), get16(&original, 510));
    assert_eq!(get16(&disk, 1022), get16(&original, 1022));
}

#[test]
fn the_stride_is_512_bytes_for_4k_structures_too() {
    for (magic, usa_offset) in [(b"FILE", 0x30usize), (b"INDX", 0x28)] {
        let original = block(magic, 4096, usa_offset);
        let mut disk = original.clone();
        protect(&mut disk, 0x1234);
        // Nine entries; the number sits at 510, 1022, ..., 4094 - not at
        // the end of a 4096-byte sector only.
        assert_eq!(get16(&disk, 0x06), 9);
        for tail in (510..4096).step_by(512) {
            assert_eq!(get16(&disk, tail), 0x1234, "tail at {tail}");
        }
        let before = disk.clone();
        assert_eq!(apply_fixups(&mut disk), Ok(FixupStatus::Ok));
        // Exactly the eight tails changed, and to the original bytes.
        for (position, (now, was)) in disk.iter().zip(&before).enumerate() {
            let is_tail = position % 512 >= 510;
            if !is_tail {
                assert_eq!(now, was, "byte {position} must not be touched");
            }
        }
        for tail in (510..4096).step_by(512) {
            assert_eq!(disk[tail..tail + 2], original[tail..tail + 2]);
        }
    }
}

#[test]
fn every_size_up_to_64_kib() {
    for size in [512usize, 1024, 2048, 4096, 8192, 16_384, 32_768, 65_536] {
        let original = block(b"FILE", size, 0x30);
        let mut disk = original.clone();
        protect(&mut disk, 0x00FF);
        assert_eq!(apply_fixups(&mut disk), Ok(FixupStatus::Ok), "size {size}");
        let array_end = 0x30 + 2 * (size / 512 + 1);
        assert_eq!(disk[array_end..], original[array_end..], "size {size}");
    }
}

#[test]
fn a_mismatching_block_is_reported_and_left_alone() {
    let original = block(b"FILE", 4096, 0x30);
    let mut disk = original.clone();
    protect(&mut disk, 0x4242);
    // Blocks 2 and 5 come from another version of the record.
    put16(&mut disk, 3 * 512 - 2, 0x4241);
    put16(&mut disk, 6 * 512 - 2, 0x0000);
    assert_eq!(
        apply_fixups(&mut disk),
        Ok(FixupStatus::Torn {
            first_bad_block: 2,
            bad_blocks: 2
        })
    );
    // The matching blocks were restored ...
    for index in [0usize, 1, 3, 4, 6, 7] {
        let tail = (index + 1) * 512 - 2;
        assert_eq!(
            disk[tail..tail + 2],
            original[tail..tail + 2],
            "block {index}"
        );
    }
    // ... the others keep what was found.
    assert_eq!(get16(&disk, 3 * 512 - 2), 0x4241);
    assert_eq!(get16(&disk, 6 * 512 - 2), 0x0000);
    assert!(FixupStatus::Torn {
        first_bad_block: 2,
        bad_blocks: 2
    }
    .is_torn());
    assert!(!FixupStatus::Torn {
        first_bad_block: 2,
        bad_blocks: 2
    }
    .is_ok());
    assert!(FixupStatus::Ok.is_ok());
    assert!(!FixupStatus::Ok.is_torn());
}

#[test]
fn a_torn_first_block_is_block_zero() {
    let mut disk = block(b"FILE", 1024, 0x30);
    protect(&mut disk, 7);
    put16(&mut disk, 510, 8);
    assert_eq!(
        apply_fixups(&mut disk),
        Ok(FixupStatus::Torn {
            first_bad_block: 0,
            bad_blocks: 1
        })
    );
}

#[test]
fn an_image_that_was_never_protected_mismatches_everywhere_and_stays_as_it_is() {
    // What the copies of new records inside $LogFile pages look like: the
    // array says number 1, saved values 0, and no block ends with 1.
    let mut image = block(b"FILE", 1024, 0x30);
    put16(&mut image, 0x30, 1);
    let before = image.clone();
    assert_eq!(
        apply_fixups(&mut image),
        Ok(FixupStatus::Torn {
            first_bad_block: 0,
            bad_blocks: 2
        })
    );
    assert_eq!(image, before);
}

#[test]
fn a_second_application_reports_torn_but_changes_nothing() {
    let mut disk = block(b"FILE", 1024, 0x30);
    protect(&mut disk, 0x7777);
    assert_eq!(apply_fixups(&mut disk), Ok(FixupStatus::Ok));
    let fixed = disk.clone();
    assert_eq!(
        apply_fixups(&mut disk),
        Ok(FixupStatus::Torn {
            first_bad_block: 0,
            bad_blocks: 2
        })
    );
    assert_eq!(disk, fixed);
}

#[test]
fn real_bytes_equal_to_the_number_are_not_a_problem() {
    // The data at the end of block 1 happens to be the number itself.
    let mut original = block(b"FILE", 1024, 0x30);
    put16(&mut original, 1022, 0x5A5A);
    let mut disk = original.clone();
    protect(&mut disk, 0x5A5A);
    assert_eq!(apply_fixups(&mut disk), Ok(FixupStatus::Ok));
    assert_eq!(get16(&disk, 1022), 0x5A5A);
    assert_eq!(get16(&disk, 510), get16(&original, 510));
}

#[test]
fn the_header_must_fit_the_block() {
    let good = {
        let mut disk = block(b"FILE", 1024, 0x30);
        protect(&mut disk, 9);
        disk
    };
    let refused = |mutate: &dyn Fn(&mut Vec<u8>), expected: Error| {
        let mut disk = good.clone();
        mutate(&mut disk);
        let before = disk.clone();
        assert_eq!(apply_fixups(&mut disk), Err(expected));
        assert_eq!(disk, before, "a refused block must not be modified");
    };
    // Too short to be a block at all.
    refused(&|d| d.truncate(511), Error::TooShort);
    refused(&|d| d.clear(), Error::TooShort);
    // Not a whole number of 512-byte blocks.
    refused(&|d| d.truncate(1000), Error::BadUsa);
    refused(&|d| d.push(0), Error::BadUsa);
    // The count does not match the length: one block too few, too many.
    refused(&|d| put16(d, 0x06, 2), Error::BadUsa);
    refused(&|d| put16(d, 0x06, 4), Error::BadUsa);
    refused(&|d| put16(d, 0x06, 0), Error::BadUsa);
    refused(&|d| put16(d, 0x06, 1), Error::BadUsa);
    refused(&|d| d.truncate(512), Error::BadUsa);
    // The array would run into the last two bytes of the first block:
    // 506 + 2 * 3 = 512 > 510.
    refused(&|d| put16(d, 0x04, 506), Error::BadUsa);
    refused(&|d| put16(d, 0x04, 0xFFFF), Error::BadUsa);
    // ... or lie inside the multi-sector header itself.
    refused(&|d| put16(d, 0x04, 7), Error::BadUsa);
    refused(&|d| put16(d, 0x04, 0), Error::BadUsa);
    // More than 64 KiB is no structure of NTFS.
    refused(
        &|d| {
            d.resize(65_536 + 512, 0);
            put16(d, 0x06, 130);
        },
        Error::BadUsa,
    );
}

#[test]
fn the_array_may_end_exactly_at_the_tail_of_the_first_block() {
    // 504 + 2 * 3 = 510: the last position that leaves the tail free.
    let original = {
        let mut bytes = block(b"FILE", 1024, 0x30);
        put16(&mut bytes, 0x04, 504);
        bytes[504..510].fill(0);
        bytes
    };
    let mut disk = original.clone();
    protect(&mut disk, 0xABCD);
    assert_eq!(apply_fixups(&mut disk), Ok(FixupStatus::Ok));
    assert_eq!(disk[510..], original[510..]);
    // And directly behind the 8-byte header.
    let mut disk = block(b"RCRD", 1024, 8);
    protect(&mut disk, 3);
    assert_eq!(apply_fixups(&mut disk), Ok(FixupStatus::Ok));
}

#[test]
fn the_magic_is_not_this_functions_business() {
    // Any structure with a multi-sector header can be fixed up; whether it
    // is a record is decided by the probes.
    let mut disk = block(b"XXXX", 1024, 0x30);
    protect(&mut disk, 0x0102);
    assert_eq!(apply_fixups(&mut disk), Ok(FixupStatus::Ok));
    let mut zeros = vec![0u8; 1024];
    assert_eq!(apply_fixups(&mut zeros), Err(Error::BadUsa));
    put32(&mut zeros, 0, 0);
    assert!(zeros.iter().all(|b| *b == 0));
}

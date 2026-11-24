use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::resilient::classify_read_error;
use crate::testing::MemSource;

fn patterned(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i / 512) as u8 ^ (i as u8).wrapping_mul(31))
        .collect()
}

#[test]
fn source_info_matches_the_protocol_object() {
    let disk = SourceInfo {
        kind: SourceKind::Disk,
        path: r"\\.\PhysicalDrive1".to_string(),
        size: 4_000_787_030_016,
        sector_size: 512,
        physical_sector_size: 4096,
        model: Some("WDC WD40EZRZ-00GXCB0".to_string()),
        serial: Some("WD-WCC7K0000000".to_string()),
        disk_number: Some(1),
    };
    // The example of PROTOCOL.md, section 2.
    let expected: serde_json::Value = serde_json::from_str(
        r#"{"kind": "disk", "path": "\\\\.\\PhysicalDrive1", "size": 4000787030016,
            "sector_size": 512, "physical_sector_size": 4096,
            "model": "WDC WD40EZRZ-00GXCB0", "serial": "WD-WCC7K0000000", "disk_number": 1}"#,
    )
    .unwrap();
    assert_eq!(serde_json::to_value(&disk).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<SourceInfo>(expected).unwrap(),
        disk
    );

    let image = SourceInfo {
        kind: SourceKind::Image,
        path: "/data/disk.img".to_string(),
        size: 1 << 30,
        sector_size: 512,
        physical_sector_size: 512,
        model: None,
        serial: None,
        disk_number: None,
    };
    let json = serde_json::to_string(&image).unwrap();
    assert!(json.contains(r#""kind":"image""#), "{json}");
    assert!(
        json.contains(r#""model":null,"serial":null,"disk_number":null"#),
        "{json}"
    );
    assert_eq!(serde_json::from_str::<SourceInfo>(&json).unwrap(), image);
}

#[test]
fn ranged_source_reads_inside_and_refuses_outside() {
    let data = patterned(64 * 512);
    let inner: Arc<dyn BlockSource> = Arc::new(MemSource::new(data.clone()));
    let ranged = RangedSource::new(inner, 4096..16384).unwrap();
    assert_eq!(ranged.range(), 4096..16384);
    assert_eq!(
        (ranged.start(), ranged.end(), ranged.len()),
        (4096, 16384, 12288)
    );
    assert!(!ranged.is_empty());
    // Offsets stay absolute and the size is that of the underlying source.
    assert_eq!(ranged.size(), 64 * 512);

    let mut buf = vec![0u8; 1024];
    ranged.read_at(4096, &mut buf).unwrap();
    assert_eq!(buf, data[4096..5120]);
    ranged.read_at(16384 - 1024, &mut buf).unwrap();
    assert_eq!(buf, data[15360..16384]);

    for (offset, len) in [
        (0u64, 512usize),
        (3584, 512),
        (3584, 1024),
        (16384, 512),
        (15872, 1024),
        (0, 64 * 512),
        (u64::MAX - 511, 512),
        (u64::MAX, 0),
    ] {
        let mut buf = vec![0xAAu8; len];
        let err = ranged
            .read_at(offset, &mut buf)
            .expect_err("read outside the session range");
        let out = OutOfRange::from_io(&err)
            .unwrap_or_else(|| panic!("distinct error expected, got {err:?}"));
        assert_eq!(
            out,
            &OutOfRange {
                offset,
                len: len as u64,
                range: 4096..16384
            }
        );
        assert_eq!(classify_read_error(&err), crate::ReadFailure::Caller);
        assert!(
            buf.iter().all(|&b| b == 0xAA),
            "a refused read must not touch the buffer"
        );
        assert!(err.to_string().contains("outside the session range"));
    }
    // An ordinary error is not mistaken for the distinct one.
    assert!(OutOfRange::from_io(&std::io::Error::other("x")).is_none());
}

#[test]
fn ranged_source_validates_its_range() {
    let inner = || -> Arc<dyn BlockSource> {
        Arc::new(MemSource::with_sector_sizes(vec![0; 8 * 4096], 4096, 4096))
    };
    assert_eq!(
        RangedSource::new(inner(), 4096..4096).unwrap_err(),
        RangeError::Empty
    );
    #[allow(clippy::reversed_empty_ranges)]
    let inverted = 8192..4096;
    assert_eq!(
        RangedSource::new(inner(), inverted).unwrap_err(),
        RangeError::Empty
    );
    assert_eq!(
        RangedSource::new(inner(), 512..8192).unwrap_err(),
        RangeError::Unaligned { sector_size: 4096 }
    );
    assert_eq!(
        RangedSource::new(inner(), 0..6000).unwrap_err(),
        RangeError::Unaligned { sector_size: 4096 }
    );
    assert_eq!(
        RangedSource::new(inner(), 0..9 * 4096).unwrap_err(),
        RangeError::BeyondEnd { size: 8 * 4096 }
    );
    let whole = RangedSource::new(inner(), 0..8 * 4096).unwrap();
    assert_eq!(whole.len(), 8 * 4096);
    for e in [
        RangeError::Empty,
        RangeError::Unaligned { sector_size: 512 },
        RangeError::BeyondEnd { size: 1 },
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn ranged_source_helpers_and_forwarding() {
    let mem = MemSource::with_sector_sizes(patterned(32 * 512), 512, 4096)
        .named("fwd")
        .at_location(SourceLocation::Disk { number: 7 });
    let ranged = RangedSource::new(Arc::new(mem), 1024..8192).unwrap();
    assert_eq!(ranged.sector_size(), 512);
    assert_eq!(ranged.physical_sector_size(), 4096);
    assert_eq!(ranged.info().path, "mem:fwd");
    assert_eq!(ranged.info().disk_number, Some(7));
    assert_eq!(ranged.location(), SourceLocation::Disk { number: 7 });
    assert!(ranged.is_alive());

    assert!(ranged.contains(&(1024..8192)));
    assert!(ranged.contains(&(2000..2001)));
    assert!(!ranged.contains(&(1023..2000)));
    assert!(!ranged.contains(&(8000..8193)));
    assert!(!ranged.contains(&(3000..3000)));
    assert_eq!(ranged.clamp(&(0..2048)), 1024..2048);
    assert_eq!(ranged.clamp(&(4096..1 << 40)), 4096..8192);
    assert_eq!(ranged.clamp(&(2048..4096)), 2048..4096);
    let empty = ranged.clamp(&(0..512));
    assert_eq!(empty.start, empty.end);
    let empty = ranged.clamp(&(9000..10000));
    assert_eq!(empty.start, empty.end);

    // Clones share the source; the wrapper types forward everything.
    let clone = ranged.clone();
    assert_eq!(clone.range(), ranged.range());
    let boxed: Box<dyn BlockSource> = Box::new(clone);
    assert_eq!(boxed.size(), 32 * 512);
    fn location_of<S: BlockSource>(source: S) -> SourceLocation {
        source.location()
    }
    assert_eq!(location_of(&boxed), SourceLocation::Disk { number: 7 });
    let arc: Arc<dyn BlockSource> = Arc::from(boxed);
    let mut buf = [0u8; 512];
    arc.read_at(1024, &mut buf).unwrap();
    assert!(arc.read_at(0, &mut buf).is_err());
    assert!(format!("{ranged:?}").contains("mem:fwd"));
}

#[test]
fn check_read_request_enforces_the_contract() {
    assert!(check_read_request(8192, 512, 0, 8192).is_ok());
    assert!(check_read_request(8192, 512, 7680, 512).is_ok());
    assert!(check_read_request(8192, 512, 8192, 0).is_ok());
    assert!(check_read_request(8192, 4096, 4096, 4096).is_ok());
    for (size, sector, offset, len) in [
        (8192u64, 512u32, 1u64, 512usize),
        (8192, 512, 0, 100),
        (8192, 4096, 512, 4096),
        (8192, 4096, 0, 512),
        (8192, 512, 7680, 1024),
        (8192, 512, 8192, 512),
        (8192, 512, u64::MAX - 511, 512),
        (u64::MAX, 512, u64::MAX - 1023, 1024),
    ] {
        let err = check_read_request(size, sector, offset, len).unwrap_err();
        assert_eq!(
            classify_read_error(&err),
            crate::ReadFailure::Caller,
            "{offset}+{len}"
        );
    }
    // A broken source that reports sector size 0 must not divide by zero.
    assert!(check_read_request(8192, 0, 3, 5).is_ok());
}

#[test]
fn open_source_validates_kind_path_and_sector_size() {
    let err = open_source(
        SourceKind::Disk,
        Path::new(r"\\.\PhysicalDrive0"),
        Some(512),
    )
    .err()
    .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    for bad in [
        r"\\.\E:",
        r"\\.\PhysicalDrive",
        "disk.img",
        r"\\?\PhysicalDrive1",
        r"\\.\PhysicalDrive1\x",
    ] {
        let err = open_source(SourceKind::Disk, Path::new(bad), None)
            .err()
            .unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "{bad}");
    }
    #[cfg(not(windows))]
    {
        let err = open_source(SourceKind::Disk, Path::new(r"\\.\PhysicalDrive0"), None)
            .err()
            .unwrap();
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }
    for bad_size in [0u32, 1, 256, 513, 1000, 8192, 65536] {
        let err = open_source(
            SourceKind::Image,
            Path::new("does-not-matter.img"),
            Some(bad_size),
        )
        .err()
        .unwrap();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "sector size {bad_size}"
        );
    }
    let err = open_source(
        SourceKind::Image,
        Path::new("surely/does/not/exist.img"),
        None,
    )
    .err()
    .unwrap();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// The trait has no way to write: the only method that takes a buffer fills it.
#[test]
fn block_source_is_object_safe_and_read_only() {
    fn assert_send_sync<T: Send + Sync + ?Sized>() {}
    assert_send_sync::<dyn BlockSource>();
    assert_send_sync::<RangedSource>();
    struct Minimal;
    impl BlockSource for Minimal {
        fn size(&self) -> u64 {
            512
        }
        fn sector_size(&self) -> u32 {
            512
        }
        fn physical_sector_size(&self) -> u32 {
            512
        }
        fn read_at(&self, _offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            buf.fill(1);
            Ok(())
        }
        fn info(&self) -> SourceInfo {
            SourceInfo {
                kind: SourceKind::Image,
                path: String::new(),
                size: 512,
                sector_size: 512,
                physical_sector_size: 512,
                model: None,
                serial: None,
                disk_number: None,
            }
        }
        fn is_alive(&self) -> bool {
            true
        }
    }
    // A source that does not say where it lives is treated as unknown by the guard.
    assert_eq!(Minimal.location(), SourceLocation::Unknown);
}

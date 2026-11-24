use std::time::Duration;

use super::*;

fn row<'a>(status: &'a str, output: &'a str, original: &'a str, detail: &'a str) -> Row<'a> {
    Row {
        status,
        flags: "",
        size: 0,
        bytes_written: 0,
        output_path: output,
        original_path: original,
        mft_record: None,
        record_offset: None,
        first_data_offset: None,
        modified: 0,
        detail,
    }
}

#[test]
fn the_header_is_bom_columns_crlf() {
    assert_eq!(
        header(),
        b"\xEF\xBB\xBFstatus,flags,size,bytes_written,output_path,original_path,mft_record,record_offset,first_data_offset,modified_utc,detail\r\n"
    );
}

#[test]
fn a_plain_row_is_written_as_is() {
    let line = row_line(&Row {
        status: "ok",
        flags: "deleted;signature_ok",
        size: 3_456_789,
        bytes_written: 3_456_789,
        output_path: "Photos/IMG_0042.JPG",
        original_path: "Photos/IMG_0042.JPG",
        mft_record: Some(91_234),
        record_offset: Some(3_314_748_416),
        first_data_offset: Some(3_328_413_696),
        modified: 125_963_012_960_000_000,
        detail: "",
    });
    assert_eq!(
        line,
        "ok,deleted;signature_ok,3456789,3456789,Photos/IMG_0042.JPG,Photos/IMG_0042.JPG,91234,3314748416,3328413696,2000-02-29 12:34:56,\r\n"
    );
}

#[test]
fn quoting_follows_rfc_4180() {
    let line = row_line(&row(
        "warning:deleted;bad_sectors",
        "a,b.txt",
        "say \"hi\".txt",
        "line one\r\nline two",
    ));
    assert_eq!(
        line,
        "warning:deleted;bad_sectors,,0,0,\"a,b.txt\",\"say \"\"hi\"\".txt\",,,,,\"line one\r\nline two\"\r\n"
    );
    let lf_only = row_line(&row("ok", "x", "x", "a\nb"));
    assert!(lf_only.ends_with(",\"a\nb\"\r\n"), "{lf_only:?}");
}

#[test]
fn formula_cells_get_an_apostrophe() {
    for (cell, expect) in [
        ("=HYPERLINK(\"x\")", "\"'=HYPERLINK(\"\"x\"\")\""),
        ("+1", "'+1"),
        ("-2", "'-2"),
        ("@SUM(A1)", "'@SUM(A1)"),
        ("\tcmd", "'\tcmd"),
        ("\rcmd", "\"'\rcmd\""),
        ("a=b", "a=b"),
        ("\u{FF1D}full-width", "\u{FF1D}full-width"),
        ("", ""),
    ] {
        let mut out = String::new();
        push_cell(&mut out, cell);
        assert_eq!(out, expect, "{cell:?}");
    }
    // In context: the guard applies to every column, paths included.
    let line = row_line(&row("ok", "-rf", "=1+2", "@x"));
    assert_eq!(line, "ok,,0,0,'-rf,'=1+2,,,,,'@x\r\n");
}

#[test]
fn the_exact_bytes_of_a_small_report() {
    let mut bytes = header();
    for r in [
        Row {
            flags: "signature_ok",
            size: 5,
            bytes_written: 5,
            mft_record: Some(64),
            record_offset: Some(65_536),
            first_data_offset: Some(65_536),
            modified: 116_444_736_000_000_000,
            ..row("ok", "f\u{E9}.png", "dir/f\u{E9}.png", "")
        },
        row(
            "skipped:compressed",
            "c.txt",
            "c,d/c.txt",
            "the file is stored compressed",
        ),
    ] {
        bytes.extend_from_slice(row_line(&r).as_bytes());
    }
    let expected: Vec<u8> = [
        &b"\xEF\xBB\xBF"[..],
        b"status,flags,size,bytes_written,output_path,original_path,mft_record,record_offset,first_data_offset,modified_utc,detail\r\n",
        b"ok,signature_ok,5,5,f\xC3\xA9.png,dir/f\xC3\xA9.png,64,65536,65536,1970-01-01 00:00:00,\r\n",
        b"skipped:compressed,,0,0,c.txt,\"c,d/c.txt\",,,,,the file is stored compressed\r\n",
    ]
    .concat();
    assert_eq!(bytes, expected);
}

#[test]
fn filetimes_are_shown_in_utc_and_invalid_ones_are_empty() {
    for (filetime, text) in [
        (116_444_736_000_000_000, Some("1970-01-01 00:00:00")),
        (116_444_735_990_000_000, Some("1969-12-31 23:59:59")),
        (125_963_012_960_000_000, Some("2000-02-29 12:34:56")),
        (137_919_572_480_000_000, Some("2038-01-19 03:14:08")),
        (157_520_160_010_000_000, Some("2100-03-01 00:00:01")),
        (119_600_064_000_000_000, Some("1980-01-01 00:00:00")),
        (1, Some("1601-01-01 00:00:00")),
        (i64::MAX as u64, Some("30828-09-14 02:48:05")),
        (0, None),
        (1 << 63, None),
        (u64::MAX, None),
    ] {
        assert_eq!(format_filetime(filetime).as_deref(), text, "{filetime}");
    }
}

#[test]
fn report_names_use_the_utc_time_of_the_start() {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_490_100);
    assert_eq!(report_name(at, 1), "HDSavior-report-20261124-031500.csv");
    assert_eq!(
        report_name(at, 2),
        "HDSavior-report-20261124-031500 (2).csv"
    );
    let before = SystemTime::UNIX_EPOCH - Duration::from_secs(14_182_940);
    assert_eq!(
        report_name(before, 0),
        "HDSavior-report-19690720-201740.csv"
    );
    assert_eq!(civil_from_days(-1), Some((1969, 12, 31)));
    assert_eq!(civil_from_days(0), Some((1970, 1, 1)));
    assert_eq!(civil_from_days(11_016), Some((2000, 2, 29)));
    assert_eq!(civil_from_days(i64::MAX), None);
}

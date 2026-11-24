use super::*;
use crate::testing::XorShift;

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn text(units: &[u16]) -> String {
    String::from_utf16(units).expect("sanitized names are well-formed UTF-16")
}

fn fix(name: &str, suffix: u32) -> String {
    text(&sanitize_component(
        &units(name),
        suffix,
        ComponentLimit::DEFAULT,
    ))
}

/// The table of `docs/notes/windows-raw-io.md`, section 11: input, result,
/// result with collision suffix 2.
const TABLE: &[(&str, &str, &str)] = &[
    ("report.txt", "report.txt", "report (2).txt"),
    (
        "a<b>c:d\"e/f\\g|h?i*j",
        "a_b_c_d_e_f_g_h_i_j",
        "a_b_c_d_e_f_g_h_i_j (2)",
    ),
    ("tab\there", "tab_here", "tab_here (2)"),
    ("trailing dot.", "trailing dot", "trailing dot (2)"),
    ("trailing space ", "trailing space", "trailing space (2)"),
    ("...", "_", "_ (2)"),
    (" ", "_", "_ (2)"),
    (".", "_", "_ (2)"),
    ("..", "_", "_ (2)"),
    (".gitignore", ".gitignore", ".gitignore (2)"),
    ("CON", "_CON", "_CON (2)"),
    ("con", "_con", "_con (2)"),
    ("Con.txt", "_Con.txt", "_Con (2).txt"),
    ("nul.tar.gz", "_nul.tar.gz", "_nul.tar (2).gz"),
    ("NUL .txt", "_NUL .txt", "_NUL  (2).txt"),
    ("aux", "_aux", "_aux (2)"),
    ("prn.", "_prn", "_prn (2)"),
    ("COM1", "_COM1", "_COM1 (2)"),
    ("com9.log", "_com9.log", "_com9 (2).log"),
    ("COM0", "_COM0", "_COM0 (2)"),
    ("LPT0", "_LPT0", "_LPT0 (2)"),
    ("COM10", "COM10", "COM10 (2)"),
    ("COM\u{b9}", "_COM\u{b9}", "_COM\u{b9} (2)"),
    ("lpt\u{b2}.txt", "_lpt\u{b2}.txt", "_lpt\u{b2} (2).txt"),
    ("COM\u{2074}", "COM\u{2074}", "COM\u{2074} (2)"),
    ("CONIN$", "_CONIN$", "_CONIN$ (2)"),
    ("conout$.txt", "_conout$.txt", "_conout$ (2).txt"),
    ("CONSOLE", "CONSOLE", "CONSOLE (2)"),
    ("CLOCK$", "CLOCK$", "CLOCK$ (2)"),
    ("name:stream", "name_stream", "name_stream (2)"),
    ("caf\u{e9}", "caf\u{e9}", "caf\u{e9} (2)"),
    ("ok name (2).txt", "ok name (2).txt", "ok name (2) (2).txt"),
];

#[test]
fn reproduces_the_reference_table() {
    for &(input, plain, with_suffix) in TABLE {
        assert_eq!(fix(input, 0), plain, "{input:?}");
        assert_eq!(fix(input, 1), plain, "{input:?}: suffix 1 means none");
        assert_eq!(fix(input, 2), with_suffix, "{input:?} with suffix 2");
        assert_eq!(
            text(&sanitize_name(&units(input))),
            plain,
            "{input:?}: rules 1-4 alone"
        );
    }
}

#[test]
fn reproduces_the_reference_length_cases() {
    // 300 x + .jpeg -> 255 units, ends "xxx.jpeg"
    let long = "x".repeat(300) + ".jpeg";
    let out = fix(&long, 0);
    assert_eq!(out.encode_utf16().count(), 255);
    assert!(out.ends_with("xxx.jpeg"));
    assert_eq!(out, "x".repeat(250) + ".jpeg");
    // 300 x + .jpeg, suffix 12 -> 255 units, ends "xx (12).jpeg"
    let out = fix(&long, 12);
    assert_eq!(out.encode_utf16().count(), 255);
    assert_eq!(out, "x".repeat(245) + " (12).jpeg");
    // lone high surrogate -> "a\u{FFFD}b"
    let mut lone = units("a");
    lone.push(0xD83D);
    lone.extend(units("b"));
    assert_eq!(
        text(&sanitize_component(&lone, 0, ComponentLimit::DEFAULT)),
        "a\u{FFFD}b"
    );
    // pair straddling unit 255 -> 254 units, last 0x0079
    let mut straddle = vec![b'y' as u16; 254];
    straddle.extend(units("\u{1F600}zz"));
    let out = sanitize_component(&straddle, 0, ComponentLimit::DEFAULT);
    assert_eq!((out.len(), out[253]), (254, 0x0079));
}

#[test]
fn rule_1_replaces_reserved_and_control_characters() {
    for code in 0u16..0x20 {
        assert_eq!(
            sanitize_name(&[b'a' as u16, code, b'b' as u16]),
            units("a_b"),
            "unit {code:#x}"
        );
    }
    for c in "<>:\"/\\|?*".chars() {
        assert_eq!(fix(&format!("x{c}y"), 0), "x_y", "{c:?}");
    }
    // 0x7F and everything else stays.
    assert_eq!(
        sanitize_name(&[0x7F, 0x80, 0xFFFF, b'!' as u16]),
        vec![0x7F, 0x80, 0xFFFF, b'!' as u16]
    );
    assert_eq!(fix("ünï|cödé €.dat", 0), "ünï_cödé €.dat");
}

#[test]
fn rule_2_replaces_only_unpaired_surrogates() {
    let pair = units("\u{1F600}");
    assert_eq!(sanitize_name(&pair), pair, "a pair is kept");
    assert_eq!(sanitize_name(&[0xDC00]), vec![0xFFFD], "lone low");
    assert_eq!(sanitize_name(&[0xD800]), vec![0xFFFD], "lone high");
    assert_eq!(
        sanitize_name(&[0xDC00, 0xD800]),
        vec![0xFFFD, 0xFFFD],
        "wrong order"
    );
    assert_eq!(
        sanitize_name(&[0xD800, 0xD800, 0xDC00]),
        vec![0xFFFD, 0xD800, 0xDC00],
        "high before a pair"
    );
    assert_eq!(
        sanitize_name(&[0xD83D, 0xDE00, 0xDE00]),
        vec![0xD83D, 0xDE00, 0xFFFD],
        "low after a pair"
    );
    assert_eq!(
        sanitize_name(&[b'a' as u16, 0xDBFF]),
        vec![b'a' as u16, 0xFFFD],
        "high at the end"
    );
}

#[test]
fn rule_3_strips_trailing_dots_and_spaces_only() {
    assert_eq!(fix("name. . .", 0), "name");
    assert_eq!(
        fix(" leading and inner . dots", 0),
        " leading and inner . dots"
    );
    assert_eq!(fix("", 0), "_");
    assert_eq!(fix(". . .", 0), "_");
    // What rule 1 produced does not count as a trailing dot or space.
    assert_eq!(fix("a\u{1}.", 0), "a_");
    assert_eq!(fix("?", 0), "_");
    assert_eq!(fix("a .\t", 0), "a ._");
}

#[test]
fn rule_4_prefixes_device_names_and_the_engines_own_names() {
    for reserved in [
        "CON",
        "prn",
        "Aux",
        "nul",
        "conin$",
        "CONOUT$",
        "com5",
        "LPT9",
        "NUL.txt",
        "nul  .a.b",
        "COM\u{b3}",
    ] {
        assert!(is_reserved_device_name(&units(reserved)), "{reserved:?}");
        assert_eq!(fix(reserved, 0), format!("_{reserved}"));
    }
    for free in [
        "CONS",
        "NULL",
        "COM",
        "COM11",
        "LPT",
        "LPTX",
        "COM\u{b9}0",
        "_CON",
        "CON1",
        "a.CON",
        "CON\u{a0}",
        "KON",
        "",
    ] {
        assert!(!is_reserved_device_name(&units(free)), "{free:?}");
    }
    // "x.NUL" is not reserved: only the part before the first dot counts.
    assert_eq!(fix("archive.NUL", 0), "archive.NUL");

    for own in [
        "HDSavior-report-20261124-031500.csv",
        "hdsavior-REPORT-x",
        "HDSavior-report-",
        "~hds-1.part",
        "~HDS-123456.PART",
        "~hds-.part",
        "~hds-anything at all.part",
    ] {
        assert!(is_engine_file_name(&units(own)), "{own:?}");
        assert_eq!(fix(own, 0), format!("_{own}"));
    }
    for other in [
        "HDSavior-report",
        "HDSavior report-1.csv",
        "~hds-1.partial",
        "hds-1.part",
        "~hds1.part",
        "x~hds-1.part",
        "~hds-.par",
    ] {
        assert!(!is_engine_file_name(&units(other)), "{other:?}");
        assert_eq!(fix(other, 0), other);
    }
    // A trailing dot is stripped before the pattern is looked at.
    assert_eq!(fix("~hds-7.part.", 0), "_~hds-7.part");
}

#[test]
fn sanitized_names_are_fixed_points_and_valid_components() {
    let mut rng = XorShift::new(4711);
    let alphabet: Vec<u16> = "abcXYZ019 ._-~$()<>:\"/\\|?*\u{e9}\u{b9}\u{4e2d}"
        .encode_utf16()
        .collect();
    let words = [
        "CON",
        "NUL",
        "COM1",
        "LPT\u{b2}",
        "HDSavior-report-",
        "~hds-",
        ".part",
        ".",
        " ",
        ".txt",
    ];
    for _ in 0..20_000 {
        let mut raw: Vec<u16> = Vec::new();
        for _ in 0..rng.below(12) {
            match rng.below(10) {
                0 => raw.extend(units(words[rng.below(words.len() as u64) as usize])),
                1 => raw.push(rng.below(0x20) as u16),
                2 => raw.push(0xD800 + rng.below(0x800) as u16),
                3 => raw.push(rng.next_u64() as u16),
                _ => raw.push(alphabet[rng.below(alphabet.len() as u64) as usize]),
            }
        }
        let name = sanitize_name(&raw);
        assert!(!name.is_empty());
        assert!(
            String::from_utf16(&name).is_ok(),
            "{raw:?} -> {name:?} is not well-formed"
        );
        assert_eq!(
            sanitize_name(&name),
            name,
            "{raw:?} -> {name:?} is not a fixed point"
        );
        assert!(is_valid_component(&name), "{raw:?} -> {name:?}");
        assert!(!is_engine_file_name(&name) && !is_reserved_device_name(&name));

        // With suffix and limit the result is still a valid, fitting component.
        let suffix = [0, 2, 17, u32::MAX][rng.below(4) as usize];
        for limit in [
            ComponentLimit::DEFAULT,
            ComponentLimit::Utf16Units(100),
            ComponentLimit::Utf8Bytes(255),
        ] {
            let fitted = fit_component(&name, suffix, limit);
            assert!(limit.allows(&fitted), "{fitted:?} exceeds {limit:?}");
            assert!(is_valid_component(&fitted), "{name:?} -> {fitted:?}");
            assert!(String::from_utf16(&fitted).is_ok());
            assert_eq!(
                sanitize_name(&fitted),
                fitted,
                "{fitted:?} would be changed again"
            );
        }
    }
}

#[test]
fn is_valid_component_accepts_exactly_what_needs_no_change() {
    for good in [
        "a",
        "report.txt",
        ".gitignore",
        "~hds-12.part",
        "HDSavior-report-1.csv",
        "x y",
        "\u{1F600}",
        "a.b.c",
        "$Recycle.Bin",
    ] {
        assert!(is_valid_component(&units(good)), "{good:?}");
    }
    for bad in [
        "", ".", "..", "a.", "a ", "a/b", "a\\b", "a:b", "x?", "x*", "a\u{0}b", "a\tb", "CON",
        "nul.txt", "COM1.log", "<", "\"q\"",
    ] {
        assert!(!is_valid_component(&units(bad)), "{bad:?}");
    }
    assert!(!is_valid_component(&[b'a' as u16, 0xD800]));
    assert!(!is_valid_component(&[0xDC00, b'a' as u16]));
    assert!(!is_valid_component(&[0xD800, b'a' as u16]));
}

#[test]
fn extensions_are_kept_only_when_they_are_short() {
    let split = |name: &str| {
        let u = units(name);
        let (stem, ext) = split_extension(&u);
        (text(stem), text(ext))
    };
    assert_eq!(split("a.txt"), ("a".into(), ".txt".into()));
    assert_eq!(split("a.tar.gz"), ("a.tar".into(), ".gz".into()));
    assert_eq!(split(".gitignore"), (".gitignore".into(), "".into()));
    assert_eq!(split("noext"), ("noext".into(), "".into()));
    assert_eq!(split("a."), ("a".into(), ".".into()));
    assert_eq!(
        split("a.123456789012345"),
        ("a".into(), ".123456789012345".into()),
        "16 units with the dot"
    );
    assert_eq!(
        split("a.1234567890123456"),
        ("a.1234567890123456".into(), "".into()),
        "17 units: part of the stem"
    );
    assert_eq!(split(""), ("".into(), "".into()));
}

#[test]
fn rule_6_shortens_the_stem_and_keeps_extension_and_pairs() {
    // UTF-16 units: a name of exactly the limit is untouched, one more loses a unit.
    let exact = "n".repeat(251) + ".bin";
    assert_eq!(fix(&exact, 0), exact);
    assert_eq!(fix(&("n".repeat(252) + ".bin"), 0), exact);
    // The suffix takes its room from the stem.
    let out = fix(&exact, 4_294_967_295);
    assert_eq!(out, "n".repeat(251 - 13) + " (4294967295).bin");
    assert_eq!(out.encode_utf16().count(), 255);
    // A cut never leaves half a pair, whether the pair is cut from the left or the right half.
    for lead in [248usize, 249, 250] {
        let mut raw = vec![b'p' as u16; lead];
        raw.extend(units("\u{1F600}\u{1F601}\u{1F602}.bin"));
        let out = sanitize_component(&raw, 0, ComponentLimit::DEFAULT);
        assert!(out.len() <= 255);
        assert!(String::from_utf16(&out).is_ok(), "lead {lead}");
        assert!(text(&out).ends_with(".bin"));
    }
    // Without extension a cut that exposes a dot or space is trimmed again.
    let dotted = "d".repeat(253) + ". " + &"t".repeat(20);
    assert_eq!(fix(&dotted, 0), "d".repeat(253));
    // ... while a short tail after the last dot is an extension and survives.
    assert_eq!(
        fix(&("d".repeat(254) + ". tail"), 0),
        "d".repeat(249) + ". tail"
    );
    // A smaller volume limit (UDF: 127) and the minimum.
    let out = text(&sanitize_component(
        &units(&long_name()),
        3,
        ComponentLimit::Utf16Units(127),
    ));
    assert_eq!(out.encode_utf16().count(), 127);
    assert!(out.ends_with(" (3).jpeg"));
    let out = sanitize_component(&units(&long_name()), 0, ComponentLimit::Utf16Units(1));
    assert_eq!(
        out.len(),
        MIN_COMPONENT_LIMIT as usize,
        "limits below the minimum are raised to it"
    );
    assert!(ComponentLimit::Utf16Units(255).allows(&units(&exact)));
    assert!(!ComponentLimit::Utf16Units(254).allows(&units(&exact)));
}

fn long_name() -> String {
    "x".repeat(300) + ".jpeg"
}

#[test]
fn byte_limits_count_utf8() {
    let limit = ComponentLimit::Utf8Bytes(255);
    // ASCII: bytes equal units.
    let out = text(&sanitize_component(&units(&long_name()), 0, limit));
    assert_eq!(out.len(), 255);
    // Three-byte characters: 255 units would be 765 bytes.
    let cjk = "\u{4e2d}".repeat(200) + ".txt";
    let out = text(&sanitize_component(&units(&cjk), 0, limit));
    assert!(out.len() <= 255 && out.len() > 250, "{} bytes", out.len());
    assert!(out.ends_with(".txt"));
    assert_eq!(
        out.chars().filter(|&c| c == '\u{4e2d}').count(),
        (255 - 4) / 3
    );
    // Four-byte characters are never split.
    let emoji = "\u{1F600}".repeat(100) + ".png";
    let out = text(&sanitize_component(&units(&emoji), 2, limit));
    assert!(out.len() <= 255);
    assert!(out.ends_with(" (2).png"));
    assert_eq!(
        out.chars().filter(|&c| c == '\u{1F600}').count(),
        (255 - 8) / 4
    );
    // Two-byte characters.
    let latin = "\u{e9}".repeat(300);
    let out = text(&sanitize_component(&units(&latin), 0, limit));
    assert_eq!(out.len(), 254);
    assert!(limit.allows(&units(&out)));
    assert!(!limit.allows(&units(&("\u{4e2d}".repeat(86)))));
}

#[test]
fn fallback_names_are_short_and_keep_the_extension() {
    let fb = |name: &str, record: u64| {
        text(&fallback_component(
            &units(name),
            record,
            ComponentLimit::DEFAULT,
        ))
    };
    assert_eq!(fb("report.txt", 4711), "report~4711.txt");
    assert_eq!(fb("noext", 0), "noext~0");
    assert_eq!(
        fb(&("a".repeat(100) + ".jpeg"), 91_234),
        "a".repeat(24) + "~91234.jpeg"
    );
    assert_eq!(fb(".gitignore", 5), ".gitignore~5");
    // The cut respects surrogate pairs.
    let emoji = "\u{1F600}".repeat(40) + ".png";
    let out = fb(&emoji, 281_474_976_710_655);
    assert_eq!(out, "\u{1F600}".repeat(12) + "~281474976710655.png");
    // A name whose stem holds dots keeps its real extension only.
    assert_eq!(
        fb("12345678.abcdefghijklmnopq", 9),
        "12345678.abcdefghijklmno~9"
    );
    // Still a valid component that fits.
    for name in ["x", "trailing space .txt", &long_name()] {
        let sanitized = sanitize_name(&units(name));
        let out = fallback_component(&sanitized, u64::MAX, ComponentLimit::Utf8Bytes(96));
        assert!(is_valid_component(&out), "{out:?}");
        assert!(ComponentLimit::Utf8Bytes(96).allows(&out));
    }
}

use super::*;

fn u(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn names_of(claims: &[(Vec<u16>, bool, Option<u64>)], limit: ComponentLimit) -> Vec<String> {
    let claims: Vec<Claim<'_>> = claims
        .iter()
        .map(|(raw, in_use, record)| Claim {
            raw,
            in_use: *in_use,
            record: *record,
        })
        .collect();
    let mut claimed = HashSet::new();
    assign(&mut claimed, 7, &claims, limit)
        .into_iter()
        .map(|a| a.name.into())
        .collect()
}

#[test]
fn every_sanitizing_rule_applies() {
    let lone_high = vec![u16::from(b'a'), 0xD83D, u16::from(b'b')];
    let pair = u("smile \u{1F600}.txt");
    let names = names_of(
        &[
            (u("a<b>c:d\"e/f\\g|h?i*j.txt"), true, Some(1)),
            (u("tab\there\u{1}"), true, Some(2)),
            (lone_high, true, Some(3)),
            (u("trailing dot. . "), true, Some(4)),
            (u("..."), true, Some(5)),
            (u("CON"), true, Some(6)),
            (u("nul.tar.gz"), true, Some(7)),
            (u("Com\u{B9}.log"), true, Some(8)),
            (u("LPT0"), true, Some(9)),
            (u("conin$"), true, Some(10)),
            (u("HDSavior-report-20260101-000000.csv"), true, Some(11)),
            (u("~hds-12.part"), true, Some(12)),
            (u("~HDS-.PART"), true, Some(13)),
            (pair, true, Some(14)),
            (u(""), true, Some(15)),
            (u("COM10"), true, Some(16)),
        ],
        ComponentLimit::DEFAULT,
    );
    assert_eq!(
        names,
        [
            "a_b_c_d_e_f_g_h_i_j.txt",
            "tab_here_",
            "a\u{FFFD}b",
            "trailing dot",
            "_",
            "_CON",
            "_nul.tar.gz",
            "_Com\u{B9}.log",
            "_LPT0",
            "_conin$",
            "_HDSavior-report-20260101-000000.csv",
            "_~hds-12.part",
            "_~HDS-.PART",
            "smile \u{1F600}.txt",
            "_ (2)",
            "COM10",
        ]
    );
}

#[test]
fn in_use_entries_claim_first_then_by_record_number() {
    // Deleted with the lowest number, in use with higher numbers, one without number.
    let names = names_of(
        &[
            (u("Readme.txt"), false, Some(5)),
            (u("README.TXT"), true, Some(10)),
            (u("readme.txt"), true, Some(7)),
            (u("ReadMe.txt"), true, None),
            (u("README.txt"), false, Some(3)),
        ],
        ComponentLimit::DEFAULT,
    );
    assert_eq!(
        names,
        [
            "Readme (5).txt",
            "README (2).TXT",
            "readme.txt",
            "ReadMe (3).txt",
            "README (4).txt",
        ]
    );
}

#[test]
fn names_that_become_equal_by_sanitizing_collide_too() {
    let names = names_of(
        &[
            (u("a:b"), true, Some(1)),
            (u("a?b"), true, Some(2)),
            (u("A_B"), true, Some(3)),
            (u("a_b (2)"), true, Some(4)),
        ],
        ComponentLimit::DEFAULT,
    );
    assert_eq!(names, ["a_b", "a_b (2)", "A_B (3)", "a_b (2) (2)"]);
}

#[test]
fn equal_entries_in_the_same_state_keep_the_given_order() {
    let names = names_of(
        &[
            (u("x"), true, Some(9)),
            (u("x"), true, Some(9)),
            (u("x"), true, Some(9)),
        ],
        ComponentLimit::DEFAULT,
    );
    assert_eq!(names, ["x", "x (2)", "x (3)"]);
}

#[test]
fn case_folding_covers_letters_beyond_ascii() {
    let names = names_of(
        &[
            (u("\u{C4}rger.txt"), true, Some(1)),
            (u("\u{E4}rger.TXT"), true, Some(2)),
            (u("\u{3C3}\u{3C2}"), true, Some(3)),
            (u("\u{3A3}\u{3A3}"), true, Some(4)),
            (u("stra\u{DF}e"), true, Some(5)),
            (u("STRASSE"), true, Some(6)),
        ],
        ComponentLimit::DEFAULT,
    );
    assert_eq!(
        names,
        [
            "\u{C4}rger.txt",
            "\u{E4}rger (2).TXT",
            "\u{3C3}\u{3C2}",
            "\u{3A3}\u{3A3} (2)",
            "stra\u{DF}e",
            "STRASSE",
        ],
        "sharp s has no one-letter upper case in NTFS either"
    );
    assert_eq!(fold("a\u{1F600}b"), u("A\u{1F600}B"));
}

#[test]
fn overlong_names_are_shortened_to_the_limit_and_still_unique() {
    let long = "x".repeat(300) + ".jpeg";
    let names = names_of(
        &[(u(&long), true, Some(1)), (u(&long), true, Some(2))],
        ComponentLimit::Utf16Units(255),
    );
    assert_eq!(units(&names[0]), 255);
    assert!(names[0].ends_with("xxx.jpeg"));
    assert_eq!(units(&names[1]), 255);
    assert!(names[1].ends_with("x (2).jpeg"), "{}", names[1]);

    // UTF-8 bytes on Unix file systems: CJK costs three bytes per character.
    let cjk = "\u{6F22}".repeat(200) + ".txt";
    let names = names_of(&[(u(&cjk), true, Some(1))], ComponentLimit::Utf8Bytes(255));
    assert!(names[0].len() <= 255, "{}", names[0].len());
    assert!(names[0].ends_with(".txt"));
    // Never half a surrogate pair.
    let emoji = "\u{1F600}".repeat(200);
    let names = names_of(
        &[(u(&emoji), true, Some(1))],
        ComponentLimit::Utf16Units(255),
    );
    assert_eq!(units(&names[0]), 254);
    assert!(names[0].chars().all(|c| c == '\u{1F600}'));
}

#[test]
fn claims_continue_past_taken_names_and_are_per_directory() {
    let mut claimed = HashSet::new();
    let base = u("photo.jpg");
    let first = claim_from(&mut claimed, 1, &base, 1, ComponentLimit::DEFAULT).unwrap();
    assert_eq!((&*first.name, first.suffix), ("photo.jpg", 1));
    let second = claim_from(&mut claimed, 1, &base, 1, ComponentLimit::DEFAULT).unwrap();
    assert_eq!((&*second.name, second.suffix), ("photo (2).jpg", 2));
    let other_dir = claim_from(&mut claimed, 2, &base, 1, ComponentLimit::DEFAULT).unwrap();
    assert_eq!(&*other_dir.name, "photo.jpg");
    assert!(claimed.contains(&folded_key(1, "PHOTO (2).JPG")));
    assert!(!claimed.contains(&folded_key(ROOT_KEY, "photo.jpg")));
    assert_ne!(exact_key(1, "a"), exact_key(1, "A"));
    assert_eq!(folded_key(1, "a"), folded_key(1, "A"));
    assert_eq!(next_suffix(1), Some(2));
    assert_eq!(next_suffix(0), Some(2));
    assert_eq!(next_suffix(7), Some(8));
    assert_eq!(next_suffix(u32::MAX), None);
}

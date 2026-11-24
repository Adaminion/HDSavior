use super::*;

/// The wire names of PROTOCOL.md (`Row.flags`), in the order listed there.
const PROTOCOL_FLAGS: [&str; 18] = [
    "deleted",
    "torn_record",
    "missing_fragments",
    "size_estimated",
    "out_of_bounds",
    "overwritten",
    "overlap",
    "compressed",
    "encrypted",
    "placeholder",
    "sparse",
    "no_data",
    "bad_sectors",
    "older_generation",
    "embedded",
    "stray",
    "signature_ok",
    "signature_mismatch",
];

#[test]
fn flags_carry_the_protocol_wire_names_in_canonical_order() {
    let names: Vec<&str> = Flag::ALL.iter().map(|f| f.name()).collect();
    assert_eq!(names, PROTOCOL_FLAGS);
    for (i, flag) in Flag::ALL.into_iter().enumerate() {
        assert_eq!(flag.bit(), 1 << i);
        assert_eq!(Flag::from_name(flag.name()), Some(flag));
        assert_eq!(flag.to_string(), flag.name());
        // serde uses the same names
        assert_eq!(
            serde_json::to_string(&flag).unwrap(),
            format!("\"{}\"", flag.name())
        );
        let back: Flag = serde_json::from_str(&format!("\"{}\"", flag.name())).unwrap();
        assert_eq!(back, flag);
        assert!(!flag.explanation().is_empty());
    }
    assert_eq!(Flag::from_name("Deleted"), None);
    assert_eq!(Flag::from_name(""), None);
    assert!(serde_json::from_str::<Flag>("\"signature\"").is_err());
}

#[test]
fn ratings_carry_the_protocol_wire_names() {
    let names: Vec<String> = Rating::ALL
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "\"good\"",
            "\"uncertain\"",
            "\"damaged\"",
            "\"unsupported\""
        ]
    );
    for rating in Rating::ALL {
        assert_eq!(Rating::from_name(rating.name()), Some(rating));
        assert_eq!(rating.to_string(), rating.name());
    }
    assert!(Rating::Good < Rating::Uncertain);
    assert!(Rating::Damaged < Rating::Unsupported);
    assert_eq!(Rating::from_name("bad"), None);
}

#[test]
fn a_flag_set_is_a_set() {
    let mut set = FlagSet::new();
    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
    set.insert(Flag::Sparse);
    set.insert(Flag::Deleted);
    set.insert(Flag::Sparse);
    assert_eq!(set.len(), 2);
    assert!(set.contains(Flag::Deleted) && set.contains(Flag::Sparse));
    assert!(!set.contains(Flag::Stray));
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        [Flag::Deleted, Flag::Sparse],
        "canonical order, not insertion order"
    );
    set.remove(Flag::Deleted);
    assert_eq!(set, FlagSet::from(Flag::Sparse));
    assert_eq!(
        set.with(Flag::Stray).without(Flag::Sparse),
        Flag::Stray.into()
    );
    let a = FlagSet::of(&[Flag::Deleted, Flag::Overlap]);
    let b = FlagSet::of(&[Flag::Overlap, Flag::Stray]);
    assert_eq!(
        a | b,
        FlagSet::of(&[Flag::Deleted, Flag::Overlap, Flag::Stray])
    );
    assert_eq!(a.intersection(b), Flag::Overlap.into());
    assert_eq!(a.difference(b), Flag::Deleted.into());
    assert!(a.intersects(b));
    assert!(!a.intersects(Flag::Stray.into()));
    let mut c = a;
    c |= b;
    assert_eq!(c, a.union(b));
    assert_eq!(format!("{a:?}"), "{Deleted, Overlap}");
    assert_eq!(a.to_string(), "deleted;overlap");
    assert_eq!(FlagSet::EMPTY.to_string(), "");
}

#[test]
fn every_bit_pattern_round_trips_and_unknown_bits_are_refused() {
    let all = FlagSet::from_iter(Flag::ALL);
    assert_eq!(all.bits(), (1 << 18) - 1);
    assert_eq!(FlagSet::from_bits(all.bits()), Some(all));
    assert_eq!(FlagSet::from_bits(0), Some(FlagSet::EMPTY));
    assert_eq!(FlagSet::from_bits(1 << 18), None);
    assert_eq!(FlagSet::from_bits(u32::MAX), None);
    for bits in [0u32, 1, 0b1010_1010, 0x2_FFFF, 0x3_FFFF] {
        let set = FlagSet::from_bits(bits).unwrap();
        let json = serde_json::to_string(&set).unwrap();
        let back: FlagSet = serde_json::from_str(&json).unwrap();
        assert_eq!(back, set, "{json}");
        assert_eq!(back.bits(), bits);
    }
}

#[test]
fn flag_sets_serialize_as_name_lists() {
    let set = FlagSet::of(&[Flag::SignatureOk, Flag::Sparse, Flag::Deleted]);
    assert_eq!(
        serde_json::to_string(&set).unwrap(),
        r#"["deleted","sparse","signature_ok"]"#
    );
    let parsed: FlagSet = serde_json::from_str(r#"["sparse","deleted","sparse"]"#).unwrap();
    assert_eq!(parsed, FlagSet::of(&[Flag::Deleted, Flag::Sparse]));
    assert!(serde_json::from_str::<FlagSet>(r#"["deleted","nonsense"]"#).is_err());
    assert!(serde_json::from_str::<FlagSet>(r#""deleted""#).is_err());
    assert_eq!(
        serde_json::from_str::<FlagSet>("[]").unwrap(),
        FlagSet::EMPTY
    );
}

/// G3 written a second time, by name, as a reference.
fn oracle(
    names: &[&str],
    geometry_not_validated: bool,
    carved_end_unverified: bool,
) -> &'static str {
    let any = |list: &[&str]| list.iter().any(|n| names.contains(n));
    if any(&["compressed", "encrypted", "placeholder", "embedded"]) {
        "unsupported"
    } else if any(&[
        "missing_fragments",
        "out_of_bounds",
        "overwritten",
        "bad_sectors",
        "signature_mismatch",
    ]) {
        "damaged"
    } else if any(&[
        "deleted",
        "overlap",
        "torn_record",
        "no_data",
        "older_generation",
        "stray",
    ]) || geometry_not_validated
        || carved_end_unverified
    {
        "uncertain"
    } else {
        "good"
    }
}

#[test]
fn the_rating_follows_the_g3_table_for_every_combination() {
    let mut names: Vec<&str> = Vec::with_capacity(18);
    for bits in 0u32..(1 << 18) {
        let flags = FlagSet::from_bits(bits).unwrap();
        names.clear();
        names.extend(flags.iter().map(Flag::name));
        for (geometry, carved) in [(false, false), (true, false), (false, true), (true, true)] {
            let context = RatingContext {
                geometry_not_validated: geometry,
                carved_end_unverified: carved,
            };
            let rating = rate(flags, context);
            assert_eq!(
                rating.name(),
                oracle(&names, geometry, carved),
                "{flags:?} {context:?}"
            );
            assert_eq!(flags.rating(context), rating);
            let reasons = rating_reasons(flags, context);
            assert_eq!(
                reasons.is_empty(),
                rating == Rating::Good,
                "{flags:?} {context:?}"
            );
        }
    }
}

#[test]
fn flags_that_never_change_a_rating_by_themselves() {
    let neutral = FlagSet::of(&[Flag::Sparse, Flag::SizeEstimated, Flag::SignatureOk]);
    assert_eq!(rate(neutral, RatingContext::default()), Rating::Good);
    assert!(rating_reasons(neutral, RatingContext::default()).is_empty());
    let context = RatingContext {
        geometry_not_validated: true,
        carved_end_unverified: true,
    };
    assert_eq!(
        rating_reasons(neutral.with(Flag::Deleted).with(Flag::BadSectors), context),
        [
            "deleted",
            "bad_sectors",
            "geometry_not_validated",
            "carved_end_unverified"
        ]
    );
    let json = serde_json::to_string(&context).unwrap();
    assert_eq!(
        json,
        r#"{"geometry_not_validated":true,"carved_end_unverified":true}"#
    );
}

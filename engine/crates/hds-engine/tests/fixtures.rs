//! `open_source` and `check_destination` on the real-driver fixtures
//! (`HDS_FIXTURES`, built by scripts/linux/make-fixtures.sh): the partition
//! table the engine reports matches the manifest's ground truth.

mod common;

use common::{fixture, Harness, TempDir};
use hds_engine::testing::FakeJobs;
use serde_json::{json, Value};

fn manifest(image: &std::path::Path) -> Value {
    let path = image.with_extension("manifest.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).expect("manifest JSON")
}

fn open(h: &mut Harness, image: &std::path::Path) -> Value {
    h.ok(
        "open_source",
        json!({"kind": "image", "path": image.to_string_lossy()}),
    )
}

#[test]
fn partition_tables_match_the_manifests() {
    for name in ["offset-mbr", "offset-gpt"] {
        let Some(image) = fixture(&format!("{name}.img")) else {
            return;
        };
        let truth = manifest(&image);
        let table = &truth["image"]["partition_table"];
        let mut h = Harness::new(FakeJobs::Finish);
        let opened = open(&mut h, &image);
        assert_eq!(opened["source"]["size"], truth["image"]["size"], "{name}");
        assert_eq!(opened["source"]["kind"], "image");
        assert_eq!(opened["table"], table["type"], "{name}");
        let ours = opened["partitions"].as_array().unwrap();
        let theirs = table["partitions"].as_array().unwrap();
        assert_eq!(ours.len(), theirs.len(), "{name}: {ours:?}");
        for (p, t) in ours.iter().zip(theirs) {
            assert_eq!(p["index"], t["index"], "{name}");
            assert_eq!(p["offset"], t["offset"], "{name}");
            assert_eq!(p["size"], t["length"], "{name}");
            assert_eq!(
                p["type_id"].as_str().unwrap().to_uppercase(),
                t["type"].as_str().unwrap().to_uppercase()
            );
            assert_eq!(p["letters"], json!([]), "images have no mounted volumes");
            assert_eq!(p["label"], Value::Null);
        }
        // The partition that was formatted again holds an NTFS volume now.
        let data = ours.last().unwrap();
        assert_eq!(data["fs"], "ntfs", "{name}");
        assert_eq!(
            data["ntfs"]["cluster_size"],
            truth["old_volume"]["cluster_size"]
        );
        assert_eq!(
            opened["warnings"],
            json!([]),
            "images raise no source warnings"
        );
    }
}

#[test]
fn unpartitioned_volume_is_one_pseudo_partition() {
    let Some(image) = fixture("basic.img") else {
        return;
    };
    let truth = manifest(&image);
    let mut h = Harness::new(FakeJobs::Finish);
    let opened = open(&mut h, &image);
    assert_eq!(opened["table"], "none");
    let partitions = opened["partitions"].as_array().unwrap();
    assert_eq!(partitions.len(), 1);
    assert_eq!(partitions[0]["index"], 0);
    assert_eq!(partitions[0]["offset"], 0);
    assert_eq!(partitions[0]["size"], truth["image"]["size"]);
    assert_eq!(partitions[0]["fs"], "ntfs");
    assert!(partitions[0]["ntfs"]["serial"].as_str().unwrap().len() == 16);

    // The image is the only location that is off limits.
    let dir = TempDir::new("fixtures");
    let ok = h.ok(
        "check_destination",
        json!({"path": dir.join("out"), "purpose": "recover"}),
    );
    assert_eq!(ok["verdict"], "ok");
    let refused = h.ok(
        "check_destination",
        json!({"path": image.to_string_lossy(), "purpose": "session"}),
    );
    assert_eq!(refused["verdict"], "overlaps_source");
    assert_eq!(refused["reason"], "source_image");
}

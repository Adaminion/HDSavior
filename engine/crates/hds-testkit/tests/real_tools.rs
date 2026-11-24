//! Volumes built by hds-testkit, read by real NTFS tools: `ntfsinfo`,
//! `ntfsls`, `ntfscat` (ntfsprogs) and a read-only `ntfs-3g` mount.
//!
//! One volume with every feature a driver can be expected to read (large
//! and split directories, fragmented, sparse and partly initialized files,
//! resident and non-resident attribute lists, the VCN-0 extent or the only
//! name in an extension record, hard links, 255-unit names, deleted files)
//! is built for every cluster size from 512 bytes to 2 MiB, for 4 KiB
//! records, 4096-byte sectors, every flavour, and with a fragmented `$MFT`
//! that continues through an attribute list in record 15. The driver must
//! list exactly the manifest's files and return exactly their content.
//!
//! Unix only, and only where the tools are installed and mounting is
//! allowed (root); otherwise the tests print a notice and pass. Set
//! `HDS_TESTKIT_REQUIRE_TOOLS=1` to make that a failure.
#![cfg(unix)]

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use common::{have, skip, write_built, TempDir};
use hds_testkit::disk::{Built, Disk};
use hds_testkit::hash::sha256_hex;
use hds_testkit::manifest::{Entry, EntryKind, Manifest};
use hds_testkit::ondisk::runlist::Run;
use hds_testkit::populate::{populate, TreeSpec};
use hds_testkit::volume::params::{Flavor, ListForm, MftFragment, VolumeParams};
use hds_testkit::volume::spec::{DirSpec, FileSpec, MultiSpec};

const MIB: u64 = 1 << 20;

/// A healthy volume with everything a driver should be able to read.
fn feature_volume(name: &str, params: VolumeParams) -> Built {
    let size = params.offset + params.partition_bytes();
    let mut disk = Disk::new(name, 99, size);
    let id = disk.format(params.label("volume").volume_name("Features"));
    let v = disk.volume(id);
    let root = v.root();
    let cs = v.cluster_size();
    let tree = populate(
        v,
        &TreeSpec {
            folders: 6,
            early_media: 6,
            subfolders: 4,
            files: 60,
            media: 8,
            big_folder: 500,
            deleted: 5,
            max_bytes: 50_000,
            name_shift: 0,
        },
    );
    let special = v.mkdir(root, "special");
    v.add_file(special, "fragmented.bin", FileSpec::tagged(1, 40 * cs + 17).fragments(9));
    // two free runs for a sparse file with a hole between them
    let first = v.allocator().alloc(6);
    let second = v.allocator().alloc(4);
    v.allocator().free_runs(&[first, second]);
    v.add_file(special, "sparse.bin", FileSpec::tagged(2, 60 * cs - 3).runs(vec![first, Run::hole(50), second]).sparse());
    v.add_file(special, "valid-data-short.bin", FileSpec::tagged(3, 10 * cs).initialized(3 * cs + 100));
    v.add_file(
        special,
        "list-resident.bin",
        FileSpec::tagged(4, 24 * cs - 5).fragments(12).multi(MultiSpec::new(ListForm::Resident).data(&[(1, 4), (2, 4), (3, 4)])),
    );
    v.add_file(
        special,
        "list-nonresident.bin",
        FileSpec::tagged(5, 18 * cs).fragments(9).multi(MultiSpec::new(ListForm::NonResident).data(&[(0, 3), (1, 3), (2, 0)])),
    );
    v.add_file(
        special,
        "vcn0-in-extension.bin",
        FileSpec::tagged(6, 12 * cs - 77).fragments(4).multi(MultiSpec::new(ListForm::Resident).data(&[(2, 2), (1, 2)])),
    );
    v.add_file(
        special,
        "name-in-extension.bin",
        FileSpec::tagged(7, 5000).multi(MultiSpec::new(ListForm::NonResident).names_in(1)),
    );
    v.add_file(
        special,
        "resident-in-extension.txt",
        FileSpec::tagged(8, 300).resident().multi(MultiSpec::new(ListForm::Resident).resident_data_in(1)),
    );
    // An index allocation split over two records needs several runs, which
    // large clusters do not give a folder of this size; there the folder is
    // a multi-record one in another way (name in the extension record).
    let mut split = if cs <= 8192 {
        DirSpec::new().multi(MultiSpec::new(ListForm::Resident).index_allocation(&[(0, 2), (1, 0)]))
    } else {
        DirSpec::new().multi(MultiSpec::new(ListForm::Resident).names_in(1))
    };
    split.fragment_index = true;
    let dir = v.mkdir_with(special, "index-in-extensions", split);
    for i in 0..300 {
        v.add_tagged(dir, format!("member {i:04}.dat").as_str(), 50 + i);
    }
    v.add_file(special, format!("{}.dat", "x".repeat(251)).as_str(), FileSpec::tagged(9, 1234));
    v.add_file(special, "empty.txt", FileSpec::tagged(0, 0));
    let linked = v.add_file(special, "linked.bin", FileSpec::tagged(10, 70_000));
    v.hard_link(linked, tree.folders[0], "second name of linked.bin", 1);
    disk.render(id);
    disk.finish()
}

/// What a driver shows of a volume: user files and folders that are in
/// use, by path. System files and everything below `$Extend` are hidden by
/// ntfs-3g; `System Volume Information` is not.
fn visible(manifest: &Manifest) -> Vec<&Entry> {
    manifest
        .entries
        .iter()
        .filter(|e| !e.deleted && e.record >= 16 && !e.path.is_empty() && e.components[0] != "$Extend")
        .collect()
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

struct Mount<'a>(&'a Path);

impl Drop for Mount<'_> {
    fn drop(&mut self) {
        for _ in 0..50 {
            if Command::new("umount").arg(self.0).output().is_ok_and(|o| o.status.success()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        eprintln!("could not unmount {}", self.0.display());
    }
}

fn walk(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).expect("read mounted directory") {
        let entry = entry.expect("directory entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        if entry.file_type().expect("file type").is_dir() {
            walk(&entry.path(), &path, out);
        }
        out.insert(path);
    }
}

/// Mounts the image read-only with ntfs-3g and compares listing and file
/// contents with the manifest. Returns `false` when mounting is impossible
/// here.
fn mounted_volume_equals_manifest(dir: &TempDir, name: &str, built: &Built) -> bool {
    let (image, _) = write_built(dir, name, built);
    let mount_point = dir.path(&format!("{name}.mnt"));
    std::fs::create_dir_all(&mount_point).unwrap();
    let mounted = Command::new("ntfs-3g").args(["-o", "ro"]).arg(&image).arg(&mount_point).output();
    match mounted {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            eprintln!("ntfs-3g refused {name}: {}", String::from_utf8_lossy(&o.stderr).trim());
            return false;
        }
        Err(e) => {
            eprintln!("ntfs-3g could not be started: {e}");
            return false;
        }
    }
    let _mount = Mount(&mount_point);
    let entries = visible(&built.manifest);
    // every name of every entry is a path the driver must offer
    let mut expected: BTreeSet<String> = BTreeSet::new();
    for e in &entries {
        expected.insert(e.path.clone());
        for alias in e.names.iter().filter(|n| n.namespace != 2) {
            let parent = built.manifest.entries.iter().find(|p| p.instance == e.instance && p.record == alias.parent.number);
            let parent_path = parent.map(|p| p.path.clone()).unwrap_or_default();
            expected.insert(if parent_path.is_empty() { alias.name.clone() } else { format!("{parent_path}/{}", alias.name) });
        }
    }
    let mut found = BTreeSet::new();
    walk(&mount_point, "", &mut found);
    let missing: Vec<&String> = expected.difference(&found).take(10).collect();
    let extra: Vec<&String> = found.difference(&expected).take(10).collect();
    assert!(missing.is_empty() && extra.is_empty(), "{name}: the driver does not list {missing:?} and lists {extra:?}");
    let mut files = 0;
    for e in entries.iter().filter(|e| e.kind == EntryKind::File && e.sha256.is_some()) {
        let data = std::fs::read(mount_point.join(&e.path)).unwrap_or_else(|err| panic!("{name}: reading {}: {err}", e.path));
        assert_eq!(data.len() as u64, e.size, "{name}: size of {}", e.path);
        assert_eq!(&sha256_hex(&data), e.sha256.as_ref().unwrap(), "{name}: content of {}", e.path);
        files += 1;
    }
    println!("{name}: ntfs-3g lists {} paths and returns {files} files with the manifest's content", found.len());
    assert!(files > 500 && found.len() > 880);
    true
}

fn tools() -> bool {
    if ["ntfs-3g", "ntfsinfo", "ntfsls", "ntfscat"].into_iter().all(have) {
        return true;
    }
    skip("the real-tool validation", "ntfs-3g / ntfsprogs are not installed");
    false
}

#[test]
fn ntfs3g_reads_every_geometry_and_flavour() {
    if !tools() {
        return;
    }
    let dir = TempDir::new("real-tools-mount");
    let base = || VolumeParams::new(0, 96 * MIB);
    let mut variants: Vec<(&str, VolumeParams)> = vec![
        ("cluster-512", base().cluster_size(512)),
        ("cluster-1k", base().cluster_size(1024)),
        ("cluster-2k", base().cluster_size(2048)),
        ("cluster-4k", base()),
        ("cluster-8k", base().cluster_size(8192)),
        ("cluster-64k", VolumeParams::new(0, 256 * MIB).cluster_size(65_536).mft_capacity(1024)),
        ("cluster-2m", VolumeParams::new(0, 6144 * MIB).cluster_size(2 << 20).mft_capacity(2048)),
        ("records-4k", base().record_size(4096)),
        ("sector-4k", base().sector_size(4096).record_size(4096)),
        ("win7", base().flavor(Flavor::Win7)),
        ("winxp", base().flavor(Flavor::WinXp)),
        ("win2000", base().flavor(Flavor::Win2000)),
        ("mkntfs", base().flavor(Flavor::Mkntfs)),
        ("offset-63", VolumeParams::new(63 * 512, 96 * MIB)),
    ];
    // a fragmented $MFT whose second half is described by record 15
    for (name, form) in [("mft-list-resident", ListForm::Resident), ("mft-list-nonresident", ListForm::NonResident)] {
        let mut params = base();
        let far = params.total_clusters() * 3 / 4;
        params.mft = vec![MftFragment { lcn: None, records: 512 }, MftFragment { lcn: Some(far), records: 512 }];
        params.mft_attribute_list = Some(form);
        variants.push((name, params));
    }
    let mut mounted = 0;
    for (name, params) in variants {
        let offset = params.offset;
        let built = feature_volume(name, params);
        if offset != 0 {
            // ntfs-3g mounts volumes, not disks: hand it the partition alone
            let size = built.image.len() - offset;
            let volume = hds_testkit::image::Image::from_bytes(&built.image.read_vec(offset, size as usize));
            let mut shifted = built.clone();
            shifted.image = volume;
            mounted += usize::from(mounted_volume_equals_manifest(&dir, name, &shifted));
        } else {
            mounted += usize::from(mounted_volume_equals_manifest(&dir, name, &built));
        }
    }
    if mounted == 0 {
        skip("the ntfs-3g mount test", "mounting is not possible here (not root, or no FUSE)");
    } else {
        assert_eq!(mounted, 16, "some variants could not be mounted although others could");
    }
}

#[test]
fn ntfsinfo_reports_the_geometry_of_the_manifest() {
    if !tools() {
        return;
    }
    let dir = TempDir::new("real-tools-info");
    for (name, params) in [
        ("info-4k", VolumeParams::new(0, 96 * MIB)),
        ("info-64k", VolumeParams::new(0, 256 * MIB).cluster_size(65_536).mft_capacity(1024)),
        ("info-4kn", VolumeParams::new(0, 96 * MIB).sector_size(4096).record_size(4096)),
    ] {
        let built = feature_volume(name, params);
        let (image, _) = write_built(&dir, name, &built);
        let info = run("ntfsinfo", &["-m", image.to_str().unwrap()]).unwrap_or_else(|| panic!("ntfsinfo -m rejects {name}"));
        let field = |label: &str| -> String {
            info.lines()
                .find_map(|l| l.trim().strip_prefix(label).map(|v| v.trim().to_string()))
                .unwrap_or_else(|| panic!("{name}: ntfsinfo prints no {label:?}:\n{info}"))
        };
        let g = &built.manifest.instances[0].geometry;
        assert_eq!(field("Volume Name:"), "Features");
        assert_eq!(field("Volume Version:"), "3.1");
        assert_eq!(field("Sector Size:"), g.bytes_per_sector.to_string());
        assert_eq!(field("Cluster Size:"), g.cluster_size.to_string());
        assert_eq!(field("Index Block Size:"), g.index_block_size.to_string());
        assert_eq!(field("Volume Size in Clusters:"), g.total_clusters.to_string());
        assert_eq!(field("MFT Record Size:"), g.record_size.to_string());
        assert_eq!(field("LCN of Data Attribute for FILE_MFT:"), g.mft_extents[0].lcn.to_string());
        assert_eq!(field("LCN of Data Attribute for File_MFTMirr:"), g.mftmirr_lcn.to_string());
        assert_eq!(field("Size of Attribute Definition Table:"), "2560");
        println!("{name}: ntfsinfo -m agrees with the manifest");
    }
}

#[test]
fn ntfsls_lists_the_tree_of_the_manifest() {
    if !tools() {
        return;
    }
    let dir = TempDir::new("real-tools-ls");
    let built = feature_volume("ls", VolumeParams::new(0, 96 * MIB));
    let (image, _) = write_built(&dir, "ls", &built);
    let listing = run("ntfsls", &["-R", "-a", image.to_str().unwrap()]).expect("ntfsls -R");
    // sections "path:" followed by names, separated by blank lines; ntfsls
    // adds "." and ".." of its own
    let mut found = BTreeSet::new();
    let mut section = String::new();
    for line in listing.lines() {
        if let Some(path) = line.strip_suffix(':').filter(|p| p.starts_with('/')) {
            section = path.trim_matches('/').to_string();
        } else if !line.is_empty() && line != "." && line != ".." {
            found.insert(if section.is_empty() { line.to_string() } else { format!("{section}/{line}") });
        }
    }
    let mut expected = BTreeSet::new();
    for e in visible(&built.manifest) {
        for n in e.names.iter().filter(|n| n.namespace != 2) {
            let parent = built.manifest.entries.iter().find(|p| p.record == n.parent.number).map(|p| p.path.clone());
            let parent = parent.unwrap_or_default();
            expected.insert(if parent.is_empty() { n.name.clone() } else { format!("{parent}/{}", n.name) });
        }
    }
    let missing: Vec<&String> = expected.difference(&found).take(10).collect();
    let extra: Vec<&String> = found.difference(&expected).take(10).collect();
    assert!(missing.is_empty() && extra.is_empty(), "ntfsls does not list {missing:?} and lists {extra:?}");
    println!("ntfsls -R lists the manifest's {} paths", found.len());
    assert!(found.len() > 880);
}

#[test]
fn system_files_equal_those_of_mkntfs_where_they_must() {
    if !tools() || !have("mkntfs") {
        return skip("the comparison with mkntfs", "mkntfs is not installed");
    }
    let dir = TempDir::new("real-tools-mkntfs");
    let reference = dir.path("mkntfs.img");
    std::fs::File::create(&reference).unwrap().set_len(64 * MIB).unwrap();
    let made = Command::new("mkntfs").args(["-Q", "-F", "-c", "4096", "-s", "512"]).arg(&reference).output().unwrap();
    assert!(made.status.success(), "mkntfs failed: {}", String::from_utf8_lossy(&made.stderr));
    let built = feature_volume("ours", VolumeParams::new(0, 64 * MIB));
    let (ours, _) = write_built(&dir, "ours", &built);
    let cat = |image: &Path, file: &str| -> Vec<u8> {
        let output = Command::new("ntfscat").arg(image).arg(file).output().unwrap();
        assert!(output.status.success(), "ntfscat {file}: {}", String::from_utf8_lossy(&output.stderr));
        output.stdout
    };
    // the attribute definition table is fixed for NTFS 3.1
    assert_eq!(cat(&ours, "$AttrDef"), cat(&reference, "$AttrDef"), "$AttrDef differs from mkntfs");
    // $UpCase: ours is a documented subset; it must be read back as written
    // and agree with mkntfs on ASCII
    let upcase = cat(&ours, "$UpCase");
    assert_eq!(upcase, hds_testkit::ondisk::upcase::upcase_table());
    assert_eq!(upcase[..256], cat(&reference, "$UpCase")[..256]);
    // a file of ours, through the tool
    let entry = built.manifest.entry("volume", "special/fragmented.bin");
    assert_eq!(&sha256_hex(&cat(&ours, "special/fragmented.bin")), entry.sha256.as_ref().unwrap());
}

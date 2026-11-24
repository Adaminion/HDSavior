//! The commit step on its own: a name this run produced is never
//! overwritten or skipped in favour of a new file, whatever `on_exists`
//! says. Through the public interface this only happens on file systems
//! that alias names (case-insensitive lookups, 8.3 short names), which a
//! Linux test machine does not have; here the alias is forced.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use hds_io::testing::MemSource;

use super::*;
use crate::plan::PlanFile;
use crate::source::{FileSource, Run};
use crate::testutil::{pattern, utf16, TempDir};

fn plan_file(node: u64, name: &str, offset: u64, len: u64) -> PlanFile {
    PlanFile {
        node,
        folder: None,
        name: utf16(name),
        ext: "txt".into(),
        source: FileSource::Extents {
            runs: vec![Run::Data { offset, len }],
            real_size: len,
            initialized_size: len,
        },
        size: len,
        times: Timestamps::default(),
        flags: FlagSet::new(),
        context: crate::RatingContext::default(),
        in_use: true,
        record_number: Some(node),
        record_offset: None,
        first_data_offset: Some(offset),
        original_path: name.into(),
    }
}

#[test]
fn a_name_this_run_produced_is_never_overwritten_or_skipped() {
    for policy in [OnExists::Overwrite, OnExists::Skip, OnExists::Rename] {
        let tmp = TempDir::new("produced");
        let disk = pattern(1 << 20);
        let source = RangedSource::new(Arc::new(MemSource::new(disk.clone())), 0..1 << 20).unwrap();
        let plan = Plan {
            folders: Vec::new(),
            files: vec![
                plan_file(1, "a.txt", 4096, 100),
                plan_file(2, "b.txt", 8192, 50),
            ],
        };
        let mut options = RecoverOptions::new(Vec::new());
        options.on_exists = policy;
        let job = Recovery::prepare(source, plan, &tmp.join("out"), options).unwrap();
        let Recovery {
            source,
            plan,
            options,
            root,
            limit,
            mut tree,
            mut report,
            bytes_total,
            ..
        } = job;
        let claimed = std::mem::take(&mut tree.claimed);
        let mut observer = ();
        let cancel = AtomicBool::new(false);
        let mut runner = Runner {
            source: &source,
            plan: &plan,
            tree: &tree,
            options: &options,
            root: &root,
            root_len: root.path_len_units(),
            max_file_bytes: None,
            limit,
            report: &mut report,
            observer: &mut observer,
            cancel: &cancel,
            claimed,
            produced: HashSet::new(),
            outputs: Vec::new(),
            root_outputs: 0,
            folders: Vec::new(),
            cache: Vec::new(),
            tick: 0,
            buffer: Vec::new(),
            scratch: None,
            counts: Counts::default(),
            meter: Meter::new(Instant::now()),
            bytes_total,
            files_total: 2,
        };
        // The first file, normally.
        let done = runner.process_file(0).unwrap();
        assert_eq!(done.status, FileStatus::OkUnverified);
        assert_eq!(done.output_path, "a.txt");
        // The second file's part, committed under the first file's name, as if the
        // destination had aliased the two names.
        let mut part = root.create_part().unwrap();
        part.write_all(&disk[8192..8242]).unwrap();
        let alias = Assigned {
            name: "a.txt".into(),
            suffix: 1,
        };
        let second = plan.files[1].clone();
        let outcome = runner
            .commit(&mut part, None, runner.root_len, &second, &alias)
            .unwrap();
        match outcome {
            Committed::Written { name, replaced, .. } => {
                assert_eq!(name, "b (2).txt", "{policy:?}");
                assert!(!replaced);
            }
            _ => panic!("{policy:?}: not written"),
        }
        assert_eq!(
            std::fs::read(root.path().join("a.txt")).unwrap(),
            &disk[4096..4196]
        );
        assert_eq!(
            std::fs::read(root.path().join("b (2).txt")).unwrap(),
            &disk[8192..8242]
        );
    }
}

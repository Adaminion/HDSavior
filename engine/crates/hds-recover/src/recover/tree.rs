//! The plan's folder tree, checked, the processing order of the files
//! (ALGORITHMS H5) and the output names of every entry (H4).
//!
//! Per file the tree keeps four bytes each for its folder, its place in the
//! order and its collision suffix: the name itself is computed again from
//! the stored name and the suffix when the file is written, which gives the
//! same name ([`naming::assign`] makes every name from the sanitized stored
//! name, the suffix and the component limit).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use hds_io::{fit_component, sanitize_name, ComponentLimit};

use crate::naming::{self, Assigned, Claim, ROOT_KEY};
use crate::plan::{FileName, PlanFiles, PlanFolder};

/// `file_folder` of a file directly in the destination.
pub(super) const NO_FOLDER: u32 = u32::MAX;

/// How many files are described between two looks at the cancel flag.
const CANCEL_STRIDE: usize = 4096;

/// The key of an output directory in name hashes: the folder's index in
/// the plan, or [`ROOT_KEY`] for the destination itself.
pub(super) fn dir_key(folder: Option<usize>) -> u64 {
    folder.map_or(ROOT_KEY, |i| {
        u64::try_from(i).unwrap_or(ROOT_KEY.saturating_sub(1))
    })
}

/// Why the tree could not be made.
#[derive(Debug)]
pub(super) enum TreeError {
    /// The plan contradicts itself (see [`StartError::InvalidPlan`](super::StartError)).
    Invalid(String),
    /// The cancel flag was set.
    Cancelled,
}

/// The checked structure of a plan: indices instead of ids, parents before
/// children, the order of the files and the names.
#[derive(Debug)]
pub(super) struct Tree {
    /// Per plan folder: the index of its parent (`None`: the destination).
    pub parent: Vec<Option<usize>>,
    /// The plan folders, every parent before its children.
    pub order: Vec<usize>,
    /// Per plan folder: 1 for folders directly in the destination, and so on.
    pub depth: Vec<u32>,
    /// Per plan file: the index of its folder ([`NO_FOLDER`]: the destination).
    pub file_folder: Vec<u32>,
    /// The plan files in the order they are processed (ALGORITHMS H5).
    pub file_order: Vec<u32>,
    /// Per plan folder: its output name (after [`Tree::assign_names`]).
    pub folder_names: Vec<Assigned>,
    /// Per plan file: the collision suffix of its output name (after
    /// [`Tree::assign_names`]; 1 = none).
    pub file_suffix: Vec<u32>,
    /// Every name claimed so far, per directory, folded
    /// ([`naming::folded_key`]).
    pub claimed: HashSet<u64>,
}

fn placeholder() -> Assigned {
    Assigned {
        name: "_".into(),
        suffix: 1,
    }
}

fn check_cancel(i: usize, cancel: &AtomicBool) -> Result<(), TreeError> {
    if i.is_multiple_of(CANCEL_STRIDE) && cancel.load(Ordering::Relaxed) {
        return Err(TreeError::Cancelled);
    }
    Ok(())
}

/// The output name of an entry stored as `raw` with collision suffix
/// `suffix`: what [`naming::assign`] gave it.
pub(super) fn output_name(raw: &[u16], suffix: u32, limit: ComponentLimit) -> Assigned {
    let base = sanitize_name(raw);
    Assigned {
        name: String::from_utf16_lossy(&fit_component(&base, suffix, limit)).into_boxed_str(),
        suffix,
    }
}

impl Tree {
    /// Checks the plan (folder ids unique, every parent and every file's
    /// folder present, no cycle among the parents) and orders the files by
    /// their first data byte (ALGORITHMS H5; files that store no byte come
    /// last, ties in plan order).
    pub(super) fn build(
        folders: &[PlanFolder],
        files: &mut dyn PlanFiles,
        cancel: &AtomicBool,
    ) -> Result<Tree, TreeError> {
        let count = folders.len();
        if u32::try_from(count).map_or(true, |c| c == NO_FOLDER) {
            return Err(TreeError::Invalid(format!("{count} folders are too many")));
        }
        let mut index: HashMap<u64, usize> = HashMap::with_capacity(count);
        for (i, folder) in folders.iter().enumerate() {
            if index.insert(folder.id, i).is_some() {
                return Err(TreeError::Invalid(format!(
                    "folder id {} is used twice",
                    folder.id
                )));
            }
        }
        let mut parent = Vec::with_capacity(count);
        for folder in folders {
            parent.push(match folder.parent {
                None => None,
                Some(id) => Some(*index.get(&id).ok_or_else(|| {
                    TreeError::Invalid(format!(
                        "folder {} lies in folder {id}, which is not in the plan",
                        folder.id
                    ))
                })?),
            });
        }
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut top: Vec<usize> = Vec::new();
        for (i, p) in parent.iter().enumerate() {
            match p {
                None => top.push(i),
                Some(p) => {
                    if let Some(list) = children.get_mut(*p) {
                        list.push(i);
                    }
                }
            }
        }
        // Depth first from the destination: every folder has one parent, so
        // each reachable folder is met once; a folder that is not reached
        // hangs in a cycle.
        let mut order = Vec::with_capacity(count);
        let mut depth = vec![0u32; count];
        let mut stack: Vec<(usize, u32)> = top.iter().rev().map(|&i| (i, 1)).collect();
        while let Some((i, d)) = stack.pop() {
            order.push(i);
            if let Some(slot) = depth.get_mut(i) {
                *slot = d;
            }
            if let Some(list) = children.get(i) {
                stack.extend(list.iter().rev().map(|&c| (c, d.saturating_add(1))));
            }
        }
        if order.len() != count {
            return Err(TreeError::Invalid(format!(
                "{} folders lie in a cycle of parents",
                count.saturating_sub(order.len())
            )));
        }
        drop(children);
        let file_count = files.count();
        if u32::try_from(file_count).map_or(true, |c| c == u32::MAX) {
            return Err(TreeError::Invalid(format!(
                "{file_count} files are too many for one job"
            )));
        }
        let mut file_folder = Vec::with_capacity(file_count);
        // (first data byte, index); sorted, then only the indices are kept.
        let mut keys: Vec<(u64, u32)> = Vec::with_capacity(file_count);
        for i in 0..file_count {
            check_cancel(i, cancel)?;
            let place = files.place(i).map_err(TreeError::Invalid)?;
            file_folder.push(match place.folder {
                None => NO_FOLDER,
                Some(id) => match index.get(&id) {
                    Some(&f) => u32::try_from(f).unwrap_or(NO_FOLDER),
                    None => {
                        let node = files.facts(i).map_or(i as u64, |f| f.node);
                        return Err(TreeError::Invalid(format!(
                            "file {node} lies in folder {id}, which is not in the plan"
                        )));
                    }
                },
            });
            let index = u32::try_from(i).unwrap_or(u32::MAX);
            keys.push((place.first_data_offset.unwrap_or(u64::MAX), index));
        }
        keys.sort_unstable();
        let file_order = keys.into_iter().map(|(_, i)| i).collect();
        Ok(Tree {
            parent,
            order,
            depth,
            file_folder,
            file_order,
            folder_names: vec![placeholder(); count],
            file_suffix: vec![1; file_count],
            claimed: HashSet::new(),
        })
    }

    /// The folder index of plan file `index` (`None`: the destination).
    pub(super) fn folder_of(&self, index: usize) -> Option<usize> {
        match self.file_folder.get(index).copied() {
            None | Some(NO_FOLDER) => None,
            Some(f) => usize::try_from(f).ok(),
        }
    }

    /// Assigns the output name of every folder and file (ALGORITHMS H4),
    /// one output directory at a time: its folders in plan order, then its
    /// files in plan order, claim names in the order of `naming::assign`.
    /// Only one directory's stored names are in memory at a time.
    pub(super) fn assign_names(
        &mut self,
        folders: &[PlanFolder],
        files: &mut dyn PlanFiles,
        limit: ComponentLimit,
        cancel: &AtomicBool,
    ) -> Result<(), TreeError> {
        let dirs = folders.len().saturating_add(1);
        let root = folders.len();
        let slot = |f: Option<usize>| f.unwrap_or(root);
        // The files and the folders of every directory, as index ranges of
        // one list each (counting sort: plan order is kept inside a group).
        let group = |of: &mut dyn Iterator<Item = usize>, n: usize| -> (Vec<usize>, Vec<u32>) {
            let entries: Vec<usize> = of.collect();
            let mut start = vec![0usize; dirs.saturating_add(1)];
            for &d in &entries {
                if let Some(s) = start.get_mut(d.saturating_add(1)) {
                    *s = s.saturating_add(1);
                }
            }
            for d in 0..dirs {
                let before = start.get(d).copied().unwrap_or(0);
                if let Some(s) = start.get_mut(d.saturating_add(1)) {
                    *s = s.saturating_add(before);
                }
            }
            let mut fill = start.clone();
            let mut list = vec![0u32; n];
            for (i, &d) in entries.iter().enumerate() {
                if let (Some(at), Some(next)) = (fill.get(d).copied(), fill.get_mut(d)) {
                    if let Some(x) = list.get_mut(at) {
                        *x = u32::try_from(i).unwrap_or(u32::MAX);
                    }
                    *next = at.saturating_add(1);
                }
            }
            (start, list)
        };
        let (folder_start, folder_list) =
            group(&mut self.parent.iter().map(|p| slot(*p)), self.parent.len());
        let file_count = self.file_folder.len();
        let (file_start, file_list) = {
            let me = &*self;
            group(
                &mut (0..file_count).map(|i| slot(me.folder_of(i))),
                file_count,
            )
        };
        let range = |start: &[usize], d: usize| {
            start.get(d).copied().unwrap_or(0)..start.get(d.saturating_add(1)).copied().unwrap_or(0)
        };
        let mut seen = 0usize;
        for d in 0..dirs {
            let dir = dir_key((d != root).then_some(d));
            let my_folders = folder_list.get(range(&folder_start, d)).unwrap_or(&[]);
            let my_files = file_list.get(range(&file_start, d)).unwrap_or(&[]);
            if my_folders.is_empty() && my_files.is_empty() {
                continue;
            }
            let mut names: Vec<FileName> = Vec::with_capacity(my_files.len());
            for &i in my_files {
                check_cancel(seen, cancel)?;
                seen = seen.saturating_add(1);
                names.push(files.name(i as usize).map_err(TreeError::Invalid)?);
            }
            let claims: Vec<Claim<'_>> = my_folders
                .iter()
                .map(|&i| {
                    folders.get(i as usize).map_or(
                        Claim {
                            raw: &[],
                            in_use: false,
                            record: None,
                        },
                        |f| Claim {
                            raw: &f.name,
                            in_use: f.in_use,
                            record: f.record_number,
                        },
                    )
                })
                .chain(names.iter().map(|n| Claim {
                    raw: &n.name,
                    in_use: n.in_use,
                    record: n.record_number,
                }))
                .collect();
            let assigned = naming::assign(&mut self.claimed, dir, &claims, limit);
            let (for_folders, for_files) = assigned.split_at(my_folders.len().min(assigned.len()));
            for (&i, name) in my_folders.iter().zip(for_folders) {
                if let Some(slot) = self.folder_names.get_mut(i as usize) {
                    *slot = name.clone();
                }
            }
            for (&i, name) in my_files.iter().zip(for_files) {
                if let Some(slot) = self.file_suffix.get_mut(i as usize) {
                    *slot = name.suffix;
                }
            }
        }
        Ok(())
    }
}

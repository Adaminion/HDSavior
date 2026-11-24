//! The plan's folder tree, checked, and the output names of every entry.

use std::collections::{HashMap, HashSet};

use hds_io::ComponentLimit;

use crate::naming::{self, Assigned, Claim, ROOT_KEY};
use crate::plan::Plan;

/// The key of an output directory in name hashes: the folder's index in
/// the plan, or [`ROOT_KEY`] for the destination itself.
pub(super) fn dir_key(folder: Option<usize>) -> u64 {
    folder.map_or(ROOT_KEY, |i| {
        u64::try_from(i).unwrap_or(ROOT_KEY.saturating_sub(1))
    })
}

/// The checked structure of a plan: indices instead of ids, parents before
/// children, and the names.
#[derive(Debug)]
pub(super) struct Tree {
    /// Per plan folder: the index of its parent (`None`: the destination).
    pub parent: Vec<Option<usize>>,
    /// The plan folders, every parent before its children.
    pub order: Vec<usize>,
    /// Per plan folder: 1 for folders directly in the destination, and so on.
    pub depth: Vec<u32>,
    /// Per plan file: the index of its folder (`None`: the destination).
    pub file_folder: Vec<Option<usize>>,
    /// Per plan folder: its output name (after [`Tree::assign_names`]).
    pub folder_names: Vec<Assigned>,
    /// Per plan file: its output name (after [`Tree::assign_names`]).
    pub file_names: Vec<Assigned>,
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

impl Tree {
    /// Checks the plan: folder ids unique, every parent and every file's
    /// folder present, no cycle among the parents.
    pub(super) fn build(plan: &Plan) -> Result<Tree, String> {
        let count = plan.folders.len();
        let mut index: HashMap<u64, usize> = HashMap::with_capacity(count);
        for (i, folder) in plan.folders.iter().enumerate() {
            if index.insert(folder.id, i).is_some() {
                return Err(format!("folder id {} is used twice", folder.id));
            }
        }
        let mut parent = Vec::with_capacity(count);
        for folder in &plan.folders {
            parent.push(match folder.parent {
                None => None,
                Some(id) => Some(*index.get(&id).ok_or_else(|| {
                    format!(
                        "folder {} lies in folder {id}, which is not in the plan",
                        folder.id
                    )
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
            return Err(format!(
                "{} folders lie in a cycle of parents",
                count.saturating_sub(order.len())
            ));
        }
        let mut file_folder = Vec::with_capacity(plan.files.len());
        for file in &plan.files {
            file_folder.push(match file.folder {
                None => None,
                Some(id) => Some(*index.get(&id).ok_or_else(|| {
                    format!(
                        "file {} lies in folder {id}, which is not in the plan",
                        file.node
                    )
                })?),
            });
        }
        Ok(Tree {
            parent,
            order,
            depth,
            file_folder,
            folder_names: vec![placeholder(); count],
            file_names: vec![placeholder(); plan.files.len()],
            claimed: HashSet::new(),
        })
    }

    /// Assigns the output name of every folder and file (ALGORITHMS H4),
    /// one output directory at a time.
    pub(super) fn assign_names(&mut self, plan: &Plan, limit: ComponentLimit) {
        // (directory, 0 = folder / 1 = file, index), grouped by directory.
        let mut entries: Vec<(u64, u8, usize)> =
            Vec::with_capacity(plan.folders.len().saturating_add(plan.files.len()));
        entries.extend(
            self.parent
                .iter()
                .enumerate()
                .map(|(i, p)| (dir_key(*p), 0, i)),
        );
        entries.extend(
            self.file_folder
                .iter()
                .enumerate()
                .map(|(i, f)| (dir_key(*f), 1, i)),
        );
        entries.sort_unstable();
        for group in entries.chunk_by(|a, b| a.0 == b.0) {
            let Some(&(dir, _, _)) = group.first() else {
                continue;
            };
            let claims: Vec<Claim<'_>> = group
                .iter()
                .map(|&(_, kind, i)| {
                    if kind == 0 {
                        plan.folders.get(i).map_or(
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
                    } else {
                        plan.files.get(i).map_or(
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
                    }
                })
                .collect();
            let names = naming::assign(&mut self.claimed, dir, &claims, limit);
            for (&(_, kind, i), name) in group.iter().zip(names) {
                let slot = if kind == 0 {
                    self.folder_names.get_mut(i)
                } else {
                    self.file_names.get_mut(i)
                };
                if let Some(slot) = slot {
                    *slot = name;
                }
            }
        }
    }
}

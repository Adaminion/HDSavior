//! Filling a volume with a believable tree in a few lines.
//!
//! The order of creation matters more than the names: a quick format wipes
//! the records with the lowest numbers, so [`populate`] creates what a real
//! disk got first — the top-level folders, then the first files copied onto
//! it — before everything else.

use crate::content::ContentKind;
use crate::rng::{derive, derive_n, Rng};
use crate::volume::spec::FileSpec;
use crate::volume::{NodeId, Volume};

/// How much of what [`populate`] creates.
#[derive(Clone, Debug)]
pub struct TreeSpec {
    /// Top-level folders, created first.
    pub folders: usize,
    /// Media files created right after the folders, one fragment each, in
    /// the first folders: their records are the next to go in a format,
    /// their content is what carving finds.
    pub early_media: usize,
    /// Second-level folders, spread over the top-level ones.
    pub subfolders: usize,
    /// Plain files spread over all folders.
    pub files: usize,
    /// Media files among the later files (valid PNG, JPEG, BMP, PDF, ZIP
    /// and WAV files with matching extensions).
    pub media: usize,
    /// Files in one extra folder, enough to need `INDX` blocks (0 = no
    /// such folder).
    pub big_folder: usize,
    /// Files deleted again at the end (the last ones created).
    pub deleted: usize,
    /// Size of the largest plain file in bytes.
    pub max_bytes: u64,
    /// Top-level folder `i` is called [`folder_name`]`(i + name_shift)`.
    /// Two file systems formatted over each other give the same record
    /// numbers to their first folders; with different shifts the folders
    /// have different names, so a name taken from the wrong file system
    /// shows.
    pub name_shift: usize,
}

impl Default for TreeSpec {
    fn default() -> Self {
        TreeSpec {
            folders: 60,
            early_media: 24,
            subfolders: 40,
            files: 260,
            media: 36,
            big_folder: 150,
            deleted: 12,
            max_bytes: 60_000,
            name_shift: 0,
        }
    }
}

impl TreeSpec {
    /// A small tree: a dozen folders, some sixty files.
    pub fn small() -> Self {
        TreeSpec {
            folders: 8,
            early_media: 6,
            subfolders: 6,
            files: 40,
            media: 8,
            big_folder: 0,
            deleted: 4,
            max_bytes: 30_000,
            name_shift: 0,
        }
    }
}

/// What [`populate`] created.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    /// The top-level folders.
    pub folders: Vec<NodeId>,
    /// The early media files.
    pub early_media: Vec<NodeId>,
    /// The second-level folders.
    pub subfolders: Vec<NodeId>,
    /// The plain files.
    pub files: Vec<NodeId>,
    /// The later media files.
    pub media: Vec<NodeId>,
    /// The folder with many files.
    pub big_folder: Option<NodeId>,
    /// Files of the big folder.
    pub big_files: Vec<NodeId>,
    /// The deleted files.
    pub deleted: Vec<NodeId>,
}

const FOLDER_NAMES: [&str; 40] = [
    "Photos",
    "Documents",
    "Music",
    "Videos",
    "Projects",
    "Backup",
    "Work",
    "Invoices",
    "Taxes",
    "Holiday 2019",
    "Scans",
    "Recipes",
    "School",
    "Letters",
    "Downloads",
    "Archive",
    "Family",
    "Wedding",
    "Garden",
    "House",
    "Car",
    "Insurance",
    "Bank",
    "Travel",
    "Books",
    "Games",
    "Software",
    "Drivers",
    "Manuals",
    "Presentations",
    "Accounting",
    "Clients",
    "Contracts",
    "Designs",
    "Drafts",
    "Exports",
    "Notes",
    "Old PC",
    "Phone",
    "Camera",
];

const MEDIA: [(ContentKind, &str); 6] = [
    (ContentKind::Jpeg, "IMG"),
    (ContentKind::Png, "screenshot"),
    (ContentKind::Pdf, "document"),
    (ContentKind::Zip, "archive"),
    (ContentKind::Bmp, "scan"),
    (ContentKind::Wav, "recording"),
];

/// The name of top-level folder `i`.
pub fn folder_name(i: usize) -> String {
    let base = FOLDER_NAMES[i % FOLDER_NAMES.len()];
    if i < FOLDER_NAMES.len() {
        base.to_string()
    } else {
        format!("{base} {}", i / FOLDER_NAMES.len() + 1)
    }
}

/// A media file spec of kind `i % 6` and its file name.
pub fn media_file(seed: u64, i: usize, min_len: u64) -> (String, FileSpec) {
    let (kind, stem) = MEDIA[i % MEDIA.len()];
    let name = format!("{stem}_{:04}.{}", i, kind.extension().expect("media kind"));
    (
        name,
        FileSpec::media(kind, derive_n(seed, i as u64), min_len).non_resident(),
    )
}

/// Creates folders and files in `v` below its root, in the order a disk
/// fills up. Everything derives from the volume's seed.
pub fn populate(v: &mut Volume, spec: &TreeSpec) -> Tree {
    let seed = derive(v.seed(), "populate");
    let mut rng = Rng::new(seed);
    let root = v.root();
    let mut tree = Tree::default();

    for i in 0..spec.folders {
        tree.folders
            .push(v.mkdir(root, folder_name(i + spec.name_shift).as_str()));
    }
    let top = |tree: &Tree, i: usize| {
        if tree.folders.is_empty() {
            root
        } else {
            tree.folders[i % tree.folders.len()]
        }
    };
    for i in 0..spec.early_media {
        let (name, file) = media_file(derive(seed, "early"), i, 20_000 + rng.below(60_000));
        let parent = top(&tree, i % 4);
        tree.early_media
            .push(v.add_file(parent, format!("first_{name}").as_str(), file));
    }
    for i in 0..spec.subfolders {
        let parent = top(&tree, i);
        tree.subfolders.push(v.mkdir(
            parent,
            format!("{} {:02}", ["Set", "Part", "Year", "Box"][i % 4], i).as_str(),
        ));
    }
    let mut homes: Vec<NodeId> = tree
        .folders
        .iter()
        .chain(&tree.subfolders)
        .copied()
        .collect();
    if homes.is_empty() {
        homes.push(root);
    }
    for i in 0..spec.files {
        let parent = homes[rng.below(homes.len() as u64) as usize];
        // many small files, some of them resident or empty, a few large ones
        let len = match rng.below(10) {
            0 => 0,
            1..=3 => rng.below(700),
            4..=8 => rng.below(spec.max_bytes / 4 + 1),
            _ => rng.below(spec.max_bytes + 1),
        };
        let ext = ["dat", "bin", "db", "log"][i % 4];
        let file = FileSpec::tagged(derive_n(seed, 1_000_000 + i as u64), len);
        tree.files
            .push(v.add_file(parent, format!("file-{i:04}.{ext}").as_str(), file));
    }
    for i in 0..spec.media {
        let parent = homes[rng.below(homes.len() as u64) as usize];
        let (name, file) = media_file(derive(seed, "media"), i, 6_000 + rng.below(90_000));
        tree.media.push(v.add_file(parent, name.as_str(), file));
    }
    if spec.big_folder > 0 {
        let big = v.mkdir(root, "Many files");
        for i in 0..spec.big_folder {
            let file =
                FileSpec::tagged(derive_n(seed, 2_000_000 + i as u64), 200 + rng.below(3000));
            tree.big_files
                .push(v.add_file(big, format!("item {i:05}.dat").as_str(), file));
        }
        tree.big_folder = Some(big);
    }
    for i in 0..spec.deleted {
        let parent = homes[rng.below(homes.len() as u64) as usize];
        let file = FileSpec::tagged(
            derive_n(seed, 3_000_000 + i as u64),
            5_000 + rng.below(40_000),
        );
        let node = v.add_file(parent, format!("deleted-{i:02}.tmp").as_str(), file);
        tree.deleted.push(node);
    }
    for &node in &tree.deleted {
        v.delete(node);
    }
    tree
}

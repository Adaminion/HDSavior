//! Shared by the integration tests: the sample files, a deterministic
//! random generator and readers that count or synthesise.
#![allow(dead_code)]

use std::cell::Cell;
use std::io;
use std::path::PathBuf;

use hds_carve::{FileKind, ReadAt};

pub fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("data")
}

/// A sample file from `tests/data`, named `<kind>_<what>.<ext>`.
pub struct Sample {
    pub name: String,
    pub data: Vec<u8>,
    pub kind: FileKind,
    /// The file's extension on disk.
    pub ext: String,
}

fn kind_of_prefix(prefix: &str) -> Option<FileKind> {
    FileKind::from_name(prefix)
}

fn load(name: &str) -> Vec<u8> {
    std::fs::read(data_dir().join(name)).unwrap_or_else(|e| panic!("tests/data/{name}: {e}"))
}

/// Every sample whose measured length must equal its size on disk.
pub fn samples() -> Vec<Sample> {
    let mut names: Vec<String> = std::fs::read_dir(data_dir())
        .expect("tests/data")
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| !n.starts_with("special_") && !n.ends_with(".sh"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let (prefix, _) = name
                .split_once('_')
                .unwrap_or_else(|| panic!("unexpected file {name}"));
            let kind =
                kind_of_prefix(prefix).unwrap_or_else(|| panic!("{name}: unknown kind prefix"));
            let ext = name.rsplit_once('.').unwrap().1.to_string();
            Sample {
                data: load(&name),
                name,
                kind,
                ext,
            }
        })
        .collect()
}

/// A sample with expectations of its own (`special_*`).
pub fn special(name: &str) -> Vec<u8> {
    load(name)
}

/// All files in `tests/data`, special ones included, with the kind their
/// name announces.
pub fn all_files() -> Vec<Sample> {
    let mut all = samples();
    let mut names: Vec<String> = std::fs::read_dir(data_dir())
        .expect("tests/data")
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("special_"))
        .collect();
    names.sort();
    for name in names {
        let prefix = name.split('_').nth(1).unwrap();
        let kind = kind_of_prefix(prefix).unwrap_or_else(|| panic!("{name}: unknown kind prefix"));
        let ext = name.rsplit_once('.').unwrap().1.to_string();
        all.push(Sample {
            data: load(&name),
            name,
            kind,
            ext,
        });
    }
    all
}

/// SplitMix64: tiny, deterministic, good enough to stand in for noise.
pub struct Rng(pub u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = vec![0u8; n];
        self.fill(&mut v);
        v
    }
}

/// Counts what `measure` asks of the source.
pub struct Counting<R> {
    pub inner: R,
    pub reads: Cell<u64>,
    pub bytes: Cell<u64>,
    pub furthest: Cell<u64>,
}

impl<R: ReadAt> Counting<R> {
    pub fn new(inner: R) -> Self {
        Counting {
            inner,
            reads: Cell::new(0),
            bytes: Cell::new(0),
            furthest: Cell::new(0),
        }
    }
}

impl<R: ReadAt> ReadAt for Counting<R> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read_at(offset, buf)?;
        self.reads.set(self.reads.get() + 1);
        self.bytes.set(self.bytes.get() + n as u64);
        self.furthest
            .set(self.furthest.get().max(offset + n as u64));
        Ok(n)
    }
}

/// A prefix followed by one unit repeated for ever: a file that never ends.
pub struct Endless {
    pub prefix: Vec<u8>,
    pub unit: Vec<u8>,
}

impl ReadAt for Endless {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let p = self.prefix.len() as u64;
        let u = self.unit.len() as u64;
        for (i, b) in buf.iter_mut().enumerate() {
            let Some(at) = offset.checked_add(i as u64) else {
                return Ok(i);
            };
            *b = if at < p {
                self.prefix[at as usize]
            } else {
                self.unit[((at - p) % u) as usize]
            };
        }
        Ok(buf.len())
    }
}

/// A large, mostly empty file: real bytes at a few places, zeros between,
/// `len` bytes in all. Stands in for multi-gigabyte media without holding it.
pub struct Sparse {
    pub parts: Vec<(u64, Vec<u8>)>,
    pub len: u64,
}

impl Sparse {
    pub fn new() -> Self {
        Sparse {
            parts: Vec::new(),
            len: 0,
        }
    }

    /// Appends real bytes.
    pub fn put(&mut self, bytes: &[u8]) -> &mut Self {
        self.parts.push((self.len, bytes.to_vec()));
        self.len += bytes.len() as u64;
        self
    }

    /// Appends `n` zero bytes without storing them.
    pub fn gap(&mut self, n: u64) -> &mut Self {
        self.len += n;
        self
    }
}

impl ReadAt for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if offset >= self.len {
            return Ok(0);
        }
        let n = (self.len - offset).min(buf.len() as u64) as usize;
        buf[..n].fill(0);
        for (at, bytes) in &self.parts {
            let (start, end) = (*at, *at + bytes.len() as u64);
            let lo = start.max(offset);
            let hi = end.min(offset + n as u64);
            if lo < hi {
                buf[(lo - offset) as usize..(hi - offset) as usize]
                    .copy_from_slice(&bytes[(lo - start) as usize..(hi - start) as usize]);
            }
        }
        Ok(n)
    }
}

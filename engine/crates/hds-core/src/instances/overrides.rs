//! The user's overrides (ALGORITHMS D8 `instance_set`, F7 `geometry_set`)
//! and their session block `OVERRIDES`.
//!
//! # Naming an instance
//!
//! An instance has no identity of its own on disk, and a resumed scan can
//! add records to it anywhere (an MFT fragment below the probe offset, for
//! instance), which changes every summary of it such as its lowest record
//! offset. An override therefore names its instance by a [`Target`]: the
//! disk offsets of up to [`MAX_WITNESSES`] of its records, spread over its
//! record numbers. Stored records never move or disappear, so after any
//! rebuild the override applies to the instance that now holds the most
//! of those records (ties: the lower instance id); strays never take one.
//! An override whose records are all gone to strays or to a current
//! volume applies to nothing; [`Layout::unmatched_overrides`] counts them.
//!
//! When two overrides of a kind end up on one instance (two instances the
//! user chose separately were joined by a resumed scan), the one set last
//! applies.

use crate::live::codec::{Reader, Writer};
use crate::live::DecodeError;
use crate::session::{OpaqueKind, Session, SessionError};
use crate::store::{RecordId, ScanStore};

use super::{InstanceId, Layout};

const TAG: &[u8; 4] = b"HDOV";
/// Version 2: targets. Version 1 (one lowest-record offset per override)
/// is still read: that offset is a record of the instance, so it becomes a
/// target with one witness.
const VERSION: u8 = 2;
const VERSION_KEYS: u8 = 1;

/// At most this many records name an instance in a [`Target`].
pub const MAX_WITNESSES: usize = 32;

/// What `instance_set` makes of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KindChoice {
    /// "This is the one I want": the main instance.
    Main,
    /// "Treat as a separate volume": not embedded, not older, not main;
    /// read with its own geometry.
    Other,
}

/// The instance an override names: disk offsets of some of its records
/// (ascending, at most [`MAX_WITNESSES`], never empty).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    offsets: Vec<u64>,
}

impl Target {
    /// The target of an instance with `records` (by number): up to
    /// [`MAX_WITNESSES`] records spread evenly over them. `None` without
    /// records.
    pub(crate) fn of(store: &ScanStore, records: &[(u32, RecordId)]) -> Option<Target> {
        let n = records.len();
        let take = n.min(MAX_WITNESSES);
        let mut offsets: Vec<u64> = (0..take)
            .filter_map(|i| {
                let k = i.checked_mul(n)?.checked_div(take)?;
                let &(_, id) = records.get(k)?;
                store.record(id).map(|r| r.disk_offset())
            })
            .collect();
        offsets.sort_unstable();
        offsets.dedup();
        (!offsets.is_empty()).then_some(Target { offsets })
    }

    /// The witness offsets, ascending.
    pub fn offsets(&self) -> &[u64] {
        &self.offsets
    }

    fn from_offsets(mut offsets: Vec<u64>) -> Result<Target, DecodeError> {
        if offsets.is_empty() || offsets.len() > MAX_WITNESSES {
            return Err(DecodeError::new("invalid override target"));
        }
        offsets.sort_unstable();
        offsets.dedup();
        Ok(Target { offsets })
    }
}

/// The overrides in force, in the order they were set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    kinds: Vec<(Target, KindChoice)>,
    geometries: Vec<(Target, u32, u64)>,
}

impl Overrides {
    /// No overrides.
    pub fn new() -> Overrides {
        Overrides::default()
    }

    /// Sets (`Some`) or withdraws (`None`, `"auto"`) the kind of instance
    /// `id` of `layout` (built from `store`). Every kind override that
    /// applies to that instance now is replaced; choosing `Main` also
    /// withdraws `Main` from every other instance. Nothing happens when
    /// `id` names no instance or one without records. The caller refuses
    /// `strays` (PROTOCOL `instance_set`); an override on it never
    /// applies.
    pub fn set_kind(
        &mut self,
        store: &ScanStore,
        layout: &Layout,
        id: InstanceId,
        choice: Option<KindChoice>,
    ) {
        let Some(target) = layout.target_of(store, id) else {
            return;
        };
        self.kinds.retain(|(t, c)| {
            layout.resolve(store, t) != Some(id)
                && !(choice == Some(KindChoice::Main) && *c == KindChoice::Main)
        });
        if let Some(c) = choice {
            self.kinds.push((target, c));
        }
    }

    /// Sets or withdraws the geometry of instance `id` of `layout` (F7):
    /// (cluster size, volume offset). Every geometry override that
    /// applies to that instance now is replaced. The caller validates the
    /// values (PROTOCOL `geometry_set`).
    pub fn set_geometry(
        &mut self,
        store: &ScanStore,
        layout: &Layout,
        id: InstanceId,
        geometry: Option<(u32, u64)>,
    ) {
        let Some(target) = layout.target_of(store, id) else {
            return;
        };
        self.geometries
            .retain(|(t, _, _)| layout.resolve(store, t) != Some(id));
        if let Some((cs, off)) = geometry {
            self.geometries.push((target, cs, off));
        }
    }

    /// The kind overrides, oldest first.
    pub fn kinds(&self) -> &[(Target, KindChoice)] {
        &self.kinds
    }

    /// The geometry overrides (target, cluster size, volume offset),
    /// oldest first.
    pub fn geometries(&self) -> &[(Target, u32, u64)] {
        &self.geometries
    }

    /// Whether nothing is overridden.
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty() && self.geometries.is_empty()
    }

    /// Encodes the overrides as one payload.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut w = Writer::new(TAG, VERSION);
        let target = |w: &mut Writer, t: &Target| {
            w.len(t.offsets.len());
            for &o in &t.offsets {
                w.u64(o);
            }
        };
        w.len(self.kinds.len());
        for (t, c) in &self.kinds {
            target(&mut w, t);
            w.u8(match c {
                KindChoice::Main => 0,
                KindChoice::Other => 1,
            });
        }
        w.len(self.geometries.len());
        for (t, cs, off) in &self.geometries {
            target(&mut w, t);
            w.u32(*cs);
            w.u64(*off);
        }
        w.finish()
    }

    /// Decodes a payload of [`to_payload`](Self::to_payload) (or of the
    /// version-1 format, one lowest-record offset per override).
    pub fn from_payload(bytes: &[u8]) -> Result<Overrides, DecodeError> {
        let (mut r, keys) = match Reader::new(bytes, TAG, VERSION) {
            Ok(r) => (r, false),
            Err(e) => match Reader::new(bytes, TAG, VERSION_KEYS) {
                Ok(r) => (r, true),
                Err(_) => return Err(e),
            },
        };
        let target = |r: &mut Reader<'_>| -> Result<Target, DecodeError> {
            if keys {
                return Target::from_offsets(vec![r.u64()?]);
            }
            let n = r.count(8)?;
            if n > MAX_WITNESSES {
                return Err(DecodeError::new("invalid override target"));
            }
            let mut offsets = Vec::with_capacity(n);
            for _ in 0..n {
                offsets.push(r.u64()?);
            }
            Target::from_offsets(offsets)
        };
        let mut o = Overrides::new();
        let n = r.count(9)?;
        for _ in 0..n {
            let t = target(&mut r)?;
            let c = match r.u8()? {
                0 => KindChoice::Main,
                1 => KindChoice::Other,
                v => return Err(DecodeError::new(format!("invalid kind choice {v}"))),
            };
            o.kinds.push((t, c));
        }
        let n = r.count(20)?;
        for _ in 0..n {
            let t = target(&mut r)?;
            let cs = r.u32()?;
            let off = r.u64()?;
            if !cs.is_power_of_two() || !(512..=(2 << 20)).contains(&cs) {
                return Err(DecodeError::new(
                    "invalid cluster size in a geometry override",
                ));
            }
            o.geometries.push((t, cs, off));
        }
        r.end()?;
        Ok(o)
    }

    /// The overrides stored in the session (its last `OVERRIDES` payload).
    pub fn load(session: &Session) -> Result<Overrides, DecodeError> {
        match session.opaque(OpaqueKind::Overrides).last() {
            Some(p) => Overrides::from_payload(p),
            None => Ok(Overrides::new()),
        }
    }

    /// Replaces the session's `OVERRIDES` block (written at the next
    /// checkpoint).
    pub fn save(&self, session: &mut Session) -> Result<(), SessionError> {
        session.replace_opaque(OpaqueKind::Overrides, vec![self.to_payload()])
    }
}

#[cfg(test)]
impl Overrides {
    /// A kind override with a hand-made target (payload tests).
    pub(crate) fn push_kind(&mut self, offsets: Vec<u64>, c: KindChoice) {
        self.kinds.push((Target { offsets }, c));
    }

    /// A geometry override with a hand-made target (payload tests).
    pub(crate) fn push_geometry(&mut self, offsets: Vec<u64>, cs: u32, off: u64) {
        self.geometries.push((Target { offsets }, cs, off));
    }
}

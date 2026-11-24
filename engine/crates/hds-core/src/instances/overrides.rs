//! The user's overrides (ALGORITHMS D8 `instance_set`, F7 `geometry_set`)
//! and their session block `OVERRIDES`.
//!
//! Instances are named by their key ([`Instance::key`](super::Instance::key),
//! the lowest disk offset of their records), which stays the same when the
//! catalog is rebuilt from the same scan.

use crate::live::codec::{Reader, Writer};
use crate::live::DecodeError;
use crate::session::{OpaqueKind, Session, SessionError};

const TAG: &[u8; 4] = b"HDOV";
const VERSION: u8 = 1;

/// What `instance_set` makes of an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KindChoice {
    /// "This is the one I want": the main instance.
    Main,
    /// "Treat as a separate volume": not embedded, not older, not main;
    /// read with its own geometry.
    Other,
}

/// The overrides in force.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    kinds: Vec<(u64, KindChoice)>,
    geometries: Vec<(u64, u32, u64)>,
}

impl Overrides {
    /// No overrides.
    pub fn new() -> Overrides {
        Overrides::default()
    }

    /// The kind chosen for the instance with `key`.
    pub fn kind(&self, key: u64) -> Option<KindChoice> {
        self.kinds.iter().find(|(k, _)| *k == key).map(|&(_, c)| c)
    }

    /// The geometry chosen for the instance with `key`: (cluster size,
    /// volume offset).
    pub fn geometry(&self, key: u64) -> Option<(u32, u64)> {
        self.geometries
            .iter()
            .find(|(k, _, _)| *k == key)
            .map(|&(_, cs, off)| (cs, off))
    }

    /// Sets (`Some`) or withdraws (`None`, `"auto"`) the kind of an
    /// instance. Choosing `Main` withdraws `Main` from every other
    /// instance.
    pub fn set_kind(&mut self, key: u64, choice: Option<KindChoice>) {
        self.kinds.retain(|(k, c)| {
            *k != key && !(choice == Some(KindChoice::Main) && *c == KindChoice::Main)
        });
        if let Some(c) = choice {
            self.kinds.push((key, c));
        }
        self.kinds.sort_unstable_by_key(|&(k, _)| k);
    }

    /// Sets or withdraws the geometry of an instance (F7). The caller
    /// validates the values (PROTOCOL `geometry_set`).
    pub fn set_geometry(&mut self, key: u64, geometry: Option<(u32, u64)>) {
        self.geometries.retain(|(k, _, _)| *k != key);
        if let Some((cs, off)) = geometry {
            self.geometries.push((key, cs, off));
        }
        self.geometries.sort_unstable_by_key(|&(k, _, _)| k);
    }

    /// Whether nothing is overridden.
    pub fn is_empty(&self) -> bool {
        self.kinds.is_empty() && self.geometries.is_empty()
    }

    /// Encodes the overrides as one payload.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut w = Writer::new(TAG, VERSION);
        w.len(self.kinds.len());
        for &(k, c) in &self.kinds {
            w.u64(k);
            w.u8(match c {
                KindChoice::Main => 0,
                KindChoice::Other => 1,
            });
        }
        w.len(self.geometries.len());
        for &(k, cs, off) in &self.geometries {
            w.u64(k);
            w.u32(cs);
            w.u64(off);
        }
        w.finish()
    }

    /// Decodes a payload of [`to_payload`](Self::to_payload).
    pub fn from_payload(bytes: &[u8]) -> Result<Overrides, DecodeError> {
        let mut r = Reader::new(bytes, TAG, VERSION)?;
        let mut o = Overrides::new();
        let n = r.count(9)?;
        for _ in 0..n {
            let k = r.u64()?;
            let c = match r.u8()? {
                0 => KindChoice::Main,
                1 => KindChoice::Other,
                v => return Err(DecodeError::new(format!("invalid kind choice {v}"))),
            };
            o.set_kind(k, Some(c));
        }
        let n = r.count(20)?;
        for _ in 0..n {
            let k = r.u64()?;
            let cs = r.u32()?;
            let off = r.u64()?;
            if !cs.is_power_of_two() || !(512..=(2 << 20)).contains(&cs) {
                return Err(DecodeError::new(
                    "invalid cluster size in a geometry override",
                ));
            }
            o.set_geometry(k, Some((cs, off)));
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

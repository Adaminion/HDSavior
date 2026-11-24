//! Huffman decoding of JPEG entropy-coded data, as a proof that a scan is
//! whole: every code is in its table, every block stays inside its 64
//! coefficients, every restart marker comes after exactly the number of
//! MCUs the restart interval states, and the scan ends - padded to a byte -
//! right where the frame's size says its last MCU ends, directly in front
//! of a marker.
//!
//! JPEG has no checksum; this is the check that replaces one. A file whose
//! clusters are not contiguous continues with foreign bytes somewhere in the
//! middle of a scan. Random data breaks a Huffman stream within a few
//! hundred bits; zeros, text, or the entropy data of another JPEG (even one
//! with the same tables) can decode for a while, but they shift the MCU
//! count, and the scan then fails to end exactly at a marker. Nothing is
//! reconstructed: no coefficient values are kept beyond what decoding needs
//! (the DC predictors, and for progressive refinement scans one bit per
//! coefficient saying whether it is already non-zero).
//!
//! Covered: baseline and extended sequential (`SOF0`, `SOF1`, 8 and 12
//! bits) and progressive (`SOF2`) Huffman coding, interleaved or not, with
//! or without restart intervals. Not covered, so never verified this way:
//! arithmetic coding, lossless and hierarchical JPEG, a height given later
//! by `DNL`, progressive images of more than [`MAX_PROGRESSIVE_BLOCKS`]
//! blocks, and scans that need more work than [`Budget`] allows.

use crate::reader::Src;

/// Most 8×8 blocks of a progressive image whose refinement scans are
/// followed (one `u64` each: 32 MiB). About 170 megapixels at 4:2:0.
pub(crate) const MAX_PROGRESSIVE_BLOCKS: u64 = 1 << 22;

/// Most components in a frame.
const MAX_COMPONENTS: usize = 4;

/// Most blocks in one MCU of an interleaved scan (T.81 B.2.3).
const MAX_BLOCKS_PER_MCU: u32 = 10;

/// One Huffman table (T.81 C and F.2.2.3).
struct Huffman {
    /// For the first 9 bits of input: `length << 8 | symbol` of the code
    /// they start with, 0 when the code is longer.
    fast: [u16; 1 << FAST_BITS],
    /// Largest code of each length (index 1-16), -1 when there is none.
    maxcode: [i32; 17],
    /// Smallest code of each length.
    mincode: [i32; 17],
    /// Index into `symbols` of the first code of each length.
    first: [u16; 17],
    symbols: [u8; 256],
    count: u16,
}

const FAST_BITS: u32 = 9;

impl Huffman {
    /// Builds the table from a `DHT` entry: 16 code counts and the symbols.
    /// `None` when the counts describe more codes than fit their lengths.
    fn new(counts: &[u8], symbols: &[u8]) -> Option<Huffman> {
        let mut t = Huffman {
            fast: [0; 1 << FAST_BITS],
            maxcode: [-1; 17],
            mincode: [0; 17],
            first: [0; 17],
            symbols: [0; 256],
            count: 0,
        };
        let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
        if counts.len() != 16 || total != symbols.len() || total > 256 || total == 0 {
            return None;
        }
        t.symbols.get_mut(..total)?.copy_from_slice(symbols);
        t.count = u16::try_from(total).ok()?;
        let mut code: i32 = 0;
        let mut k: u16 = 0;
        for len in 1..=16usize {
            let n = i32::from(*counts.get(len - 1)?);
            *t.first.get_mut(len)? = k;
            *t.mincode.get_mut(len)? = code;
            if n > 0 {
                // the codes of this length, for the fast table
                if len as u32 <= FAST_BITS {
                    for i in 0..n {
                        let c = (code + i) as usize;
                        let sym = *t.symbols.get(usize::from(k) + i as usize)?;
                        let shift = FAST_BITS - len as u32;
                        let lo = c << shift;
                        let hi = (c + 1) << shift;
                        for e in t.fast.get_mut(lo..hi)? {
                            *e = (len as u16) << 8 | u16::from(sym);
                        }
                    }
                }
                code += n;
                *t.maxcode.get_mut(len)? = code - 1;
                k += n as u16;
            }
            if code > 1 << len {
                return None;
            }
            code <<= 1;
        }
        Some(t)
    }

    /// Decodes one symbol.
    fn decode(&self, bits: &mut Bits) -> Option<u8> {
        let peek = bits.peek16();
        let e = *self.fast.get((peek >> (16 - FAST_BITS)) as usize)?;
        if e != 0 {
            bits.consume(u32::from(e >> 8))?;
            return Some(e as u8);
        }
        for len in (FAST_BITS as usize + 1)..=16 {
            let code = (peek >> (16 - len)) as i32;
            let max = *self.maxcode.get(len)?;
            if max >= 0 && code <= max {
                let min = *self.mincode.get(len)?;
                if code < min {
                    return None;
                }
                let at = usize::from(*self.first.get(len)?) + (code - min) as usize;
                if at >= usize::from(self.count) {
                    return None;
                }
                bits.consume(len as u32)?;
                return self.symbols.get(at).copied();
            }
        }
        None
    }
}

/// Where the reader stopped loading bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// Not yet.
    No,
    /// At the `FF` of a marker (`FF` followed by neither `00` nor... the
    /// marker code is read by whoever handles it).
    Marker(u64),
    /// The data ended (or the source failed).
    End,
}

/// Bits of entropy-coded data, most significant first, with `FF 00`
/// unstuffed. Loading stops in front of every marker.
struct Bits {
    acc: u64,
    /// Valid bits at the top of `acc`.
    n: u32,
    /// Next byte to load.
    pos: u64,
    stop: Stop,
}

impl Bits {
    fn new(pos: u64) -> Bits {
        Bits {
            acc: 0,
            n: 0,
            pos,
            stop: Stop::No,
        }
    }

    /// Loads bytes until at least 57 bits are held or a marker or the end
    /// of the data is reached.
    fn fill(&mut self, src: &mut Src) {
        while self.n <= 56 && self.stop == Stop::No {
            let w = src.window_min(self.pos, 2);
            if w.is_empty() {
                self.stop = Stop::End;
                return;
            }
            let mut i = 0usize;
            while self.n <= 56 {
                let Some(&b) = w.get(i) else { break };
                if b == 0xFF {
                    match w.get(i + 1) {
                        Some(0x00) => {
                            self.acc |= 0xFF_u64 << (56 - self.n);
                            self.n += 8;
                            i += 2;
                        }
                        Some(_) => {
                            self.stop = Stop::Marker(self.pos + i as u64);
                            break;
                        }
                        // the pair continues in the next window; when the
                        // window holds only this byte the data ends here
                        None => {
                            if w.len() == 1 {
                                self.stop = Stop::End;
                            }
                            break;
                        }
                    }
                } else {
                    self.acc |= u64::from(b) << (56 - self.n);
                    self.n += 8;
                    i += 1;
                }
            }
            self.pos += i as u64;
        }
    }

    /// The next 16 bits (zeros where none are held).
    fn peek16(&self) -> u32 {
        (self.acc >> 48) as u32
    }

    /// Drops `k` bits; `None` when fewer are held (the data ran out or a
    /// marker came first).
    fn consume(&mut self, k: u32) -> Option<()> {
        if k > self.n {
            return None;
        }
        self.acc = self.acc.checked_shl(k).unwrap_or(0);
        self.n -= k;
        Some(())
    }

    /// The next `k` bits (`k` at most 16) as a number.
    fn get(&mut self, k: u32) -> Option<u32> {
        if k == 0 {
            return Some(0);
        }
        let v = (self.acc >> (64 - k)) as u32;
        self.consume(k)?;
        Some(v)
    }

    /// The end of an entropy-coded segment: fewer than 8 bits of padding
    /// left and a marker next. Returns the offset of that marker's first
    /// `FF`.
    fn at_marker(&mut self, src: &mut Src) -> Option<u64> {
        self.fill(src);
        if self.n >= 8 {
            return None;
        }
        match self.stop {
            Stop::Marker(at) => Some(at),
            _ => None,
        }
    }
}

/// The marker code at `at` (an `FF`, possibly followed by fill bytes) and
/// the offset just behind it.
fn marker_code(src: &mut Src, mut at: u64) -> Option<(u8, u64)> {
    for _ in 0..64 {
        let m = src.arr::<2>(at)?;
        if m[0] != 0xFF {
            return None;
        }
        if m[1] != 0xFF {
            return Some((m[1], at + 2));
        }
        at += 1;
    }
    None
}

/// A frame component.
#[derive(Clone, Copy, Default)]
struct Component {
    id: u8,
    h: u8,
    v: u8,
}

/// What a frame header says, as far as decoding needs it.
#[derive(Clone, Copy)]
pub(crate) struct FrameInfo {
    /// The `SOFn` code.
    pub code: u8,
    pub precision: u8,
    pub width: u16,
    pub height: u16,
    components: [Component; MAX_COMPONENTS],
    count: usize,
}

impl FrameInfo {
    /// The frame header `b` (the body of `SOFn` with code `code`), checked
    /// by the caller.
    pub(crate) fn parse(b: &[u8], code: u8) -> Option<FrameInfo> {
        let precision = *b.first()?;
        let height = u16::from_be_bytes([*b.get(1)?, *b.get(2)?]);
        let width = u16::from_be_bytes([*b.get(3)?, *b.get(4)?]);
        let count = usize::from(*b.get(5)?);
        if count == 0 || count > MAX_COMPONENTS {
            return None;
        }
        let mut components = [Component::default(); MAX_COMPONENTS];
        for (i, c) in b.get(6..6 + 3 * count)?.chunks_exact(3).enumerate() {
            *components.get_mut(i)? = Component {
                id: c[0],
                h: c[1] >> 4,
                v: c[1] & 15,
            };
        }
        Some(FrameInfo {
            code,
            precision,
            width,
            height,
            components,
            count,
        })
    }

    fn components(&self) -> &[Component] {
        self.components.get(..self.count).unwrap_or(&[])
    }

    fn hmax(&self) -> u32 {
        self.components()
            .iter()
            .map(|c| u32::from(c.h))
            .max()
            .unwrap_or(1)
    }

    fn vmax(&self) -> u32 {
        self.components()
            .iter()
            .map(|c| u32::from(c.v))
            .max()
            .unwrap_or(1)
    }

    /// Blocks across and down of component `c` in a scan of that
    /// component alone (T.81 A.1.1).
    fn blocks(&self, c: &Component) -> (u64, u64) {
        let x = u64::from(self.width) * u64::from(c.h);
        let y = u64::from(self.height) * u64::from(c.v);
        let w = x.div_ceil(u64::from(self.hmax())).div_ceil(8);
        let h = y.div_ceil(u64::from(self.vmax())).div_ceil(8);
        (w, h)
    }

    fn progressive(&self) -> bool {
        self.code == 0xC2
    }

    /// Whether this decoder covers the frame's coding at all.
    fn decodable(&self) -> bool {
        matches!(self.code, 0xC0..=0xC2)
            && self.height != 0
            && self.width != 0
            && (self.precision == 8 || (self.precision == 12 && self.code != 0xC0))
    }
}

/// How a scan ended.
pub(crate) enum ScanEnd {
    /// Decoded to its last MCU, directly followed by the marker whose
    /// first `FF` is at this offset.
    Verified(u64),
    /// The data does not decode as announced: from `resume` on (with
    /// `next_rst` the restart marker expected next) the scan has to be
    /// searched for its end the old way, and the file's end is not
    /// verified.
    Broken { resume: u64, next_rst: u8 },
    /// Not decoded (a coding this decoder does not cover, or the work
    /// budget ran out): search from the start of the scan.
    Skipped,
}

/// A cap on the work spent decoding: `BASE` steps plus `PER_BYTE` for
/// every byte of entropy-coded data. Ordinary scans cost a few steps per
/// byte; a progressive refinement scan of a flat image costs more (each
/// block of an end-of-band run is walked), and a crafted file could make
/// every bit cost a whole run of blocks.
struct Budget;

impl Budget {
    const BASE: u64 = 1 << 26;
    const PER_BYTE: u64 = 256;
}

/// Work done so far, and the entropy-coded bytes it was done on.
#[derive(Default)]
struct Work {
    steps: u64,
    bytes: u64,
}

impl Work {
    /// Charges `steps`; `Skip` once the budget for the bytes read so far
    /// (`bytes` plus the current scan's) is spent.
    fn charge(&mut self, steps: u64, bits: &Bits, start: u64) -> Result<(), Halt> {
        self.steps = self.steps.saturating_add(steps);
        let read = self.bytes.saturating_add(bits.pos.saturating_sub(start));
        let allowed = Budget::BASE.saturating_add(read.saturating_mul(Budget::PER_BYTE));
        if self.steps > allowed {
            Err(Halt::Skip)
        } else {
            Ok(())
        }
    }
}

/// DC tables 0-3, then AC tables 0-3.
type TableSet = [Option<Box<Huffman>>; 8];

fn table(tables: &TableSet, ac: bool, id: u8) -> Option<&Huffman> {
    let slot = usize::from(ac) * 4 + usize::from(id & 3);
    tables.get(slot)?.as_deref()
}

/// Decoding state across the scans of one image.
pub(crate) struct Decoder {
    tables: TableSet,
    frame: Option<FrameInfo>,
    /// Progressive images: per component, one mask per block of which AC
    /// coefficients (bit = zig-zag index) are non-zero so far.
    nonzero: [Vec<u64>; MAX_COMPONENTS],
    work: Work,
    /// False once a scan could not be verified: the file's end is then not
    /// verified either.
    pub(crate) all_verified: bool,
}

impl Decoder {
    pub(crate) fn new() -> Decoder {
        Decoder {
            tables: Default::default(),
            frame: None,
            nonzero: Default::default(),
            work: Work::default(),
            all_verified: true,
        }
    }

    /// Takes the tables of a `DHT` segment body (structure already checked
    /// by the caller). A table that is not a valid prefix code is
    /// remembered as missing: a scan that uses it cannot be verified.
    pub(crate) fn define_tables(&mut self, mut b: &[u8]) {
        while let Some(&class_id) = b.first() {
            let Some(counts) = b.get(1..17) else { return };
            let n: usize = counts.iter().map(|&c| usize::from(c)).sum();
            let Some(symbols) = b.get(17..17 + n) else {
                return;
            };
            let slot = usize::from(class_id >> 4) * 4 + usize::from(class_id & 3);
            if let Some(t) = self.tables.get_mut(slot) {
                *t = Huffman::new(counts, symbols).map(Box::new);
            }
            b = b.get(17 + n..).unwrap_or(&[]);
        }
    }

    /// Takes the frame header.
    pub(crate) fn set_frame(&mut self, frame: FrameInfo) {
        if !frame.decodable() {
            self.all_verified = false;
        }
        self.frame = Some(frame);
    }

    /// Decodes the scan whose header body is `sos` and whose data starts at
    /// `start`.
    pub(crate) fn scan(
        &mut self,
        src: &mut Src,
        sos: &[u8],
        start: u64,
        restart_interval: u16,
    ) -> ScanEnd {
        let mut bits = Bits::new(start);
        let mut next_rst = 0u8;
        let end = match self.frame {
            Some(frame) if frame.decodable() => match ScanHeader::parse(sos, &frame) {
                Some(header) => {
                    let mut scan = Scan {
                        src,
                        frame: &frame,
                        header: &header,
                        bits: &mut bits,
                        restart_interval,
                        next_rst: &mut next_rst,
                        start,
                        work: &mut self.work,
                    };
                    scan.decode(&self.tables, &mut self.nonzero)
                }
                None => Err(Halt::Skip),
            },
            _ => Err(Halt::Skip),
        };
        self.work.bytes = self
            .work
            .bytes
            .saturating_add(bits.pos.saturating_sub(start));
        match end {
            Ok(at) => ScanEnd::Verified(at),
            Err(Halt::Skip) => {
                self.all_verified = false;
                ScanEnd::Skipped
            }
            Err(Halt::Broken) => {
                self.all_verified = false;
                ScanEnd::Broken {
                    resume: bits.pos,
                    next_rst,
                }
            }
        }
    }
}

/// One scan being decoded.
struct Scan<'s, 'a> {
    src: &'s mut Src<'a>,
    frame: &'s FrameInfo,
    header: &'s ScanHeader,
    bits: &'s mut Bits,
    restart_interval: u16,
    next_rst: &'s mut u8,
    start: u64,
    work: &'s mut Work,
}

impl Scan<'_, '_> {
    fn decode(
        &mut self,
        tables: &TableSet,
        nonzero: &mut [Vec<u64>; MAX_COMPONENTS],
    ) -> Result<u64, Halt> {
        let frame = self.frame;
        let mode = Mode::of(self.header, frame.progressive()).ok_or(Halt::Skip)?;
        let limits = if frame.precision == 12 {
            Limits {
                dc_max: 15,
                ac_max: 14,
                dc_limit: 32767,
            }
        } else {
            Limits {
                dc_max: 11,
                ac_max: 10,
                dc_limit: 2047,
            }
        };

        // the tables the scan needs, per scan component
        let mut used = Tables {
            dc: [None; MAX_COMPONENTS],
            ac: [None; MAX_COMPONENTS],
        };
        for (i, s) in self.header.components().iter().enumerate() {
            if matches!(mode, Mode::Sequential | Mode::DcFirst) {
                *used.dc.get_mut(i).ok_or(Halt::Skip)? =
                    Some(table(tables, false, s.td).ok_or(Halt::Skip)?);
            }
            if matches!(mode, Mode::Sequential | Mode::AcFirst | Mode::AcRefine) {
                *used.ac.get_mut(i).ok_or(Halt::Skip)? =
                    Some(table(tables, true, s.ta).ok_or(Halt::Skip)?);
            }
        }

        // the MCUs: single blocks of one component, or the interleaved
        // groups of `h × v` blocks per component
        let comps = self.header.components();
        let (mcus, layout) = if comps.len() == 1 {
            let s = comps.first().ok_or(Halt::Skip)?;
            let c = frame
                .components()
                .get(usize::from(s.index))
                .ok_or(Halt::Skip)?;
            let (w, h) = frame.blocks(c);
            (w.checked_mul(h).ok_or(Halt::Skip)?, Layout::Single)
        } else {
            let across = u64::from(frame.width).div_ceil(8 * u64::from(frame.hmax()));
            let down = u64::from(frame.height).div_ceil(8 * u64::from(frame.vmax()));
            let mut per_mcu = 0u32;
            for s in comps {
                let c = frame
                    .components()
                    .get(usize::from(s.index))
                    .ok_or(Halt::Skip)?;
                per_mcu += u32::from(c.h) * u32::from(c.v);
            }
            if per_mcu > MAX_BLOCKS_PER_MCU {
                return Err(Halt::Skip);
            }
            (
                across.checked_mul(down).ok_or(Halt::Skip)?,
                Layout::Interleaved,
            )
        };

        // progressive AC scans (one component each) carry the masks of
        // their component's blocks from scan to scan
        if matches!(mode, Mode::AcFirst | Mode::AcRefine) {
            let total = frame
                .components()
                .iter()
                .map(|c| {
                    let (w, h) = frame.blocks(c);
                    w.saturating_mul(h)
                })
                .fold(0u64, u64::saturating_add);
            if total > MAX_PROGRESSIVE_BLOCKS {
                return Err(Halt::Skip);
            }
            let s = comps.first().ok_or(Halt::Skip)?;
            let masks = nonzero.get_mut(usize::from(s.index)).ok_or(Halt::Skip)?;
            let blocks = usize::try_from(mcus).map_err(|_| Halt::Skip)?;
            if masks.is_empty() {
                *masks = vec![0u64; blocks];
            }
            if masks.len() != blocks {
                return Err(Halt::Skip);
            }
            return self.run(mode, &used, &limits, mcus, layout, Some(masks));
        }
        self.run(mode, &used, &limits, mcus, layout, None)
    }

    fn run(
        &mut self,
        mode: Mode,
        tables: &Tables,
        limits: &Limits,
        mcus: u64,
        layout: Layout,
        mut masks: Option<&mut Vec<u64>>,
    ) -> Result<u64, Halt> {
        let comps = self.header.components();
        let band = u64::from(self.header.se.saturating_sub(self.header.ss)) + 1;
        let mut pred = [0i32; MAX_COMPONENTS];
        let mut eobrun: u32 = 0;
        let mut since_restart: u64 = 0;
        for m in 0..mcus {
            if self.restart_interval != 0 && since_restart == u64::from(self.restart_interval) {
                // RSTn after every `restart_interval` MCUs (not after the
                // last); an end-of-band run never crosses one
                if eobrun != 0 {
                    return Err(Halt::Broken);
                }
                let at = self.bits.at_marker(self.src).ok_or(Halt::Broken)?;
                let (code, after) = marker_code(self.src, at).ok_or(Halt::Broken)?;
                if code != 0xD0 + *self.next_rst {
                    return Err(Halt::Broken);
                }
                *self.next_rst = (*self.next_rst + 1) & 7;
                *self.bits = Bits::new(after);
                pred = [0; MAX_COMPONENTS];
                since_restart = 0;
            }
            since_restart += 1;
            for (i, s) in comps.iter().enumerate() {
                let c = self
                    .frame
                    .components()
                    .get(usize::from(s.index))
                    .ok_or(Halt::Skip)?;
                let count = match layout {
                    Layout::Single => 1,
                    Layout::Interleaved => u32::from(c.h) * u32::from(c.v),
                };
                let dc = tables.dc.get(i).copied().flatten();
                let ac = tables.ac.get(i).copied().flatten();
                let p = pred.get_mut(i).ok_or(Halt::Skip)?;
                for _ in 0..count {
                    let bits = &mut *self.bits;
                    let src = &mut *self.src;
                    let steps = match mode {
                        Mode::Sequential => {
                            dc_diff(bits, src, dc, p, limits)?;
                            ac_sequential(bits, src, ac, limits)?;
                            1
                        }
                        Mode::DcFirst => {
                            dc_diff(bits, src, dc, p, limits)?;
                            1
                        }
                        Mode::DcRefine => {
                            top_up(bits, src);
                            bits.get(1).ok_or(Halt::Broken)?;
                            1
                        }
                        Mode::AcFirst | Mode::AcRefine => {
                            let mask = masks
                                .as_deref_mut()
                                .and_then(|v| v.get_mut(usize::try_from(m).ok()?))
                                .ok_or(Halt::Skip)?;
                            if mode == Mode::AcFirst {
                                ac_first(bits, src, ac, self.header, limits, &mut eobrun, mask)?;
                                1
                            } else {
                                ac_refine(bits, src, ac, self.header, &mut eobrun, mask)?;
                                band
                            }
                        }
                    };
                    self.work.charge(steps, self.bits, self.start)?;
                }
            }
        }
        // the scan ends with its last MCU: an end-of-band run must not
        // reach beyond it, and a marker other than RSTn follows the padding
        if eobrun != 0 {
            return Err(Halt::Broken);
        }
        let at = self.bits.at_marker(self.src).ok_or(Halt::Broken)?;
        let (code, _) = marker_code(self.src, at).ok_or(Halt::Broken)?;
        if matches!(code, 0x00 | 0xD0..=0xD7) {
            return Err(Halt::Broken);
        }
        Ok(at)
    }
}

/// Why decoding stopped early.
enum Halt {
    /// Not decodable here; says nothing about the data.
    Skip,
    /// The data does not decode.
    Broken,
}

/// Per scan component: index into the frame's components and table ids.
#[derive(Clone, Copy, Default)]
struct ScanComponent {
    index: u8,
    td: u8,
    ta: u8,
}

struct ScanHeader {
    components: [ScanComponent; MAX_COMPONENTS],
    count: usize,
    ss: u8,
    se: u8,
    ah: u8,
    al: u8,
}

impl ScanHeader {
    fn parse(b: &[u8], frame: &FrameInfo) -> Option<ScanHeader> {
        let ns = usize::from(*b.first()?);
        if ns == 0 || ns > MAX_COMPONENTS {
            return None;
        }
        let mut components = [ScanComponent::default(); MAX_COMPONENTS];
        for (i, c) in b.get(1..1 + 2 * ns)?.chunks_exact(2).enumerate() {
            let index = frame.components().iter().position(|f| f.id == c[0])?;
            // each component at most once per scan
            if components
                .get(..i)?
                .iter()
                .any(|s| usize::from(s.index) == index)
            {
                return None;
            }
            *components.get_mut(i)? = ScanComponent {
                index: u8::try_from(index).ok()?,
                td: c[1] >> 4,
                ta: c[1] & 15,
            };
        }
        let ss = *b.get(1 + 2 * ns)?;
        let se = *b.get(2 + 2 * ns)?;
        let a = *b.get(3 + 2 * ns)?;
        Some(ScanHeader {
            components,
            count: ns,
            ss,
            se,
            ah: a >> 4,
            al: a & 15,
        })
    }

    fn components(&self) -> &[ScanComponent] {
        self.components.get(..self.count).unwrap_or(&[])
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sequential,
    DcFirst,
    DcRefine,
    AcFirst,
    AcRefine,
}

impl Mode {
    /// The kind of scan, or `None` for a header that T.81 does not allow.
    fn of(s: &ScanHeader, progressive: bool) -> Option<Mode> {
        if !progressive {
            return (s.ss == 0 && s.se == 63 && s.ah == 0 && s.al == 0).then_some(Mode::Sequential);
        }
        if s.al > 13 || (s.ah != 0 && s.ah != s.al + 1) {
            return None;
        }
        if s.ss == 0 {
            if s.se != 0 {
                return None;
            }
            return Some(if s.ah == 0 {
                Mode::DcFirst
            } else {
                Mode::DcRefine
            });
        }
        if s.se < s.ss || s.se > 63 || s.count != 1 {
            return None;
        }
        Some(if s.ah == 0 {
            Mode::AcFirst
        } else {
            Mode::AcRefine
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// One component, one block per MCU.
    Single,
    /// Several components, `h × v` blocks of each per MCU.
    Interleaved,
}

struct Tables<'a> {
    dc: [Option<&'a Huffman>; MAX_COMPONENTS],
    ac: [Option<&'a Huffman>; MAX_COMPONENTS],
}

struct Limits {
    /// Largest DC difference category.
    dc_max: u32,
    /// Largest AC coefficient size.
    ac_max: u32,
    /// Largest magnitude of a DC value.
    dc_limit: i32,
}

/// Sign-extends the `s`-bit magnitude category value `v` (T.81 F.2.2.1).
fn extend(v: u32, s: u32) -> i32 {
    if s == 0 {
        return 0;
    }
    let v = v as i32;
    if v < 1 << (s - 1) {
        v - (1 << s) + 1
    } else {
        v
    }
}

/// Refills when fewer than 32 bits are held (a code and its extra bits
/// take at most 31).
fn top_up(bits: &mut Bits, src: &mut Src) {
    if bits.n < 32 {
        bits.fill(src);
    }
}

fn dc_diff(
    bits: &mut Bits,
    src: &mut Src,
    table: Option<&Huffman>,
    pred: &mut i32,
    limits: &Limits,
) -> Result<(), Halt> {
    let table = table.ok_or(Halt::Skip)?;
    top_up(bits, src);
    let s = u32::from(table.decode(bits).ok_or(Halt::Broken)?);
    if s > limits.dc_max {
        return Err(Halt::Broken);
    }
    let v = bits.get(s).ok_or(Halt::Broken)?;
    *pred = pred.saturating_add(extend(v, s));
    if pred.unsigned_abs() > limits.dc_limit.unsigned_abs() {
        return Err(Halt::Broken);
    }
    Ok(())
}

fn ac_sequential(
    bits: &mut Bits,
    src: &mut Src,
    table: Option<&Huffman>,
    limits: &Limits,
) -> Result<(), Halt> {
    let table = table.ok_or(Halt::Skip)?;
    let mut k = 1u32;
    while k <= 63 {
        top_up(bits, src);
        let rs = table.decode(bits).ok_or(Halt::Broken)?;
        let (r, s) = (u32::from(rs >> 4), u32::from(rs & 15));
        if s == 0 {
            match r {
                0 => return Ok(()),
                // ZRL: sixteen zeros, which must leave room in the block
                15 if k + 15 <= 63 => k += 16,
                _ => return Err(Halt::Broken),
            }
            continue;
        }
        k += r;
        if k > 63 || s > limits.ac_max {
            return Err(Halt::Broken);
        }
        bits.get(s).ok_or(Halt::Broken)?;
        k += 1;
    }
    Ok(())
}

fn ac_first(
    bits: &mut Bits,
    src: &mut Src,
    table: Option<&Huffman>,
    scan: &ScanHeader,
    limits: &Limits,
    eobrun: &mut u32,
    mask: &mut u64,
) -> Result<(), Halt> {
    if *eobrun > 0 {
        *eobrun -= 1;
        return Ok(());
    }
    let table = table.ok_or(Halt::Skip)?;
    let se = u32::from(scan.se);
    let mut k = u32::from(scan.ss);
    while k <= se {
        top_up(bits, src);
        let rs = table.decode(bits).ok_or(Halt::Broken)?;
        let (r, s) = (u32::from(rs >> 4), u32::from(rs & 15));
        if s == 0 {
            if r == 15 {
                if k + 15 > se {
                    return Err(Halt::Broken);
                }
                k += 16;
                continue;
            }
            // EOBr: this block and 2^r - 1 + (r bits) more end here
            let extra = bits.get(r).ok_or(Halt::Broken)?;
            *eobrun = (1u32 << r) - 1 + extra;
            return Ok(());
        }
        k += r;
        if k > se || s > limits.ac_max {
            return Err(Halt::Broken);
        }
        bits.get(s).ok_or(Halt::Broken)?;
        *mask |= 1u64 << k;
        k += 1;
    }
    Ok(())
}

fn ac_refine(
    bits: &mut Bits,
    src: &mut Src,
    table: Option<&Huffman>,
    scan: &ScanHeader,
    eobrun: &mut u32,
    mask: &mut u64,
) -> Result<(), Halt> {
    let se = u32::from(scan.se);
    let mut k = u32::from(scan.ss);
    if *eobrun == 0 {
        let table = table.ok_or(Halt::Skip)?;
        while k <= se {
            top_up(bits, src);
            let rs = table.decode(bits).ok_or(Halt::Broken)?;
            let (mut r, s) = (i32::from(rs >> 4), u32::from(rs & 15));
            let new = match s {
                0 if r != 15 => {
                    // EOBr: the rest of this block is in the run
                    let extra = bits.get(r as u32).ok_or(Halt::Broken)?;
                    *eobrun = (1u32 << r) + extra;
                    break;
                }
                // ZRL
                0 => false,
                // the sign of a newly non-zero coefficient
                1 => {
                    bits.get(1).ok_or(Halt::Broken)?;
                    true
                }
                _ => return Err(Halt::Broken),
            };
            // over non-zero coefficients (one correction bit each) and r
            // zero ones, to the position of the new coefficient
            loop {
                if k > se {
                    break;
                }
                if *mask & (1u64 << k) != 0 {
                    top_up(bits, src);
                    bits.get(1).ok_or(Halt::Broken)?;
                } else {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                }
                k += 1;
            }
            if new {
                if k > se {
                    return Err(Halt::Broken);
                }
                *mask |= 1u64 << k;
            } else if k > se {
                // a ZRL running past the band
                return Err(Halt::Broken);
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        // correction bits for the non-zero coefficients left in the band
        while k <= se {
            if *mask & (1u64 << k) != 0 {
                top_up(bits, src);
                bits.get(1).ok_or(Halt::Broken)?;
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}

//! [`AlignedBuf`]: the buffer every device read goes into.

use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// A zero-initialised heap buffer whose address is a multiple of 4096 bytes.
///
/// Windows asks for sector-aligned buffers on raw devices ("should be
/// physical sector-aligned"); 4096 covers every physical sector size in use,
/// and page alignment lets the storage class driver move a 64 KiB request in
/// one piece. A sub-slice that starts at a multiple of the sector size is
/// sector-aligned as well, which is what the per-sector retries of
/// [`read_resilient`](crate::read_resilient) rely on.
///
/// ```
/// let mut buf = hds_io::AlignedBuf::new(8 * 1024 * 1024);
/// assert_eq!(buf.as_ptr() as usize % hds_io::AlignedBuf::ALIGN, 0);
/// buf[..4].copy_from_slice(b"FILE");
/// assert_eq!(&buf[..5], b"FILE\0");
/// ```
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: the buffer owns its allocation exclusively, like `Box<[u8]>`.
unsafe impl Send for AlignedBuf {}
// SAFETY: shared access only hands out `&[u8]`.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Alignment of the buffer start in bytes.
    pub const ALIGN: usize = 4096;

    /// Allocates `len` zero bytes. `len` may be 0 (nothing is allocated).
    ///
    /// # Panics
    ///
    /// Panics when `len` exceeds what an allocation can hold (`isize::MAX`
    /// rounded down to the alignment) and aborts when the system is out of
    /// memory, as `Vec` does.
    pub fn new(len: usize) -> AlignedBuf {
        if len == 0 {
            // A dangling but well-aligned, non-null address for the empty slice.
            const DANGLING: NonZeroUsize = match NonZeroUsize::new(AlignedBuf::ALIGN) {
                Some(address) => address,
                None => panic!("ALIGN is not zero"),
            };
            return AlignedBuf {
                ptr: NonNull::without_provenance(DANGLING),
                len: 0,
            };
        }
        let layout = Layout::from_size_align(len, Self::ALIGN)
            .expect("AlignedBuf: length exceeds isize::MAX");
        // SAFETY: `layout` has a non-zero size.
        let raw = unsafe { alloc_zeroed(layout) };
        match NonNull::new(raw) {
            Some(ptr) => AlignedBuf { ptr, len },
            None => handle_alloc_error(layout),
        }
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True for a buffer of length 0.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The whole buffer.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `ptr` points to `len` initialised bytes owned by `self` (or is a
        // well-aligned dangling pointer for `len == 0`).
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }

    /// The whole buffer, writable.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as in `as_slice`; `&mut self` guarantees exclusive access.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if self.len != 0 {
            // SAFETY: allocated in `new` with exactly this layout.
            unsafe {
                dealloc(
                    self.ptr.as_ptr(),
                    Layout::from_size_align_unchecked(self.len, Self::ALIGN),
                )
            }
        }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl DerefMut for AlignedBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlignedBuf")
            .field("len", &self.len)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::AlignedBuf;

    #[test]
    fn is_aligned_zeroed_and_writable() {
        for len in [1usize, 511, 512, 4096, 65536, 8 << 20] {
            let mut buf = AlignedBuf::new(len);
            assert_eq!(buf.len(), len);
            assert_eq!(buf.as_ptr() as usize % AlignedBuf::ALIGN, 0);
            assert!(buf.iter().all(|&b| b == 0));
            buf[len - 1] = 0xA5;
            buf[0] = 0x5A;
            assert_eq!(buf.as_slice()[len - 1], if len == 1 { 0x5A } else { 0xA5 });
            // A sub-slice at a sector multiple is sector-aligned.
            if len > 512 {
                assert_eq!(buf[512..].as_ptr() as usize % 512, 0);
            }
        }
    }

    #[test]
    fn empty_buffer_allocates_nothing() {
        let mut buf = AlignedBuf::new(0);
        assert!(buf.is_empty());
        assert_eq!(buf.as_slice(), &[] as &[u8]);
        assert_eq!(buf.as_mut_slice().len(), 0);
        assert_eq!(buf.as_ptr() as usize % AlignedBuf::ALIGN, 0);
        assert_eq!(format!("{buf:?}"), "AlignedBuf { len: 0 }");
    }

    #[test]
    fn can_move_between_threads() {
        let mut buf = AlignedBuf::new(4096);
        buf[7] = 7;
        let handle = std::thread::spawn(move || buf[7]);
        assert_eq!(handle.join().unwrap(), 7);
    }
}

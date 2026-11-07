//! Model memory: owned, allocated fallibly, and reusable across streams.

use alloc_crate::alloc::{Layout, alloc, alloc_zeroed};
use alloc_crate::boxed::Box;
use alloc_crate::vec::Vec;

use crate::error::{Error, ErrorKind, Result};

/// The memory a model's sub-allocator works in.
///
/// Every codec owns one. `into_arena` takes it back out when a stream is
/// done, and `with_arena` hands it to the next codec, so a container
/// decoding many folders or blocks allocates (and faults in) the arena once.
/// The model's layout depends only on its parameters, never on how large
/// the arena behind it is, so a reused arena decodes exactly as a fresh one.
///
/// Allocation never aborts: an arena the allocator cannot provide is
/// [`ErrorKind::AllocationFailed`]. The memory starts zeroed, lazily where
/// the platform allows, so an arena larger than a stream needs costs
/// address space, not resident memory, until it is used.
pub struct Arena {
    pub(crate) buf: Vec<u8>,
}

impl Arena {
    /// Allocates `bytes` of zeroed memory.
    ///
    /// Errors: [`ErrorKind::AllocationFailed`] when the allocator refuses,
    /// or `bytes` does not fit the address space.
    pub fn try_new(bytes: u64) -> Result<Self> {
        Ok(Self {
            buf: try_zeroed(bytes)?,
        })
    }

    /// The bytes this arena holds.
    pub fn capacity(&self) -> u64 {
        self.buf.capacity() as u64
    }

    /// An arena with no memory, for codecs that have none yet.
    pub(crate) const fn empty() -> Self {
        Self { buf: Vec::new() }
    }
}

impl core::fmt::Debug for Arena {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Arena")
            .field("capacity", &self.capacity())
            .finish()
    }
}

#[cold]
fn alloc_failed(bytes: u64) -> Error {
    Error::new(ErrorKind::AllocationFailed { bytes })
}

/// `bytes` of zeroed memory as a `Vec` whose length is its capacity, or
/// [`ErrorKind::AllocationFailed`].
pub(crate) fn try_zeroed(bytes: u64) -> Result<Vec<u8>> {
    let len = usize::try_from(bytes).map_err(|_| alloc_failed(bytes))?;
    if len == 0 {
        return Ok(Vec::new());
    }
    let layout = Layout::array::<u8>(len).map_err(|_| alloc_failed(bytes))?;
    // SAFETY: `layout` has a non-zero size (`len > 0`).
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        return Err(alloc_failed(bytes));
    }
    advise_huge_pages(ptr, len);
    // SAFETY: `ptr` came from the global allocator, which `Vec` uses, with
    // the layout of `[u8; len]` (alignment 1, `len` bytes); every byte is
    // initialized (zeroed); length and capacity are both `len`.
    Ok(unsafe { Vec::from_raw_parts(ptr, len, len) })
}

/// Asks Linux to back the whole 2 MiB pages inside a fresh allocation with
/// transparent huge pages, as 7-Zip's `BigAlloc` does for its PPMd arena.
///
/// The model reaches all over its arena, so with 4 KiB pages a large arena
/// misses the TLB on a good share of its accesses; on a kernel whose
/// huge-page mode is `madvise` (the common default) only an advised range
/// gets huge pages. The advice changes how the kernel backs the memory,
/// never its contents, and the pages are still faulted in lazily, now
/// 2 MiB at a time. A kernel that refuses the advice leaves the arena as it
/// was, so the result is ignored. The head and tail outside whole huge
/// pages keep small pages.
#[cfg(all(target_os = "linux", feature = "std", not(miri)))]
fn advise_huge_pages(ptr: *mut u8, len: usize) {
    const HUGE_PAGE: usize = 2 << 20;
    // `MADV_HUGEPAGE` in `<linux/mman.h>`, the same value on every
    // architecture.
    const MADV_HUGEPAGE: core::ffi::c_int = 14;
    unsafe extern "C" {
        fn madvise(
            addr: *mut core::ffi::c_void,
            len: usize,
            advice: core::ffi::c_int,
        ) -> core::ffi::c_int;
    }
    let addr = ptr as usize;
    let Some(start) = addr.checked_next_multiple_of(HUGE_PAGE) else {
        return;
    };
    let end = (addr + len) & !(HUGE_PAGE - 1);
    if end <= start {
        return;
    }
    let first = ptr.wrapping_add(start - addr);
    // SAFETY: `[start, end)` lies inside the `len`-byte allocation at
    // `ptr`, starts on a page boundary, and is a whole number of pages.
    // `MADV_HUGEPAGE` reads and writes no byte of it: it only sets how the
    // kernel backs the range, and the bytes read the same either way.
    let _ = unsafe { madvise(first.cast(), end - start, MADV_HUGEPAGE) };
}

#[cfg(not(all(target_os = "linux", feature = "std", not(miri))))]
fn advise_huge_pages(_ptr: *mut u8, _len: usize) {}

/// Grows or shrinks `buf` to exactly `len` bytes, reusing its allocation
/// when its capacity already covers `len`, otherwise allocating a fresh
/// zeroed one (the old one is freed only once the new one exists).
pub(crate) fn fit(buf: &mut Vec<u8>, len: usize) -> Result<()> {
    if buf.capacity() >= len {
        if buf.len() >= len {
            buf.truncate(len);
        } else {
            // Within capacity: no allocation.
            buf.resize(len, 0);
        }
        return Ok(());
    }
    let fresh = try_zeroed(len as u64)?;
    *buf = fresh;
    Ok(())
}

/// `Box::new` that reports a refused allocation instead of aborting.
pub(crate) fn try_box<T>(value: T) -> Result<Box<T>> {
    let layout = Layout::new::<T>();
    if layout.size() == 0 {
        return Ok(Box::new(value));
    }
    // SAFETY: `layout` has a non-zero size.
    let ptr = unsafe { alloc(layout) }.cast::<T>();
    if ptr.is_null() {
        return Err(alloc_failed(layout.size() as u64));
    }
    // SAFETY: `ptr` is a fresh, suitably aligned allocation of `T`'s layout
    // from the global allocator; writing `value` initializes it, and
    // `Box::from_raw` takes ownership with the same layout and allocator.
    unsafe {
        ptr.write(value);
        Ok(Box::from_raw(ptr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_size_is_an_error_not_an_abort() {
        let e = Arena::try_new(u64::MAX).unwrap_err();
        assert_eq!(e.kind, ErrorKind::AllocationFailed { bytes: u64::MAX });
        let e = Arena::try_new(isize::MAX as u64 + 1).unwrap_err();
        assert!(matches!(e.kind, ErrorKind::AllocationFailed { .. }));
    }

    #[test]
    fn arenas_start_zeroed_and_fit_reuses_capacity() {
        let arena = Arena::try_new(4096).unwrap();
        assert_eq!(arena.capacity(), 4096);
        assert!(arena.buf.iter().all(|&b| b == 0));
        let mut buf = arena.buf;
        let addr = buf.as_ptr();
        fit(&mut buf, 1000).unwrap();
        assert_eq!((buf.len(), buf.as_ptr()), (1000, addr));
        fit(&mut buf, 4096).unwrap();
        assert_eq!((buf.len(), buf.as_ptr()), (4096, addr));
        fit(&mut buf, 8192).unwrap();
        assert_eq!(buf.len(), 8192);
        assert!(Arena::try_new(0).unwrap().capacity() == 0);
    }

    #[test]
    fn arenas_around_huge_page_sizes_are_zeroed_and_whole() {
        const HUGE: u64 = 2 << 20;
        for bytes in [1, HUGE - 1, HUGE, HUGE + 1, 5 * HUGE + 3] {
            let mut buf = try_zeroed(bytes).unwrap();
            assert_eq!(buf.len() as u64, bytes);
            assert!(buf.iter().all(|&b| b == 0), "{bytes} bytes not zeroed");
            let last = buf.len() - 1;
            buf[last] = 0x5A;
            assert_eq!(buf[last], 0x5A);
            buf[0] = 0xA5;
            assert_eq!(buf[0], 0xA5);
        }
    }
}

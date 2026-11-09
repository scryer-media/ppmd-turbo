//! Model memory: owned, allocated fallibly, and reusable across streams, or
//! supplied by the caller.

use alloc_crate::alloc::{Layout, alloc, alloc_zeroed};
use alloc_crate::boxed::Box;
use alloc_crate::sync::Arc;
use alloc_crate::vec::Vec;
use core::panic::AssertUnwindSafe;
use core::ptr::NonNull;

use crate::error::{Error, ErrorKind, Result};

/// Memory an [`ArenaAllocator`] lends a model for its arena.
///
/// The crate calls [`ArenaMemory::as_mut_ptr`] and [`ArenaMemory::len`]
/// once each, when it receives the value, and reads and writes through that
/// pointer until it drops the value: when the codec is dropped, when a
/// restart needs an arena of another size, or when the [`Arena`] holding it
/// is dropped.
///
/// # Safety
///
/// An implementation must guarantee all of the following, from the call to
/// [`ArenaMemory::as_mut_ptr`] until the value is dropped:
///
/// - the pointer is non-null and valid for reads and writes of `len()`
///   bytes, which is at least the `len` [`ArenaAllocator::allocate`] was
///   asked for; the crate uses exactly the first `len` asked for;
/// - those bytes are initialized, to any value: they need not be zero;
/// - the address is stable until drop, including when the `Box` holding
///   the value moves: the memory is not moved, unmapped, remapped or freed;
/// - the memory is exclusive to the crate until drop: nothing else reads
///   or writes it, as with a `&mut [u8]`.
///
/// Dropping the value releases the memory; the crate does not touch it
/// afterwards. No result depends on a byte the model has not written, so
/// the starting contents do not matter: garbage decodes exactly as zeros
/// do. (The sub-allocator's glue pass reads the two bytes after the guard
/// stamp it writes at the free-space boundary and discards them, as
/// 7-Zip's does, which is why the bytes must be initialized, though not to
/// any value.)
pub unsafe trait ArenaMemory: Send {
    /// The start of the memory. Called once.
    fn as_mut_ptr(&mut self) -> *mut u8;

    /// How many bytes [`ArenaMemory::as_mut_ptr`] points at: at least the
    /// `len` asked for. Called once.
    fn len(&self) -> usize;

    /// Whether [`ArenaMemory::len`] is zero.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The error an [`ArenaAllocator`] returns.
///
/// It is boxed, not an `std::io::Error`, because the crate builds without
/// `std` and the allocator API is part of that build. An `io::Error`
/// converts into it with `?` or `.into()`, and with the `std` feature its
/// `io::ErrorKind` reaches the caller in
/// [`ErrorKind::AllocationFailed`] (and through `io::Error::from`). The
/// error itself is dropped: an [`Error`] is `Copy` and cannot hold it, so
/// the allocator logs or keeps any other detail.
pub type ArenaAllocError = Box<dyn core::error::Error + Send + Sync>;

/// A source of caller memory for model arenas; see
/// [`Arena::with_allocator`].
///
/// Asked once for each arena a model needs that is over the in-RAM limit
/// and at or under the ceiling ([`Arena::with_max_lent`]): when a codec is
/// built, and again when a reset or a RAR block restart needs an arena of
/// another size, larger or smaller, that the memory in hand cannot back.
///
/// # Concurrency
///
/// [`ArenaAllocator::allocate`] can be called from any thread, and from
/// several at once when codecs sharing one allocator run on several
/// threads. The crate starts no threads of its own: a codec asks on the
/// thread that calls it, and holds at most one lent arena at a time, the
/// new one being asked for while the old one is still held.
pub trait ArenaAllocator: Send + Sync {
    /// Returns memory for an arena of `len` bytes, or the error that
    /// stopped it, which the crate reports as
    /// [`ErrorKind::AllocationFailed`]. Memory shorter than `len`, longer
    /// than `isize::MAX`, or at a null address is
    /// [`ErrorKind::ArenaMemoryLength`] and is dropped at once.
    ///
    /// # Errors
    ///
    /// Whatever stops the allocator: no disk space, a failed map, a quota.
    fn allocate(&self, len: usize) -> core::result::Result<Box<dyn ArenaMemory>, ArenaAllocError>;
}

/// Where arenas above an in-RAM limit come from.
pub(crate) struct ArenaSource {
    ram_limit: u64,
    max_lent: u64,
    // `AssertUnwindSafe` keeps the codecs that hold a source `UnwindSafe`
    // and `RefUnwindSafe`, as they are without one; the crate only ever
    // calls `allocate`, and keeps no state of the allocator's.
    allocator: AssertUnwindSafe<Arc<dyn ArenaAllocator>>,
}

impl Clone for ArenaSource {
    fn clone(&self) -> Self {
        Self {
            ram_limit: self.ram_limit,
            max_lent: self.max_lent,
            allocator: AssertUnwindSafe(Arc::clone(&self.allocator)),
        }
    }
}

impl ArenaSource {
    /// Whether an arena of `len` bytes is the allocator's to provide.
    fn lends(&self, len: usize) -> bool {
        len as u64 > self.ram_limit
    }

    /// `Err` with [`ErrorKind::MemoryLimit`] when an arena of `len` bytes
    /// would be lent and is over the ceiling.
    pub(crate) fn admit(&self, len: usize) -> Result<()> {
        if self.lends(len) && len as u64 > self.max_lent {
            return Err(Error::new(ErrorKind::memory_limit(
                len as u64,
                self.max_lent,
            )));
        }
        Ok(())
    }

    /// Asks the allocator for `len` bytes and checks what it gave back
    /// against the part of the [`ArenaMemory`] contract it can check. A
    /// length over the ceiling or over `isize::MAX` is refused without
    /// asking; memory that fails the check is dropped before the error
    /// returns.
    fn lend(&self, len: usize) -> Result<Lent> {
        self.admit(len)?;
        if isize::try_from(len).is_err() {
            return Err(alloc_failed(len as u64));
        }
        let mut mem = self
            .allocator
            .allocate(len)
            .map_err(|e| refused(len as u64, &*e))?;
        let ptr = mem.as_mut_ptr();
        let provided = mem.len();
        // A length over `isize::MAX` cannot describe real memory: refused
        // before any slice is built over it.
        match NonNull::new(ptr) {
            Some(ptr) if provided >= len && isize::try_from(provided).is_ok() => Ok(Lent {
                ptr,
                len,
                _owner: AssertUnwindSafe(mem),
            }),
            _ => Err(Error::new(ErrorKind::ArenaMemoryLength {
                requested: len as u64,
                provided: provided as u64,
            })),
        }
    }
}

/// [`ErrorKind::AllocationFailed`] for an allocator's error, keeping an
/// `io::Error`'s kind.
#[cold]
fn refused(bytes: u64, e: &(dyn core::error::Error + Send + Sync + 'static)) -> Error {
    #[cfg(feature = "std")]
    let kind = ErrorKind::AllocationFailed {
        bytes,
        io_kind: e.downcast_ref::<std::io::Error>().map(std::io::Error::kind),
    };
    #[cfg(not(feature = "std"))]
    let kind = {
        let _ = e;
        ErrorKind::allocation_failed(bytes)
    };
    Error::new(kind)
}

/// Caller memory, checked once when the allocator returned it: `len` (the
/// length asked for, which the arena uses) bytes at `ptr`, owned by
/// `_owner`.
pub(crate) struct Lent {
    ptr: NonNull<u8>,
    len: usize,
    _owner: AssertUnwindSafe<Box<dyn ArenaMemory>>,
}

// SAFETY: the owner is `Send` (`ArenaMemory: Send`), and `ptr` points into
// memory the contract makes exclusive to its holder, so sending the value
// sends sole access with it.
unsafe impl Send for Lent {}
// SAFETY: a `&Lent` gives out only the copied `ptr` and `len`; nothing
// reaches the owner through it, and writing through `ptr` takes the
// arena's `&mut`.
unsafe impl Sync for Lent {}

impl Lent {
    /// The start of the memory.
    pub(crate) fn ptr(&self) -> NonNull<u8> {
        self.ptr
    }

    /// The bytes the arena may use: the length asked for.
    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

/// The memory behind an arena.
pub(crate) enum Storage {
    /// The crate's own zeroed allocation, as a `Vec` whose length is the
    /// arena in use.
    Owned(Vec<u8>),
    /// The caller's memory, the length it was asked for available to the
    /// arena.
    Caller(Lent),
}

impl Storage {
    /// Bytes the storage can hold.
    pub(crate) fn capacity(&self) -> usize {
        match self {
            Self::Owned(buf) => buf.capacity(),
            Self::Caller(lent) => lent.len(),
        }
    }
}

/// `len` bytes of fresh storage: the caller's memory when `source` is set
/// and `len` is above its in-RAM limit, otherwise a zeroed allocation.
pub(crate) fn fresh_storage(len: usize, source: Option<&ArenaSource>) -> Result<Storage> {
    match source {
        Some(src) if src.lends(len) => src.lend(len).map(Storage::Caller),
        _ => try_zeroed(len as u64).map(Storage::Owned),
    }
}

/// The memory a model's sub-allocator works in.
///
/// Every codec owns one. `into_arena` takes it back out when a stream is
/// done, and `with_arena` hands it to the next codec, so a container
/// decoding many folders or blocks allocates (and faults in) the arena once.
/// The model's layout depends only on its parameters, never on how large
/// the arena behind it is, so a reused arena decodes exactly as a fresh one.
///
/// Allocation never aborts: an arena the allocator cannot provide is
/// [`ErrorKind::AllocationFailed`]. The crate's own memory starts zeroed,
/// lazily where the platform allows, so an arena larger than a stream needs
/// costs address space, not resident memory, until it is used. An arena
/// built [`with_allocator`](Self::with_allocator) takes memory above its
/// in-RAM limit from the caller instead.
pub struct Arena {
    pub(crate) storage: Storage,
    pub(crate) source: Option<ArenaSource>,
}

impl Arena {
    /// The default ceiling on one lent arena: the largest arena a model can
    /// lay out, for [`Params::MAX_MEM`](crate::Params::MAX_MEM).
    pub const DEFAULT_MAX_LENT: u64 =
        crate::alloc::Arena::arena_bytes(crate::Params::MAX_MEM) as u64;

    /// Allocates `bytes` of zeroed memory.
    ///
    /// Errors: [`ErrorKind::AllocationFailed`] when the allocator refuses,
    /// or `bytes` does not fit the address space.
    pub fn try_new(bytes: u64) -> Result<Self> {
        Ok(Self {
            storage: Storage::Owned(try_zeroed(bytes)?),
            source: None,
        })
    }

    /// An empty arena that takes a model's memory from the crate's own
    /// allocation, as today, when the arena is `ram_limit` bytes or less,
    /// and from `allocator` when it is larger, up to the ceiling
    /// ([`with_max_lent`](Self::with_max_lent), by default
    /// [`DEFAULT_MAX_LENT`](Self::DEFAULT_MAX_LENT)). The limits count the
    /// arena alone: the memory size plus up to three bytes of alignment.
    ///
    /// A codec built over it keeps the rule for every later restart, and
    /// its `into_arena` hands the rule on with the memory. Caller memory is
    /// never zeroed, advised or read before the model writes it.
    pub fn with_allocator(ram_limit: u64, allocator: Arc<dyn ArenaAllocator>) -> Self {
        Self {
            storage: Storage::Owned(Vec::new()),
            source: Some(ArenaSource {
                ram_limit,
                max_lent: Self::DEFAULT_MAX_LENT,
                allocator: AssertUnwindSafe(allocator),
            }),
        }
    }

    /// Sets the largest arena the allocator is asked for. A model whose
    /// arena is over the in-RAM limit and over `max_lent` is refused with
    /// [`ErrorKind::MemoryLimit`], and the allocator is never asked. No
    /// effect on an arena without an allocator.
    #[must_use]
    pub fn with_max_lent(mut self, max_lent: u64) -> Self {
        if let Some(src) = &mut self.source {
            src.max_lent = max_lent;
        }
        self
    }

    /// The bytes this arena holds.
    pub fn capacity(&self) -> u64 {
        self.storage.capacity() as u64
    }

    /// Whether the memory is the caller's, from an [`ArenaAllocator`].
    pub fn is_caller_memory(&self) -> bool {
        matches!(self.storage, Storage::Caller(_))
    }

    /// An arena with no memory, for codecs that have none yet.
    pub(crate) const fn empty() -> Self {
        Self::with_source(None)
    }

    /// An arena with no memory and `source`'s allocator rule.
    pub(crate) const fn with_source(source: Option<ArenaSource>) -> Self {
        Self {
            storage: Storage::Owned(Vec::new()),
            source,
        }
    }
}

impl core::fmt::Debug for Arena {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Arena")
            .field("capacity", &self.capacity())
            .field("caller_memory", &self.is_caller_memory())
            .field("ram_limit", &self.source.as_ref().map(|s| s.ram_limit))
            .field("max_lent", &self.source.as_ref().map(|s| s.max_lent))
            .finish()
    }
}

#[cold]
fn alloc_failed(bytes: u64) -> Error {
    Error::new(ErrorKind::allocation_failed(bytes))
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
        assert_eq!(e.kind, ErrorKind::allocation_failed(u64::MAX));
        let e = Arena::try_new(isize::MAX as u64 + 1).unwrap_err();
        assert!(matches!(e.kind, ErrorKind::AllocationFailed { .. }));
    }

    #[test]
    fn arenas_start_zeroed_and_fit_reuses_capacity() {
        let arena = Arena::try_new(4096).unwrap();
        assert_eq!(arena.capacity(), 4096);
        let Storage::Owned(mut buf) = arena.storage else {
            panic!("try_new is the crate's own memory");
        };
        assert!(buf.iter().all(|&b| b == 0));
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
    #[cfg_attr(
        miri,
        ignore = "megabytes read a byte at a time; the advice is compiled out under Miri"
    )]
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

/// Heap-backed caller memory and allocators for the caller-memory tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Bytes from a xorshift generator: garbage for caller memory.
    pub(crate) fn garbage(len: usize, mut seed: u32) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                (seed >> 24) as u8
            })
            .collect()
    }

    /// Caller memory on the heap, counting its drops and the calls to
    /// `as_mut_ptr` and `len`.
    pub(crate) struct HeapMemory {
        ptr: *mut u8,
        len: usize,
        /// What `len` reports, when it is not the allocation's length.
        claimed: Option<usize>,
        drops: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }

    impl HeapMemory {
        pub(crate) fn new(
            bytes: Vec<u8>,
            drops: Arc<AtomicUsize>,
            calls: Arc<AtomicUsize>,
        ) -> Self {
            let len = bytes.len();
            let ptr = Box::into_raw(bytes.into_boxed_slice()).cast::<u8>();
            Self {
                ptr,
                len,
                claimed: None,
                drops,
                calls,
            }
        }
    }

    // SAFETY: the memory is a heap allocation this value owns alone.
    unsafe impl Send for HeapMemory {}

    // SAFETY: `ptr` is a live, initialized `len`-byte allocation owned by
    // this value until it drops; the pointer never changes, and moving the
    // value does not move the allocation. A `claimed` length breaks the
    // contract on purpose, for tests that the crate refuses it before
    // using the memory.
    unsafe impl ArenaMemory for HeapMemory {
        fn as_mut_ptr(&mut self) -> *mut u8 {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.ptr
        }
        fn len(&self) -> usize {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.claimed.unwrap_or(self.len)
        }
    }

    impl Drop for HeapMemory {
        fn drop(&mut self) {
            // SAFETY: `ptr` and `len` are the boxed slice leaked in `new`.
            drop(unsafe { Box::from_raw(core::ptr::slice_from_raw_parts_mut(self.ptr, self.len)) });
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// What a [`TestAllocator`] hands back.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Mode {
        /// Memory of the asked length, filled with garbage.
        Garbage,
        /// An error.
        Fail,
        /// An `io::Error` of this kind.
        #[cfg(feature = "std")]
        Io(std::io::ErrorKind),
        /// Garbage memory 61 bytes longer than asked.
        Longer,
        /// Memory one byte shorter than asked.
        Shorter,
        /// Memory claiming `isize::MAX + 1` bytes.
        Huge,
    }

    /// An allocator that counts its allocations and their drops.
    pub(crate) struct TestAllocator {
        mode: Mode,
        pub(crate) allocs: AtomicUsize,
        pub(crate) drops: Arc<AtomicUsize>,
        /// Calls to `as_mut_ptr` and `len` on the memory it lent.
        pub(crate) calls: Arc<AtomicUsize>,
    }

    impl TestAllocator {
        pub(crate) fn new(mode: Mode) -> Arc<Self> {
            Arc::new(Self {
                mode,
                allocs: AtomicUsize::new(0),
                drops: Arc::new(AtomicUsize::new(0)),
                calls: Arc::new(AtomicUsize::new(0)),
            })
        }

        pub(crate) fn allocs(&self) -> usize {
            self.allocs.load(Ordering::SeqCst)
        }

        pub(crate) fn drops(&self) -> usize {
            self.drops.load(Ordering::SeqCst)
        }
    }

    #[derive(Debug)]
    struct Refused;

    impl core::fmt::Display for Refused {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("refused")
        }
    }

    impl core::error::Error for Refused {}

    impl ArenaAllocator for TestAllocator {
        fn allocate(
            &self,
            len: usize,
        ) -> core::result::Result<Box<dyn ArenaMemory>, ArenaAllocError> {
            let n = self.allocs.fetch_add(1, Ordering::SeqCst) as u32;
            let (len, claimed) = match self.mode {
                Mode::Fail => return Err(Box::new(Refused)),
                #[cfg(feature = "std")]
                Mode::Io(kind) => return Err(std::io::Error::new(kind, "scratch").into()),
                Mode::Garbage => (len, None),
                Mode::Longer => (len + 61, None),
                Mode::Shorter => (len - 1, None),
                Mode::Huge => (len, Some(isize::MAX as usize + 1)),
            };
            let bytes = garbage(len, 0x9E37_79B9 ^ n.wrapping_mul(0x85EB_CA6B) | 1);
            let mut mem = HeapMemory::new(bytes, self.drops.clone(), self.calls.clone());
            mem.claimed = claimed;
            Ok(Box::new(mem))
        }
    }
}

#[cfg(test)]
mod caller_memory_tests {
    use super::testing::{Mode, TestAllocator};
    use super::*;
    use crate::{Params, SevenZDecoder, SevenZEncoder, SevenZStatus};
    use core::sync::atomic::Ordering;

    /// Word salad over an invented vocabulary.
    fn text(len: usize) -> Vec<u8> {
        const WORDS: [&str; 12] = [
            "quorvel ",
            "tasp ",
            "minden ",
            "olbrat ",
            "ziv ",
            "pellumor ",
            "ka ",
            "drossen ",
            "ulf ",
            "brenno ",
            "saquith ",
            "lom\n",
        ];
        let mut out = Vec::with_capacity(len + 16);
        let mut seed = 0x2545_F491u32;
        while out.len() < len {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            out.extend_from_slice(WORDS[(seed % 12) as usize].as_bytes());
        }
        out.truncate(len);
        out
    }

    fn encode(params: Params, data: &[u8]) -> Vec<u8> {
        let mut enc = SevenZEncoder::new(params).unwrap();
        let mut out = vec![0u8; data.len() * 2 + 64];
        let step = enc.encode(data, &mut out).unwrap();
        assert_eq!(step.consumed, data.len());
        let mut len = step.produced;
        loop {
            let fin = enc.finish(&mut out[len..], false).unwrap();
            len += fin.produced;
            if fin.done {
                break;
            }
        }
        out.truncate(len);
        out
    }

    fn decode(dec: &mut SevenZDecoder, stream: &[u8], len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let step = dec.decode(stream, true, &mut out).unwrap();
        assert_eq!(step.status, SevenZStatus::ReachedSize);
        assert_eq!(step.produced, len);
        out
    }

    /// The smallest arena at the highest order restarts every few hundred
    /// symbols; small enough for Miri.
    fn restarting() -> (Params, Vec<u8>, Vec<u8>) {
        let params = Params::new(64, Params::MIN_MEM).unwrap();
        let data = text(if cfg!(miri) { 3000 } else { 200_000 });
        let stream = encode(params, &data);
        (params, data, stream)
    }

    #[test]
    fn garbage_caller_memory_decodes_7z_as_owned_memory_does_through_restarts() {
        let (params, data, stream) = restarting();
        let len = data.len();

        let mut owned = SevenZDecoder::new(params, Some(len as u64)).unwrap();
        let owned_out = decode(&mut owned, &stream, len);
        assert!(!owned.into_arena().is_caller_memory());

        let alloc = TestAllocator::new(Mode::Garbage);
        let arena = Arena::with_allocator(0, alloc.clone());
        let mut dec = SevenZDecoder::with_arena(params, Some(len as u64), arena).unwrap();
        let out = decode(&mut dec, &stream, len);
        assert!(
            dec.model_restarts() > 2,
            "{} restarts",
            dec.model_restarts()
        );
        assert_eq!(out, owned_out);
        assert_eq!(out, data);
        assert_eq!((alloc.allocs(), alloc.drops()), (1, 0));
        assert_eq!(
            dec.memory_footprint(),
            Params::MIN_MEM as u64 + core::mem::size_of::<crate::model::Model>() as u64
        );

        // Handed back and reused: the same caller memory, no new allocation,
        // its old contents left over from the last stream.
        let arena = dec.into_arena();
        assert!(arena.is_caller_memory());
        let mut dec = SevenZDecoder::with_arena(params, Some(len as u64), arena).unwrap();
        assert_eq!(decode(&mut dec, &stream, len), data);
        assert_eq!((alloc.allocs(), alloc.drops()), (1, 0));
        drop(dec);
        assert_eq!(alloc.drops(), 1);
    }

    #[test]
    fn arenas_within_the_ram_limit_stay_the_crates_own() {
        let (params, data, stream) = restarting();
        let alloc = TestAllocator::new(Mode::Garbage);
        let arena = Arena::with_allocator(Params::MIN_MEM as u64, alloc.clone());
        let mut dec = SevenZDecoder::with_arena(params, Some(data.len() as u64), arena).unwrap();
        assert_eq!(decode(&mut dec, &stream, data.len()), data);
        assert_eq!(alloc.allocs(), 0);

        // A reset to an arena past the limit asks the allocator; the old
        // owned arena is freed.
        let big = Params::new(64, Params::MIN_MEM * 4).unwrap();
        let big_stream = encode(big, &data);
        dec.reset(big, Some(data.len() as u64)).unwrap();
        assert_eq!(decode(&mut dec, &big_stream, data.len()), data);
        assert_eq!((alloc.allocs(), alloc.drops()), (1, 0));
        // And back under it: the caller memory is too large to keep (more
        // than twice the need), so it goes and an owned arena replaces it.
        dec.reset(params, Some(data.len() as u64)).unwrap();
        assert_eq!(decode(&mut dec, &stream, data.len()), data);
        assert_eq!((alloc.allocs(), alloc.drops()), (1, 1));
        assert!(!dec.into_arena().is_caller_memory());
    }

    #[test]
    fn a_refused_allocation_is_an_error() {
        let params = Params::new(6, Params::MIN_MEM).unwrap();
        let alloc = TestAllocator::new(Mode::Fail);
        let arena = Arena::with_allocator(0, alloc.clone());
        let e = SevenZDecoder::with_arena(params, None, arena)
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::allocation_failed(Params::MIN_MEM as u64));
        assert!(e.is_resource_error());
        assert_eq!(alloc.allocs(), 1);

        // A reset that needs the allocator fails and leaves the decoder
        // poisoned, its arena as it was.
        let arena = Arena::with_allocator(Params::MIN_MEM as u64, alloc.clone());
        let mut dec = SevenZDecoder::with_arena(params, None, arena).unwrap();
        let big = Params::new(6, Params::MIN_MEM * 4).unwrap();
        let e = dec.reset(big, None).unwrap_err();
        assert!(matches!(e.kind, ErrorKind::AllocationFailed { .. }));
        assert_eq!(dec.decode(&[0; 8], true, &mut [0; 4]), Err(e));
        assert_eq!(
            dec.memory_footprint() - core::mem::size_of::<crate::model::Model>() as u64,
            Params::MIN_MEM as u64
        );
    }

    #[test]
    fn longer_memory_is_used_for_its_first_len_bytes_and_shorter_is_refused() {
        let (params, data, stream) = restarting();
        // Longer memory decodes as exact memory does, its pointer and
        // length read once each however many restarts follow.
        let alloc = TestAllocator::new(Mode::Longer);
        let arena = Arena::with_allocator(0, alloc.clone());
        let mut dec = SevenZDecoder::with_arena(params, Some(data.len() as u64), arena).unwrap();
        assert_eq!(decode(&mut dec, &stream, data.len()), data);
        assert!(dec.model_restarts() > 2);
        assert_eq!(alloc.calls.load(Ordering::SeqCst), 2);
        // The arena is the length asked for, not the memory's.
        let arena = dec.into_arena();
        assert_eq!(arena.capacity(), Params::MIN_MEM as u64);
        let mut dec = SevenZDecoder::with_arena(params, Some(data.len() as u64), arena).unwrap();
        assert_eq!(decode(&mut dec, &stream, data.len()), data);
        assert_eq!((alloc.allocs(), alloc.calls.load(Ordering::SeqCst)), (1, 2));
        drop(dec);
        assert_eq!(alloc.drops(), 1);

        let params = Params::new(6, Params::MIN_MEM + 1).unwrap();
        let alloc = TestAllocator::new(Mode::Shorter);
        let arena = Arena::with_allocator(0, alloc.clone());
        let e = SevenZDecoder::with_arena(params, None, arena)
            .err()
            .unwrap();
        // 2049 bytes lay out with three bytes of alignment.
        assert_eq!(
            e.kind,
            ErrorKind::ArenaMemoryLength {
                requested: 2052,
                provided: 2051
            }
        );
        assert!(!e.is_resource_error() && !e.is_data_error());
        assert_eq!((alloc.allocs(), alloc.drops()), (1, 1));
    }

    #[test]
    fn lengths_past_isize_max_are_refused_and_the_memory_released() {
        // Returned: refused before any slice is built, and dropped.
        let alloc = TestAllocator::new(Mode::Huge);
        let src = Arena::with_allocator(0, alloc.clone())
            .with_max_lent(u64::MAX)
            .source
            .unwrap();
        let e = fresh_storage(4096, Some(&src)).err().unwrap();
        assert_eq!(
            e.kind,
            ErrorKind::ArenaMemoryLength {
                requested: 4096,
                provided: isize::MAX as u64 + 1
            }
        );
        assert_eq!((alloc.allocs(), alloc.drops()), (1, 1));

        // Requested: refused without asking.
        let alloc = TestAllocator::new(Mode::Garbage);
        let src = Arena::with_allocator(0, alloc.clone())
            .with_max_lent(u64::MAX)
            .source
            .unwrap();
        let too_long = isize::MAX as usize + 1;
        let e = fresh_storage(too_long, Some(&src)).err().unwrap();
        assert_eq!(e.kind, ErrorKind::allocation_failed(too_long as u64));
        assert_eq!(alloc.allocs(), 0);
    }

    #[test]
    fn an_arena_over_the_ceiling_is_refused_without_asking() {
        let alloc = TestAllocator::new(Mode::Garbage);
        assert_eq!(
            Arena::with_allocator(0, alloc.clone())
                .source
                .unwrap()
                .max_lent,
            Arena::DEFAULT_MAX_LENT
        );
        assert_eq!(Arena::DEFAULT_MAX_LENT, 0xFFFF_FFDC);
        let params = Params::new(6, Params::MIN_MEM).unwrap();
        let ceiling = |max| Arena::with_allocator(0, alloc.clone()).with_max_lent(max);

        let e = SevenZDecoder::with_arena(params, None, ceiling(Params::MIN_MEM as u64 - 1))
            .err()
            .unwrap();
        assert_eq!(
            e.kind,
            ErrorKind::memory_limit(Params::MIN_MEM as u64, Params::MIN_MEM as u64 - 1)
        );
        assert!(e.is_resource_error());
        assert_eq!(alloc.allocs(), 0);

        // At the ceiling: lent.
        let dec = SevenZDecoder::with_arena(params, None, ceiling(Params::MIN_MEM as u64)).unwrap();
        assert!(dec.into_arena().is_caller_memory());
        assert_eq!(alloc.allocs(), 1);

        // Under the in-RAM limit the ceiling does not apply.
        let arena = Arena::with_allocator(Params::MIN_MEM as u64, alloc.clone()).with_max_lent(0);
        let mut dec = SevenZDecoder::with_arena(params, None, arena).unwrap();
        // A reset past both is refused, again without asking.
        let big = Params::new(6, Params::MIN_MEM * 2).unwrap();
        let e = dec.reset(big, None).unwrap_err();
        assert_eq!(
            e.kind,
            ErrorKind::memory_limit(Params::MIN_MEM as u64 * 2, 0)
        );
        assert_eq!(alloc.allocs(), 1);
    }

    #[cfg(feature = "std")]
    #[test]
    fn an_allocators_io_error_keeps_its_kind() {
        let params = Params::new(6, Params::MIN_MEM).unwrap();
        let alloc = TestAllocator::new(Mode::Io(std::io::ErrorKind::StorageFull));
        let arena = Arena::with_allocator(0, alloc.clone());
        let e = SevenZDecoder::with_arena(params, None, arena)
            .err()
            .unwrap();
        let ErrorKind::AllocationFailed { bytes, io_kind } = e.kind else {
            panic!("{e}");
        };
        assert_eq!(bytes, Params::MIN_MEM as u64);
        assert_eq!(io_kind, Some(std::io::ErrorKind::StorageFull));
        let io = std::io::Error::from(e);
        assert_eq!(io.kind(), std::io::ErrorKind::StorageFull);
        assert_eq!(io.downcast::<Error>().unwrap(), e);

        // Any other error is plain out-of-memory.
        let alloc = TestAllocator::new(Mode::Fail);
        let arena = Arena::with_allocator(0, alloc);
        let e = SevenZDecoder::with_arena(params, None, arena)
            .err()
            .unwrap();
        assert_eq!(e.kind, ErrorKind::allocation_failed(Params::MIN_MEM as u64));
        assert_eq!(
            std::io::Error::from(e).kind(),
            std::io::ErrorKind::OutOfMemory
        );
    }

    #[test]
    fn arenas_and_codecs_keep_their_auto_traits() {
        use core::panic::{RefUnwindSafe, UnwindSafe};
        fn all<T: Send + Sync + UnwindSafe + RefUnwindSafe>() {}
        all::<Arena>();
        all::<SevenZDecoder>();
        all::<crate::RarPpmd>();
    }

    #[test]
    fn caller_memory_drops_with_a_decoder_that_failed_midway() {
        let (params, data, stream) = restarting();
        let alloc = TestAllocator::new(Mode::Garbage);
        let arena = Arena::with_allocator(0, alloc.clone());
        let mut dec = SevenZDecoder::with_arena(params, Some(data.len() as u64), arena).unwrap();
        let cut = &stream[..stream.len() / 2];
        let mut out = vec![0u8; data.len()];
        let e = dec.decode(cut, true, &mut out).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Truncated);
        assert!(e.at.output > 0);
        assert_eq!(&out[..e.at.output as usize], &data[..e.at.output as usize]);
        assert_eq!(alloc.drops(), 0);
        drop(dec);
        assert_eq!((alloc.allocs(), alloc.drops()), (1, 1));

        // And with an `Arena` that never reached a decoder.
        let arena = Arena::with_allocator(0, alloc.clone());
        let dec = SevenZDecoder::with_arena(params, None, arena).unwrap();
        let arena = dec.into_arena();
        assert_eq!(alloc.drops(), 1);
        drop(arena);
        assert_eq!((alloc.allocs(), alloc.drops()), (2, 2));
    }
}

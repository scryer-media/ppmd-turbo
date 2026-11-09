//! Model memory: owned, allocated fallibly, and reusable across streams, or
//! supplied by the caller.

use alloc_crate::alloc::{Layout, alloc, alloc_zeroed};
use alloc_crate::boxed::Box;
use alloc_crate::sync::Arc;
use alloc_crate::vec::Vec;

use crate::error::{Error, ErrorKind, Result};

/// Memory the caller owns, lent to one model for its arena.
///
/// An [`ArenaAllocator`] returns one when the crate asks it for an arena
/// (for example a memory-mapped scratch file, for a model too large to keep
/// in RAM). The model drops it when it is done with it: when the codec is
/// dropped, when a restart needs an arena of another size, or when the
/// [`Arena`] holding it is dropped.
///
/// # Safety
///
/// An implementation promises, from the moment the allocator returns it
/// until it is dropped:
///
/// - [`as_mut_ptr`](Self::as_mut_ptr) returns the same non-null pointer
///   every time, to exactly [`len`](Self::len) bytes that are readable,
///   writable and initialized (any values: the memory need not be zeroed);
/// - nothing else reads or writes those bytes while the crate holds the
///   value, and moving the value (the crate moves the `Box`) does not move
///   them;
/// - `len` never changes.
///
/// No result depends on a byte the model has not written, so the starting
/// contents do not matter: garbage decodes exactly as zeros do. (The
/// sub-allocator's glue pass reads the two bytes after the guard stamp it
/// writes at the free-space boundary and discards them, as 7-Zip's does, so
/// the bytes must be initialized, though not to any value.)
pub unsafe trait ArenaMemory: Send {
    /// The start of the memory.
    fn as_mut_ptr(&mut self) -> *mut u8;
    /// Bytes the memory holds.
    fn len(&self) -> usize;
    /// Whether the memory holds no bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The error an [`ArenaAllocator`] returns. The crate reports it as
/// [`ErrorKind::AllocationFailed`]; the allocator keeps or logs the detail.
/// An `std::io::Error` converts into it with `?` or `.into()`.
pub type ArenaAllocError = Box<dyn core::error::Error + Send + Sync>;

/// Supplies the memory for arenas larger than an in-RAM limit; see
/// [`Arena::with_allocator`].
pub trait ArenaAllocator: Send + Sync {
    /// Memory for an arena of exactly `len` bytes. Memory of any other
    /// length is [`ErrorKind::ArenaMemoryLength`]; an error is
    /// [`ErrorKind::AllocationFailed`].
    fn allocate(&self, len: usize) -> core::result::Result<Box<dyn ArenaMemory>, ArenaAllocError>;
}

/// Where arenas above an in-RAM limit come from.
#[derive(Clone)]
pub(crate) struct ArenaSource {
    ram_limit: u64,
    allocator: Arc<dyn ArenaAllocator>,
}

/// The memory behind an arena.
pub(crate) enum Storage {
    /// The crate's own zeroed allocation, as a `Vec` whose length is the
    /// arena in use.
    Owned(Vec<u8>),
    /// The caller's memory, all of it available to the arena.
    Caller(Box<dyn ArenaMemory>),
}

impl Storage {
    /// Bytes the storage can hold.
    pub(crate) fn capacity(&self) -> usize {
        match self {
            Self::Owned(buf) => buf.capacity(),
            Self::Caller(mem) => mem.len(),
        }
    }
}

/// `len` bytes of fresh storage: the caller's memory when `source` is set
/// and `len` is above its in-RAM limit, otherwise a zeroed allocation.
pub(crate) fn fresh_storage(len: usize, source: Option<&ArenaSource>) -> Result<Storage> {
    match source {
        Some(src) if len as u64 > src.ram_limit => {
            let mem = src
                .allocator
                .allocate(len)
                .map_err(|_| alloc_failed(len as u64))?;
            check_memory(mem, len).map(Storage::Caller)
        }
        _ => try_zeroed(len as u64).map(Storage::Owned),
    }
}

/// `mem` when it holds exactly `len` bytes at a non-null address.
fn check_memory(mut mem: Box<dyn ArenaMemory>, len: usize) -> Result<Box<dyn ArenaMemory>> {
    let provided = mem.len();
    if provided != len || mem.as_mut_ptr().is_null() {
        return Err(Error::new(ErrorKind::ArenaMemoryLength {
            requested: len as u64,
            provided: provided as u64,
        }));
    }
    Ok(mem)
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
    /// and from `allocator` when it is larger. The limit counts the arena
    /// alone: the memory size plus up to three bytes of alignment.
    ///
    /// A codec built over it keeps the rule for every later restart, and
    /// its `into_arena` hands the rule on with the memory. Caller memory is
    /// never zeroed, advised or read before the model writes it.
    pub fn with_allocator(ram_limit: u64, allocator: Arc<dyn ArenaAllocator>) -> Self {
        Self {
            storage: Storage::Owned(Vec::new()),
            source: Some(ArenaSource {
                ram_limit,
                allocator,
            }),
        }
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

    /// Caller memory on the heap, counting its drops.
    pub(crate) struct HeapMemory {
        ptr: *mut u8,
        len: usize,
        drops: Arc<AtomicUsize>,
    }

    impl HeapMemory {
        pub(crate) fn new(bytes: Vec<u8>, drops: Arc<AtomicUsize>) -> Self {
            let len = bytes.len();
            let ptr = Box::into_raw(bytes.into_boxed_slice()).cast::<u8>();
            Self { ptr, len, drops }
        }
    }

    // SAFETY: the memory is a heap allocation this value owns alone.
    unsafe impl Send for HeapMemory {}

    // SAFETY: `ptr` is a live, initialized `len`-byte allocation owned by
    // this value until it drops; the pointer never changes, and moving the
    // value does not move the allocation.
    unsafe impl ArenaMemory for HeapMemory {
        fn as_mut_ptr(&mut self) -> *mut u8 {
            self.ptr
        }
        fn len(&self) -> usize {
            self.len
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
        /// Memory one byte longer than asked.
        WrongLength,
    }

    /// An allocator that counts its allocations and their drops.
    pub(crate) struct TestAllocator {
        mode: Mode,
        pub(crate) allocs: AtomicUsize,
        pub(crate) drops: Arc<AtomicUsize>,
    }

    impl TestAllocator {
        pub(crate) fn new(mode: Mode) -> Arc<Self> {
            Arc::new(Self {
                mode,
                allocs: AtomicUsize::new(0),
                drops: Arc::new(AtomicUsize::new(0)),
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
            let len = match self.mode {
                Mode::Fail => return Err(Box::new(Refused)),
                Mode::Garbage => len,
                Mode::WrongLength => len + 1,
            };
            let bytes = garbage(len, 0x9E37_79B9 ^ n.wrapping_mul(0x85EB_CA6B) | 1);
            Ok(Box::new(HeapMemory::new(bytes, self.drops.clone())))
        }
    }
}

#[cfg(test)]
mod caller_memory_tests {
    use super::testing::{Mode, TestAllocator};
    use super::*;
    use crate::{Params, SevenZDecoder, SevenZEncoder, SevenZStatus};

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
        assert_eq!(
            e.kind,
            ErrorKind::AllocationFailed {
                bytes: Params::MIN_MEM as u64
            }
        );
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
    fn memory_of_the_wrong_length_is_an_error_and_is_dropped() {
        let params = Params::new(6, Params::MIN_MEM + 1).unwrap();
        let alloc = TestAllocator::new(Mode::WrongLength);
        let arena = Arena::with_allocator(0, alloc.clone());
        let e = SevenZDecoder::with_arena(params, None, arena)
            .err()
            .unwrap();
        // 2049 bytes lay out with three bytes of alignment.
        assert_eq!(
            e.kind,
            ErrorKind::ArenaMemoryLength {
                requested: 2052,
                provided: 2053
            }
        );
        assert!(!e.is_resource_error() && !e.is_data_error());
        assert_eq!((alloc.allocs(), alloc.drops()), (1, 1));
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

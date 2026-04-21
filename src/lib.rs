//! Transparent-huge-page large-object allocator with cooperative residency control.
//!
//! hugalloc allocates in power-of-two size classes from 2 MiB to 128 GiB, backed
//! by anonymous `mmap` + `MADV_HUGEPAGE` on Linux. The allocator pool is managed
//! by a per-size-class work-stealing queue with per-thread buffering.
//!
//! # Overview
//!
//! The two most useful entry points for users:
//!
//! - [`Buffer`] — a Vec-like, length-tracking buffer with lgalloc-or-heap backing.
//! - [`RawBuffer`] — the underlying fixed-capacity uninitialized primitive.
//!
//! Low-level users can reach for [`allocate`] directly, which returns a [`Handle`]
//! whose `Drop` returns the allocation to the pool.
//!
//! # Residency advisories
//!
//! Handles and buffers expose three `madvise`-based hints:
//!
//! - `prefetch` (portable) — `MADV_WILLNEED`: page in eagerly.
//! - `cold` (Linux 5.4+) — `MADV_COLD`: mark range as preferred eviction.
//! - `pageout` (Linux 5.4+) — `MADV_PAGEOUT`: force eviction (requires swap).
//!
//! All advisories are hints — they never affect correctness.
//!
//! # Configuration
//!
//! Use [`builder`] to build a configuration and apply it:
//!
//! ```ignore
//! hugalloc::builder()
//!     .enable()
//!     .eager_return(true)
//!     .apply()?;
//! ```
//!
//! See [`Builder`] for the full list of knobs.
//!
//! # Forked from `lgalloc` 0.7.0.

#![deny(missing_docs)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem::{take, ManuallyDrop, MaybeUninit};
use std::ops::Range;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread::{JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use thiserror::Error;

mod readme {
    #![doc = include_str!("../README.md")]
}

/// Handle to an allocation obtained from [`allocate`].
///
/// Dropping a `Handle` returns its memory to the pool. The pool is
/// lock-free and the drop may occur on any thread. To release eagerly,
/// call `drop(handle)`. To intentionally leak the allocation, use
/// [`std::mem::forget`].
pub struct Handle {
    /// The actual pointer.
    ptr: NonNull<u8>,
    /// Length of the allocation.
    len: usize,
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

#[allow(clippy::len_without_is_empty)]
impl Handle {
    /// Construct a new handle from a region of memory
    fn new(ptr: NonNull<u8>, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Construct a dangling handle, which is only suitable for zero-sized types.
    fn dangling() -> Self {
        Self {
            ptr: NonNull::dangling(),
            len: 0,
        }
    }

    fn is_dangling(&self) -> bool {
        self.ptr == NonNull::dangling()
    }

    /// Length of the memory area in bytes.
    fn len(&self) -> usize {
        self.len
    }

    /// Pointer to memory.
    fn as_non_null(&self) -> NonNull<u8> {
        self.ptr
    }

    /// Indicate that the memory is not in use and that the OS can lazily recycle it.
    ///
    /// Uses `MADV_FREE` on Linux (lazy reclaim, avoids immediate page zeroing) and
    /// `MADV_DONTNEED` elsewhere.
    fn clear(&mut self) -> std::io::Result<()> {
        // SAFETY: `MADV_CLEAR_STRATEGY` guaranteed to be a valid argument.
        unsafe { self.madvise(MADV_CLEAR_STRATEGY) }
    }

    /// Indicate that the memory is not in use and that the OS should immediately recycle it.
    fn fast_clear(&mut self) -> std::io::Result<()> {
        // SAFETY: `libc::MADV_DONTNEED` documented to be a valid argument.
        unsafe { self.madvise(libc::MADV_DONTNEED) }
    }

    /// Issue a `madvise` hint over a byte range of this allocation.
    ///
    /// Performs the bounds check and the zero-length / dangling short-circuit.
    /// The kernel return code is discarded — advisories are hints that never
    /// affect correctness.
    fn advise_range(
        &self,
        byte_range: Range<usize>,
        advice: libc::c_int,
    ) -> Result<(), AdviseError> {
        let byte_offset = byte_range.start;
        let byte_len = byte_range.end.saturating_sub(byte_range.start);
        if byte_len == 0 || self.is_dangling() {
            return Ok(());
        }
        if byte_offset.saturating_add(byte_len) > self.len {
            return Err(AdviseError::OutOfBounds {
                byte_offset,
                byte_len,
                allocation_len: self.len,
            });
        }
        // SAFETY: advisory hint, ptr in-bounds by check above.
        unsafe {
            let ptr = self.as_non_null().as_ptr().add(byte_offset);
            // MADV_WILLNEED is portable; MADV_COLD / MADV_PAGEOUT are Linux-only.
            // On non-Linux, MADV_COLD_STRATEGY / MADV_PAGEOUT_STRATEGY are -1 and
            // must not be passed to the kernel.
            if advice != -1 {
                libc::madvise(ptr.cast(), byte_len, advice);
            }
        }
        Ok(())
    }

    /// Hint that a byte range will be needed soon.
    ///
    /// Issues `MADV_WILLNEED` over `byte_range`. The kernel begins paging the
    /// range in; a subsequent access will find it resident or wait for a
    /// shorter I/O.
    ///
    /// The byte range does not need to be page-aligned — the kernel rounds
    /// to page boundaries internally.
    ///
    /// This is a performance hint and never affects correctness.
    ///
    /// # Errors
    ///
    /// Returns [`AdviseError::OutOfBounds`] if `byte_range.end` exceeds the
    /// allocation length.
    pub fn prefetch(&self, byte_range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_range(byte_range, libc::MADV_WILLNEED)
    }

    /// Hint that a byte range is unlikely to be accessed soon.
    ///
    /// Issues `MADV_COLD` on Linux (silent no-op on other platforms). Pages
    /// remain mapped and populated but become preferred eviction candidates
    /// under memory pressure. Cheap.
    ///
    /// On a transparent huge page, the kernel may split the page down to 4
    /// KiB granularity to operate on a sub-range. Splits are not
    /// automatically reversed.
    ///
    /// This is a performance hint and never affects correctness.
    ///
    /// # Errors
    ///
    /// Returns [`AdviseError::OutOfBounds`] if `byte_range.end` exceeds the
    /// allocation length.
    pub fn cold(&self, byte_range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_range(byte_range, MADV_COLD_STRATEGY)
    }

    /// Request immediate eviction of a byte range.
    ///
    /// Issues `MADV_PAGEOUT` on Linux (silent no-op on other platforms). On
    /// anonymous pages this requires swap to be configured; without swap
    /// the kernel cannot evict anonymous memory and the call has no useful
    /// effect.
    ///
    /// Expensive — issues syscalls synchronously and may cause the kernel to
    /// split transparent huge pages.
    ///
    /// This is a performance hint and never affects correctness.
    ///
    /// # Errors
    ///
    /// Returns [`AdviseError::OutOfBounds`] if `byte_range.end` exceeds the
    /// allocation length.
    pub fn pageout(&self, byte_range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_range(byte_range, MADV_PAGEOUT_STRATEGY)
    }

    /// Consume the handle and return its raw components. The caller becomes
    /// responsible for the allocation and must reconstruct via
    /// [`Handle::from_raw_parts`] to release it, or leak the memory
    /// permanently via [`std::mem::forget`].
    pub fn into_raw_parts(self) -> (NonNull<u8>, usize) {
        let parts = (self.ptr, self.len);
        std::mem::forget(self);
        parts
    }

    /// Reconstruct a `Handle` from a pointer and length previously returned
    /// by [`Handle::into_raw_parts`].
    ///
    /// # Safety
    ///
    /// - `ptr` and `len` must have come from a prior call to
    ///   `Handle::into_raw_parts` on the same process.
    /// - The allocation must not have been freed or reconstructed since.
    /// - Calling `from_raw_parts` twice on the same pair of values without
    ///   an intervening deallocate produces aliasing and is undefined behavior.
    pub unsafe fn from_raw_parts(ptr: NonNull<u8>, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Call `madvise` on the memory region. Unsafe because `advice` is passed verbatim.
    unsafe fn madvise(&self, advice: libc::c_int) -> std::io::Result<()> {
        // SAFETY: Calling into `madvise`:
        // * The ptr is page-aligned by construction.
        // * The ptr + length is page-aligned by construction (not required but surprising otherwise)
        // * Pages not locked.
        // * The caller is responsible for passing a valid `advice` parameter.
        let ptr = self.as_non_null().as_ptr().cast();
        let ret = unsafe { libc::madvise(ptr, self.len, advice) };
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            return Err(err);
        }
        Ok(())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.is_dangling() {
            return;
        }
        // Steal the fields into a fresh owned Handle and forward to the
        // thread-local deallocation path. The original `self` is left in a
        // dangling state so any (impossible) recursive drop is a no-op.
        let taken = Handle {
            ptr: std::mem::replace(&mut self.ptr, NonNull::dangling()),
            len: std::mem::replace(&mut self.len, 0),
        };
        thread_context(|s| s.deallocate(taken));
    }
}

/// Initial area size
const INITIAL_SIZE: usize = 32 << 20;

/// Range of valid size classes.
pub const VALID_SIZE_CLASS: Range<usize> = 21..37;

/// Strategy for background worker clear: `MADV_FREE` on Linux (lazy reclaim), `MADV_DONTNEED` elsewhere.
#[cfg(target_os = "linux")]
const MADV_CLEAR_STRATEGY: libc::c_int = libc::MADV_FREE;

#[cfg(not(target_os = "linux"))]
const MADV_CLEAR_STRATEGY: libc::c_int = libc::MADV_DONTNEED;

/// Linux-only "mark as cold" advice; `-1` sentinel on other platforms (never passed to kernel).
#[cfg(target_os = "linux")]
const MADV_COLD_STRATEGY: libc::c_int = libc::MADV_COLD;
#[cfg(not(target_os = "linux"))]
const MADV_COLD_STRATEGY: libc::c_int = -1;

/// Linux-only "page out" advice; `-1` sentinel on other platforms (never passed to kernel).
#[cfg(target_os = "linux")]
const MADV_PAGEOUT_STRATEGY: libc::c_int = libc::MADV_PAGEOUT;
#[cfg(not(target_os = "linux"))]
const MADV_PAGEOUT_STRATEGY: libc::c_int = -1;

/// Whether we have already warned about `MADV_HUGEPAGE` failure.
#[cfg(target_os = "linux")]
static MADV_HUGEPAGE_WARNED: AtomicBool = AtomicBool::new(false);

type PhantomUnsyncUnsend<T> = PhantomData<*mut T>;

/// Allocation errors
#[derive(Error, Debug)]
pub enum AllocError {
    /// IO error, unrecoverable
    #[error("I/O error")]
    Io(#[from] std::io::Error),
    /// Out of memory, meaning that the pool is exhausted.
    #[error("Out of memory")]
    OutOfMemory,
    /// Size class too large or small
    #[error("Invalid size class")]
    InvalidSizeClass(usize),
    /// Allocator disabled
    #[error("Disabled by configuration")]
    Disabled,
    /// Failed to allocate memory that suits alignment properties.
    #[error("Memory unsuitable for requested alignment")]
    UnalignedMemory,
}

/// Errors from [`Handle::prefetch`], [`Handle::cold`], and [`Handle::pageout`].
#[derive(Error, Debug)]
pub enum AdviseError {
    /// The requested byte range exceeds the allocation.
    #[error("advise byte range [{byte_offset}..{end}) exceeds allocation length {allocation_len}", end = byte_offset + byte_len)]
    OutOfBounds {
        /// Byte offset of the requested range.
        byte_offset: usize,
        /// Byte length of the requested range.
        byte_len: usize,
        /// Total byte length of the allocation.
        allocation_len: usize,
    },
}

/// Errors from [`Builder::apply`].
#[derive(Error, Debug)]
pub enum ConfigError {
    /// The background worker thread failed to spawn.
    #[error("failed to spawn background worker thread: {0}")]
    BackgroundWorkerFailed(#[from] std::io::Error),
}

impl AllocError {
    /// Check if this error is [`AllocError::Disabled`].
    #[must_use]
    pub fn is_disabled(&self) -> bool {
        matches!(self, AllocError::Disabled)
    }
}

/// Abstraction over size classes.
#[derive(Clone, Copy)]
struct SizeClass(usize);

impl SizeClass {
    const fn new_unchecked(value: usize) -> Self {
        Self(value)
    }

    const fn index(self) -> usize {
        self.0 - VALID_SIZE_CLASS.start
    }

    /// The size in bytes of this size class.
    const fn byte_size(self) -> usize {
        1 << self.0
    }

    const fn from_index(index: usize) -> Self {
        Self(index + VALID_SIZE_CLASS.start)
    }

    /// Obtain a size class from a size in bytes.
    fn from_byte_size(byte_size: usize) -> Result<Self, AllocError> {
        let class = byte_size.next_power_of_two().trailing_zeros() as usize;
        class.try_into()
    }

    const fn from_byte_size_unchecked(byte_size: usize) -> Self {
        Self::new_unchecked(byte_size.next_power_of_two().trailing_zeros() as usize)
    }
}

impl TryFrom<usize> for SizeClass {
    type Error = AllocError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        if VALID_SIZE_CLASS.contains(&value) {
            Ok(SizeClass(value))
        } else {
            Err(AllocError::InvalidSizeClass(value))
        }
    }
}

#[derive(Default, Debug)]
struct AllocStats {
    allocations: AtomicU64,
    slow_path: AtomicU64,
    refill: AtomicU64,
    deallocations: AtomicU64,
    clear_eager: AtomicU64,
    clear_slow: AtomicU64,
}

/// Handle to the shared global state.
static INJECTOR: OnceLock<GlobalStealer> = OnceLock::new();

/// Enabled switch to turn on or off hugalloc. Off by default.
static LGALLOC_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable eager returning of memory. Off by default.
static LGALLOC_EAGER_RETURN: AtomicBool = AtomicBool::new(false);

/// Dampener in the area growth rate. 0 corresponds to doubling and in general `n` to `1+1/(n+1)`.
///
/// Setting this to 0 results in creating areas with doubling capacity.
/// Larger numbers result in more conservative approaches that create more areas.
static LGALLOC_GROWTH_DAMPENER: AtomicUsize = AtomicUsize::new(0);

/// The size of allocations to retain locally, per thread and size class.
static LOCAL_BUFFER_BYTES: AtomicUsize = AtomicUsize::new(32 << 20);

/// Type maintaining the global state for each size class.
struct GlobalStealer {
    /// State for each size class. An entry at position `x` handles size class `x`, which is areas
    /// of size `1<<x`.
    size_classes: Vec<SizeClassState>,
    /// Shared token to access background thread.
    background_sender: Mutex<Option<(JoinHandle<()>, Sender<BackgroundConfigUpdate>)>>,
}

/// Per-size-class state
#[derive(Default)]
struct SizeClassState {
    /// Handle to anonymous memory-mapped regions.
    ///
    /// We must never dereference the memory-mapped regions stored here.
    areas: RwLock<Vec<ManuallyDrop<(usize, usize)>>>,
    /// Injector to distribute memory globally.
    injector: Injector<Handle>,
    /// Injector to distribute memory globally, freed memory.
    clean_injector: Injector<Handle>,
    /// Slow-path lock to refill pool.
    lock: Mutex<()>,
    /// Thread stealers to allow all participating threads to steal memory.
    stealers: RwLock<HashMap<ThreadId, PerThreadState<Handle>>>,
    /// Summed stats for terminated threads.
    alloc_stats: AllocStats,
    /// Total virtual size of all mappings in this size class in bytes.
    total_bytes: AtomicUsize,
    /// Count of areas backing this size class.
    area_count: AtomicUsize,
}

impl GlobalStealer {
    /// Obtain the shared global state.
    fn get_static() -> &'static Self {
        INJECTOR.get_or_init(Self::new)
    }

    /// Obtain the per-size-class global state.
    fn get_size_class(&self, size_class: SizeClass) -> &SizeClassState {
        &self.size_classes[size_class.index()]
    }

    fn new() -> Self {
        let mut size_classes = Vec::with_capacity(VALID_SIZE_CLASS.len());

        for _ in VALID_SIZE_CLASS {
            size_classes.push(SizeClassState::default());
        }

        Self {
            size_classes,
            background_sender: Mutex::default(),
        }
    }
}

impl Drop for GlobalStealer {
    fn drop(&mut self) {
        // Unmap all areas to return virtual address space.
        for size_class_state in &mut self.size_classes {
            let mut areas = size_class_state.areas.write().expect("lock poisoned");
            for area in areas.drain(..) {
                let (addr, len) = ManuallyDrop::into_inner(area);
                // SAFETY: `addr` and `len` were returned by `mmap` during `try_refill_and_get`.
                unsafe {
                    libc::munmap(addr as *mut libc::c_void, len);
                }
            }
        }
        take(&mut self.size_classes);
    }
}

struct PerThreadState<T> {
    stealer: Stealer<T>,
    alloc_stats: Arc<AllocStats>,
}

/// Per-thread and state, sharded by size class.
struct ThreadLocalStealer {
    /// Per-size-class state
    size_classes: Vec<LocalSizeClass>,
    _phantom: PhantomUnsyncUnsend<Self>,
}

impl ThreadLocalStealer {
    fn new() -> Self {
        let thread_id = std::thread::current().id();
        let size_classes = VALID_SIZE_CLASS
            .map(|size_class| LocalSizeClass::new(SizeClass::new_unchecked(size_class), thread_id))
            .collect();
        Self {
            size_classes,
            _phantom: PhantomData,
        }
    }

    /// Allocate a memory region from a specific size class.
    ///
    /// Returns [`AllocError::Disabled`] if hugalloc is not enabled. Returns other error types
    /// if out of memory, or an internal operation fails.
    fn allocate(&self, size_class: SizeClass) -> Result<Handle, AllocError> {
        if !LGALLOC_ENABLED.load(Ordering::Relaxed) {
            return Err(AllocError::Disabled);
        }
        self.size_classes[size_class.index()].get_with_refill()
    }

    /// Return memory to the allocator. Must have been obtained through [`allocate`].
    fn deallocate(&self, mem: Handle) {
        let size_class = SizeClass::from_byte_size_unchecked(mem.len());

        self.size_classes[size_class.index()].push(mem);
    }
}

thread_local! {
    static WORKER: RefCell<ThreadLocalStealer> = RefCell::new(ThreadLocalStealer::new());
}

/// Per-thread, per-size-class state
///
/// # Safety
///
/// We store parts of areas in this struct. Leaking this struct leaks the areas, which is safe
/// because we will never try to access or reclaim them.
struct LocalSizeClass {
    /// Local memory queue.
    worker: Worker<Handle>,
    /// Size class we're covering
    size_class: SizeClass,
    /// Handle to global size class state
    size_class_state: &'static SizeClassState,
    /// Owning thread's ID
    thread_id: ThreadId,
    /// Shared statistics maintained by this thread.
    stats: Arc<AllocStats>,
    /// Phantom data to prevent sending the type across thread boundaries.
    _phantom: PhantomUnsyncUnsend<Self>,
}

impl LocalSizeClass {
    /// Construct a new local size class state. Registers the worker with the global state.
    fn new(size_class: SizeClass, thread_id: ThreadId) -> Self {
        let worker = Worker::new_lifo();
        let stealer = GlobalStealer::get_static();
        let size_class_state = stealer.get_size_class(size_class);

        let stats = Arc::new(AllocStats::default());

        let mut lock = size_class_state.stealers.write().expect("lock poisoned");
        lock.insert(
            thread_id,
            PerThreadState {
                stealer: worker.stealer(),
                alloc_stats: Arc::clone(&stats),
            },
        );

        Self {
            worker,
            size_class,
            size_class_state,
            thread_id,
            stats,
            _phantom: PhantomData,
        }
    }

    /// Get a memory area. Tries to get a region from the local cache, before obtaining data from
    /// the global state. As a last option, obtains memory from other workers.
    ///
    /// Returns [`AllocError::OutOfMemory`] if all pools are empty.
    #[inline]
    fn get(&self) -> Result<Handle, AllocError> {
        self.worker
            .pop()
            .or_else(|| {
                std::iter::repeat_with(|| {
                    // The loop tries to obtain memory in the following order:
                    // 1. Memory from the global state,
                    // 2. Memory from the global cleaned state,
                    // 3. Memory from other threads.
                    let limit = 1.max(
                        LOCAL_BUFFER_BYTES.load(Ordering::Relaxed)
                            / self.size_class.byte_size()
                            / 2,
                    );

                    self.size_class_state
                        .injector
                        .steal_batch_with_limit_and_pop(&self.worker, limit)
                        .or_else(|| {
                            self.size_class_state
                                .clean_injector
                                .steal_batch_with_limit_and_pop(&self.worker, limit)
                        })
                        .or_else(|| {
                            self.size_class_state
                                .stealers
                                .read()
                                .expect("lock poisoned")
                                .values()
                                .map(|state| state.stealer.steal())
                                .collect()
                        })
                })
                .find(|s| !s.is_retry())
                .and_then(Steal::success)
            })
            .ok_or(AllocError::OutOfMemory)
    }

    /// Like [`Self::get()`] but trying to refill the pool if it is empty.
    fn get_with_refill(&self) -> Result<Handle, AllocError> {
        self.stats.allocations.fetch_add(1, Ordering::Relaxed);
        // Fast-path: Get non-blocking
        match self.get() {
            Err(AllocError::OutOfMemory) => {
                self.stats.slow_path.fetch_add(1, Ordering::Relaxed);
                // Get a slow-path lock
                let _lock = self.size_class_state.lock.lock().expect("lock poisoned");
                // Try again because another thread might have refilled already
                if let Ok(mem) = self.get() {
                    return Ok(mem);
                }
                self.try_refill_and_get()
            }
            r => r,
        }
    }

    /// Recycle memory. Stores it locally or forwards it to the global state.
    fn push(&self, mut mem: Handle) {
        debug_assert_eq!(mem.len(), self.size_class.byte_size());
        self.stats.deallocations.fetch_add(1, Ordering::Relaxed);
        if self.worker.len()
            >= LOCAL_BUFFER_BYTES.load(Ordering::Relaxed) / self.size_class.byte_size()
        {
            if LGALLOC_EAGER_RETURN.load(Ordering::Relaxed) {
                self.stats.clear_eager.fetch_add(1, Ordering::Relaxed);
                mem.fast_clear().expect("clearing successful");
            }
            self.size_class_state.injector.push(mem);
        } else {
            self.worker.push(mem);
        }
    }

    /// Refill the memory pool, and get one area.
    ///
    /// Returns an error if the memory pool cannot be refilled.
    fn try_refill_and_get(&self) -> Result<Handle, AllocError> {
        self.stats.refill.fetch_add(1, Ordering::Relaxed);
        let mut stash = self.size_class_state.areas.write().expect("lock poisoned");

        let initial_capacity = std::cmp::max(1, INITIAL_SIZE / self.size_class.byte_size());

        let last_capacity =
            stash.iter().last().map_or(0, |mmap| mmap.1) / self.size_class.byte_size();
        let growth_dampener = LGALLOC_GROWTH_DAMPENER.load(Ordering::Relaxed);
        // We would like to grow the area capacity by a factor of `1+1/(growth_dampener+1)`,
        // but at least by `initial_capacity`.
        let next_capacity = last_capacity
            + std::cmp::max(
                initial_capacity,
                last_capacity / (growth_dampener.saturating_add(1)),
            );

        let next_byte_len = next_capacity * self.size_class.byte_size();

        let (mmap_ptr, slice) = mmap_anonymous(next_byte_len)?;

        self.size_class_state
            .total_bytes
            .fetch_add(next_byte_len, Ordering::Relaxed);
        self.size_class_state
            .area_count
            .fetch_add(1, Ordering::Relaxed);

        // SAFETY: Memory region initialized, so pointers to it are valid.
        let mut chunks = slice
            .chunks_exact_mut(self.size_class.byte_size())
            .map(|chunk| NonNull::new(chunk.as_mut_ptr()).expect("non-null"));

        // Capture first region to return immediately.
        let ptr = chunks.next().expect("At least one chunk allocated.");
        let mem = Handle::new(ptr, self.size_class.byte_size());

        // Stash remaining in the injector.
        for ptr in chunks {
            self.size_class_state
                .clean_injector
                .push(Handle::new(ptr, self.size_class.byte_size()));
        }

        stash.push(ManuallyDrop::new((mmap_ptr, next_byte_len)));
        Ok(mem)
    }
}

/// Create an anonymous memory mapping with huge page hints.
///
/// Returns a tuple of `(address, mutable slice)` on success.
fn mmap_anonymous(len: usize) -> Result<(usize, &'static mut [u8]), AllocError> {
    // SAFETY: Creating an anonymous private mapping with no file descriptor.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error().into());
    }

    // Hint to the kernel to use transparent huge pages. This is a performance hint,
    // not a correctness requirement — THP may be disabled system-wide.
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `ptr` is a valid mapping returned by `mmap` above.
        let ret = unsafe { libc::madvise(ptr, len, libc::MADV_HUGEPAGE) };
        if ret == -1 && !MADV_HUGEPAGE_WARNED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "hugalloc: MADV_HUGEPAGE failed: {}. Transparent huge pages may be disabled.",
                std::io::Error::last_os_error()
            );
        }
    }

    // SAFETY: `ptr` is a valid mapping of `len` bytes.
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), len) };
    Ok((ptr as usize, slice))
}

impl Drop for LocalSizeClass {
    fn drop(&mut self) {
        // Remove state associated with thread
        if let Ok(mut lock) = self.size_class_state.stealers.write() {
            lock.remove(&self.thread_id);
        }

        // Send memory back to global state
        while let Some(mem) = self.worker.pop() {
            self.size_class_state.injector.push(mem);
        }

        let ordering = Ordering::Relaxed;

        // Update global metrics by moving all worker-local metrics to global state.
        self.size_class_state
            .alloc_stats
            .allocations
            .fetch_add(self.stats.allocations.load(ordering), ordering);
        let global_stats = &self.size_class_state.alloc_stats;
        global_stats
            .refill
            .fetch_add(self.stats.refill.load(ordering), ordering);
        global_stats
            .slow_path
            .fetch_add(self.stats.slow_path.load(ordering), ordering);
        global_stats
            .deallocations
            .fetch_add(self.stats.deallocations.load(ordering), ordering);
        global_stats
            .clear_slow
            .fetch_add(self.stats.clear_slow.load(ordering), ordering);
        global_stats
            .clear_eager
            .fetch_add(self.stats.clear_eager.load(ordering), ordering);
    }
}

/// Access the per-thread context.
fn thread_context<R, F: FnOnce(&ThreadLocalStealer) -> R>(f: F) -> R {
    WORKER.with(|cell| f(&cell.borrow()))
}

/// Allocate a memory area suitable to hold `capacity` consecutive elements of `T`.
///
/// Returns a pointer, a capacity in `T`, and a handle if successful, and an error
/// otherwise. The capacity can be larger than requested.
///
/// The returned [`Handle`] must be dropped or explicitly freed; dropping it returns the memory to
/// the pool. The memory can be freed on a different thread.
///
/// # Errors
///
/// Allocate errors if the capacity cannot be supported by one of the size classes,
/// the alignment requirements of `T` cannot be fulfilled, if no more memory can be
/// obtained from the system, or if any syscall fails.
///
/// The function also returns an error if hugalloc is disabled.
///
/// In the case of an error, no memory is allocated, and we maintain the internal
/// invariants of the allocator.
///
/// # Panics
///
/// The function can panic on internal errors, specifically when an allocation returned
/// an unexpected size. In this case, we do not maintain the allocator invariants
/// and the caller should abort the process.
///
/// Panics if the thread local variable has been dropped, see [`std::thread::LocalKey`]
/// for details.
pub fn allocate<T>(capacity: usize) -> Result<(NonNull<T>, usize, Handle), AllocError> {
    if std::mem::size_of::<T>() == 0 {
        return Ok((NonNull::dangling(), usize::MAX, Handle::dangling()));
    } else if capacity == 0 {
        return Ok((NonNull::dangling(), 0, Handle::dangling()));
    }

    // Round up to at least a page.
    let byte_len = std::cmp::max(page_size::get(), std::mem::size_of::<T>() * capacity);
    // With above rounding up to page sizes, we only allocate multiples of page size because
    // we only support powers-of-two sized regions.
    let size_class = SizeClass::from_byte_size(byte_len)?;

    let handle = thread_context(|s| s.allocate(size_class))?;
    debug_assert_eq!(handle.len(), size_class.byte_size());
    let ptr: NonNull<T> = handle.as_non_null().cast();
    // Memory region should be page-aligned, which we assume to be larger than any alignment
    // we might encounter. If this is not the case, bail out.
    if ptr.as_ptr().align_offset(std::mem::align_of::<T>()) != 0 {
        thread_context(move |s| s.deallocate(handle));
        return Err(AllocError::UnalignedMemory);
    }
    let actual_capacity = handle.len() / std::mem::size_of::<T>();
    Ok((ptr, actual_capacity, handle))
}

/// Full configuration the background worker holds. Not public.
#[derive(Debug, Clone)]
struct BackgroundWorkerConfig {
    /// How frequently the worker ticks. `Duration::MAX` = effectively disabled.
    interval: Duration,
    /// Minimum bytes to clear per size class per tick.
    clear_bytes: usize,
    /// Decay factor for exponential backlog drain, in `[0.0, 1.0]`.
    decay: f32,
}

impl Default for BackgroundWorkerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::MAX,
            clear_bytes: 0,
            decay: 0.5,
        }
    }
}

/// Partial configuration sent over the config channel. The worker overlays
/// `Some(_)` fields onto its current config and ignores `None` fields.
/// Lets callers tweak one background knob without clobbering the others.
#[derive(Default, Debug, Clone)]
struct BackgroundConfigUpdate {
    interval: Option<Duration>,
    clear_bytes: Option<usize>,
    decay: Option<f32>,
}

/// A background worker that performs periodic tasks.
struct BackgroundWorker {
    config: BackgroundWorkerConfig,
    receiver: Receiver<BackgroundConfigUpdate>,
    global_stealer: &'static GlobalStealer,
    worker: Worker<Handle>,
}

impl BackgroundWorker {
    fn new(receiver: Receiver<BackgroundConfigUpdate>) -> Self {
        let global_stealer = GlobalStealer::get_static();
        let worker = Worker::new_fifo();
        Self {
            config: BackgroundWorkerConfig::default(),
            receiver,
            global_stealer,
            worker,
        }
    }

    fn run(&mut self) {
        let mut next_cleanup: Option<Instant> = None;
        loop {
            let timeout = next_cleanup.map_or(Duration::MAX, |next_cleanup| {
                next_cleanup.saturating_duration_since(Instant::now())
            });
            match self.receiver.recv_timeout(timeout) {
                Ok(update) => {
                    if let Some(i) = update.interval {
                        self.config.interval = i;
                    }
                    if let Some(c) = update.clear_bytes {
                        self.config.clear_bytes = c;
                    }
                    if let Some(d) = update.decay {
                        self.config.decay = d;
                    }
                    next_cleanup = None;
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    self.maintenance();
                }
            }
            next_cleanup = next_cleanup
                .unwrap_or_else(Instant::now)
                .checked_add(self.config.interval);
        }
    }

    fn maintenance(&self) {
        for (index, size_class_state) in self.global_stealer.size_classes.iter().enumerate() {
            let size_class = SizeClass::from_index(index);
            let count = self.clear(size_class, size_class_state, &self.worker);
            size_class_state
                .alloc_stats
                .clear_slow
                .fetch_add(count.try_into().expect("must fit"), Ordering::Relaxed);
        }
    }

    fn clear(
        &self,
        size_class: SizeClass,
        state: &SizeClassState,
        worker: &Worker<Handle>,
    ) -> usize {
        let byte_size = size_class.byte_size();
        let floor = (self.config.clear_bytes + byte_size - 1) / byte_size;
        let ceiling = floor.saturating_mul(64).max(1);
        let backlog = state.injector.len();
        let want = (backlog as f32 * self.config.decay) as usize;
        let mut limit = want.max(floor).min(ceiling);
        let mut count = 0;
        let mut steal = Steal::Retry;
        while limit > 0 && !steal.is_empty() {
            steal = std::iter::repeat_with(|| state.injector.steal_batch_with_limit(worker, limit))
                .find(|s| !s.is_retry())
                .unwrap_or(Steal::Empty);
            while let Some(mut mem) = worker.pop() {
                match mem.clear() {
                    Ok(()) => count += 1,
                    Err(e) => panic!("Syscall failed: {e:?}"),
                }
                state.clean_injector.push(mem);
                limit -= 1;
            }
        }
        count
    }
}

/// Fluent builder for hugalloc configuration.
///
/// Constructed via [`builder()`]. Each setter consumes and returns `self`
/// so the whole configuration reads as one expression. [`Builder::apply`]
/// commits the configuration — only fields explicitly set via a setter are
/// written to the global state; unset fields are preserved at their prior
/// values.
#[derive(Default, Clone)]
pub struct Builder {
    enabled: Option<bool>,
    eager_return: Option<bool>,
    growth_dampener: Option<usize>,
    local_buffer_bytes: Option<usize>,
    background_interval: Option<Duration>,
    background_clear_bytes: Option<usize>,
    background_decay: Option<f32>,
}

/// Begin configuring hugalloc. Terminate the chain with [`Builder::apply`].
#[must_use]
pub fn builder() -> Builder {
    Builder::default()
}

impl Builder {
    /// Set whether hugalloc is enabled.
    pub fn enabled(mut self, yes: bool) -> Self {
        self.enabled = Some(yes);
        self
    }

    /// Shorthand for `.enabled(true)`.
    pub fn enable(self) -> Self {
        self.enabled(true)
    }

    /// Shorthand for `.enabled(false)`.
    pub fn disable(self) -> Self {
        self.enabled(false)
    }

    /// Whether to return physical memory on deallocate.
    pub fn eager_return(mut self, yes: bool) -> Self {
        self.eager_return = Some(yes);
        self
    }

    /// Dampener in the area growth rate. `0` doubles; `n` grows by `1 + 1/(n+1)`.
    pub fn growth_dampener(mut self, n: usize) -> Self {
        self.growth_dampener = Some(n);
        self
    }

    /// Size of the per-thread per-size class cache, in bytes.
    pub fn local_buffer_bytes(mut self, bytes: usize) -> Self {
        self.local_buffer_bytes = Some(bytes);
        self
    }

    /// Background worker tick interval.
    pub fn background_interval(mut self, d: Duration) -> Self {
        self.background_interval = Some(d);
        self
    }

    /// Minimum bytes of backlog to clear per tick per size class.
    pub fn background_clear_bytes(mut self, bytes: usize) -> Self {
        self.background_clear_bytes = Some(bytes);
        self
    }

    /// Exponential decay factor for backlog drain.
    ///
    /// Higher values drain faster but cause more per-tick work. `0.5` halves
    /// the backlog per tick beyond the floor. `0.0` disables acceleration
    /// and falls back to the floor rate.
    ///
    /// Valid range is `[0.0, 1.0]`. Values outside this range are clamped
    /// to the nearest boundary by [`Builder::apply`]; `NaN` is replaced by the
    /// default (0.5).
    pub fn background_decay(mut self, decay: f32) -> Self {
        self.background_decay = Some(decay);
        self
    }

    /// Commit the configuration to hugalloc's global state.
    ///
    /// Only fields explicitly set on the builder are written. Unset fields
    /// preserve their prior values. This applies uniformly to both
    /// top-level knobs (e.g. `enabled`, `growth_dampener`) and background
    /// worker knobs (`background_interval`, `background_clear_bytes`,
    /// `background_decay`) — the background worker receives a partial
    /// update and overlays set fields onto its running configuration.
    ///
    /// The background worker thread is spawned lazily on the first
    /// `apply()` call that includes any background-related knob. If no
    /// background knob has ever been set, the worker never starts.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::BackgroundWorkerFailed`] if the background
    /// worker thread cannot be spawned.
    pub fn apply(self) -> Result<(), ConfigError> {
        apply_config(self)
    }
}

fn apply_config(b: Builder) -> Result<(), ConfigError> {
    let stealer = GlobalStealer::get_static();

    if let Some(enabled) = b.enabled {
        LGALLOC_ENABLED.store(enabled, Ordering::Relaxed);
    }
    if let Some(eager_return) = b.eager_return {
        LGALLOC_EAGER_RETURN.store(eager_return, Ordering::Relaxed);
    }
    if let Some(growth_dampener) = b.growth_dampener {
        LGALLOC_GROWTH_DAMPENER.store(growth_dampener, Ordering::Relaxed);
    }
    if let Some(local_buffer_bytes) = b.local_buffer_bytes {
        LOCAL_BUFFER_BYTES.store(local_buffer_bytes, Ordering::Relaxed);
    }

    let decay = b.background_decay.map(|d| {
        if d.is_nan() { 0.5 } else { d.clamp(0.0, 1.0) }
    });
    let update = BackgroundConfigUpdate {
        interval: b.background_interval,
        clear_bytes: b.background_clear_bytes,
        decay,
    };
    let any_background = update.interval.is_some()
        || update.clear_bytes.is_some()
        || update.decay.is_some();
    if any_background {
        send_background_update(stealer, update)?;
    }
    Ok(())
}

fn send_background_update(
    stealer: &'static GlobalStealer,
    update: BackgroundConfigUpdate,
) -> Result<(), ConfigError> {
    let mut lock = stealer.background_sender.lock().expect("lock poisoned");
    let leftover = if let Some((_, sender)) = &*lock {
        match sender.send(update.clone()) {
            Ok(()) => None,
            Err(err) => Some(err.0),
        }
    } else {
        Some(update)
    };
    if let Some(update) = leftover {
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut worker = BackgroundWorker::new(receiver);
        let join_handle = std::thread::Builder::new()
            .name("hugalloc-0".to_string())
            .spawn(move || worker.run())
            .map_err(ConfigError::BackgroundWorkerFailed)?;
        sender.send(update).expect("Receiver exists");
        *lock = Some((join_handle, sender));
    }
    Ok(())
}

/// Determine global statistics per size class.
///
/// This function is relatively fast. It reads atomic counters and lock-free queue lengths
/// without issuing syscalls.
///
/// Note that this function takes a read lock on various structures, which can block refills
/// until the function returns.
///
/// # Panics
///
/// Panics if the internal state of hugalloc is corrupted.
pub fn stats() -> Stats {
    let global = GlobalStealer::get_static();

    let mut size_class_stats = Vec::with_capacity(VALID_SIZE_CLASS.len());
    for (index, state) in global.size_classes.iter().enumerate() {
        let size_class = SizeClass::from_index(index);
        let size_class_bytes = size_class.byte_size();

        size_class_stats.push((size_class_bytes, SizeClassStats::from(state)));
    }

    Stats {
        size_class: size_class_stats,
    }
}

/// Statistics about hugalloc's internal behavior.
#[derive(Debug)]
pub struct Stats {
    /// Per size-class statistics.
    pub size_class: Vec<(usize, SizeClassStats)>,
}

/// Statistics per size class.
#[derive(Debug)]
pub struct SizeClassStats {
    /// Number of areas backing a size class.
    pub areas: usize,
    /// Total number of bytes summed across all areas.
    pub area_total_bytes: usize,
    /// Free regions
    pub free_regions: usize,
    /// Clean free regions in the global allocator
    pub clean_regions: usize,
    /// Regions in the global allocator
    pub global_regions: usize,
    /// Regions retained in thread-local allocators
    pub thread_regions: usize,
    /// Total allocations
    pub allocations: u64,
    /// Total slow-path allocations (globally out of memory)
    pub slow_path: u64,
    /// Total refills
    pub refill: u64,
    /// Total deallocations
    pub deallocations: u64,
    /// Total times memory has been returned to the OS (eager reclamation) in regions.
    pub clear_eager_total: u64,
    /// Total times memory has been returned to the OS (slow reclamation) in regions.
    pub clear_slow_total: u64,
}

impl From<&SizeClassState> for SizeClassStats {
    fn from(size_class_state: &SizeClassState) -> Self {
        let areas = size_class_state.area_count.load(Ordering::Relaxed);
        let area_total_bytes = size_class_state.total_bytes.load(Ordering::Relaxed);
        let global_regions = size_class_state.injector.len();
        let clean_regions = size_class_state.clean_injector.len();
        let stealers = size_class_state.stealers.read().expect("lock poisoned");
        let mut thread_regions = 0;
        let mut allocations = 0;
        let mut deallocations = 0;
        let mut refill = 0;
        let mut slow_path = 0;
        let mut clear_eager_total = 0;
        let mut clear_slow_total = 0;
        for thread_state in stealers.values() {
            thread_regions += thread_state.stealer.len();
            let thread_stats = &*thread_state.alloc_stats;
            allocations += thread_stats.allocations.load(Ordering::Relaxed);
            deallocations += thread_stats.deallocations.load(Ordering::Relaxed);
            refill += thread_stats.refill.load(Ordering::Relaxed);
            slow_path += thread_stats.slow_path.load(Ordering::Relaxed);
            clear_eager_total += thread_stats.clear_eager.load(Ordering::Relaxed);
            clear_slow_total += thread_stats.clear_slow.load(Ordering::Relaxed);
        }

        let free_regions = thread_regions + global_regions + clean_regions;

        let global_stats = &size_class_state.alloc_stats;
        allocations += global_stats.allocations.load(Ordering::Relaxed);
        deallocations += global_stats.deallocations.load(Ordering::Relaxed);
        refill += global_stats.refill.load(Ordering::Relaxed);
        slow_path += global_stats.slow_path.load(Ordering::Relaxed);
        clear_eager_total += global_stats.clear_eager.load(Ordering::Relaxed);
        clear_slow_total += global_stats.clear_slow.load(Ordering::Relaxed);
        Self {
            areas,
            area_total_bytes,
            free_regions,
            global_regions,
            clean_regions,
            thread_regions,
            allocations,
            deallocations,
            refill,
            slow_path,
            clear_eager_total,
            clear_slow_total,
        }
    }
}

/// Fixed-capacity uninitialized allocation. lgalloc-backed when possible,
/// heap fallback otherwise.
///
/// Analogous to `Box<[MaybeUninit<T>]>` but with a hugalloc backing and an
/// explicit fallback mode.
///
/// `RawBuffer` does **not** drop its elements. Its `Drop` only releases the
/// backing. Users who need element destructors should convert to
/// [`Buffer`] via [`RawBuffer::assume_init_buffer`] after initialization.
///
/// # Zeroing
///
/// No `_zeroed` constructor exists. Fresh lgalloc regions may contain stale
/// bytes from prior use, and memset-zero forces full residency up front
/// (which on THP-backed memory is a major cost). To zero a buffer
/// explicitly:
///
/// ```ignore
/// let mut raw: RawBuffer<u64> = RawBuffer::with_capacity(n);
/// raw.as_uninit_slice_mut().fill(MaybeUninit::zeroed());
/// ```
pub struct RawBuffer<T> {
    handle: Option<Handle>,
    ptr: NonNull<T>,
    capacity: usize,
}

unsafe impl<T: Send> Send for RawBuffer<T> {}

impl<T> RawBuffer<T> {
    /// Allocate `n` elements of `T`. Tries lgalloc first; falls back to heap
    /// on allocation failure.
    pub fn with_capacity(n: usize) -> Self {
        match Self::try_lgalloc(n) {
            Ok(buf) => buf,
            Err(_) => Self::heap(n),
        }
    }

    /// Allocate `n` elements of `T` from lgalloc. Returns `AllocError` if the
    /// allocator is disabled, out of memory, or the element type cannot be
    /// satisfied.
    pub fn try_lgalloc(n: usize) -> Result<Self, AllocError> {
        let (ptr, capacity, handle) = allocate::<T>(n)?;
        Ok(Self {
            handle: Some(handle),
            ptr,
            capacity,
        })
    }

    /// Allocate `n` elements of `T` from the system heap, bypassing lgalloc.
    pub fn heap(n: usize) -> Self {
        let mut vec: Vec<MaybeUninit<T>> = Vec::with_capacity(n);
        let capacity = vec.capacity();
        let ptr = NonNull::new(vec.as_mut_ptr()).expect("allocator returned null");
        std::mem::forget(vec);
        Self {
            handle: None,
            ptr: ptr.cast::<T>(),
            capacity,
        }
    }

    /// Capacity, in elements.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Whether the backing is an lgalloc handle (true) or a heap fallback (false).
    pub fn is_lgalloc(&self) -> bool {
        self.handle.is_some()
    }

    /// View the buffer as a slice of `MaybeUninit<T>`.
    pub fn as_uninit_slice(&self) -> &[MaybeUninit<T>] {
        // SAFETY: ptr is valid for `capacity` elements; MaybeUninit requires no initialization.
        unsafe {
            std::slice::from_raw_parts(self.ptr.as_ptr().cast::<MaybeUninit<T>>(), self.capacity)
        }
    }

    /// View the buffer as a mutable slice of `MaybeUninit<T>`.
    pub fn as_uninit_slice_mut(&mut self) -> &mut [MaybeUninit<T>] {
        // SAFETY: ptr is valid for `capacity` elements; MaybeUninit requires no initialization.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.ptr.as_ptr().cast::<MaybeUninit<T>>(),
                self.capacity,
            )
        }
    }

    /// Consume the buffer into its raw parts.
    pub fn into_raw_parts(self) -> (NonNull<T>, usize, Option<Handle>) {
        // SAFETY: reading by value out of self and forgetting self; no double-drop.
        let parts = unsafe {
            (
                std::ptr::read(&self.ptr),
                self.capacity,
                std::ptr::read(&self.handle),
            )
        };
        std::mem::forget(self);
        parts
    }

    /// Reconstruct a `RawBuffer` from its raw parts.
    ///
    /// # Safety
    ///
    /// The pointer, capacity, and handle must have come from a prior call to
    /// [`RawBuffer::into_raw_parts`] on the same process, and none of them
    /// must have been freed or reconstructed since.
    pub unsafe fn from_raw_parts(
        ptr: NonNull<T>,
        capacity: usize,
        handle: Option<Handle>,
    ) -> Self {
        Self { handle, ptr, capacity }
    }

    /// Convert into a [`Buffer`] with the first `len` elements assumed initialized.
    ///
    /// # Safety
    ///
    /// Elements `0..len` of the buffer must be initialized.
    ///
    /// # Panics
    ///
    /// Panics if `len > self.capacity`.
    pub unsafe fn assume_init_buffer(self, len: usize) -> Buffer<T> {
        assert!(
            len <= self.capacity,
            "assume_init_buffer len ({len}) exceeds capacity ({})",
            self.capacity
        );
        Buffer { raw: self, len }
    }

    /// Prefetch the byte range covered by the given element range.
    ///
    /// Element offsets are converted to byte offsets via
    /// `checked_mul(size_of::<T>())`. On overflow, returns
    /// `AdviseError::OutOfBounds` with saturating byte values.
    pub fn prefetch(&self, range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_element_range(range, libc::MADV_WILLNEED)
    }

    /// Mark a byte range as cold. See [`Handle::cold`].
    pub fn cold(&self, range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_element_range(range, MADV_COLD_STRATEGY)
    }

    /// Request eviction of a byte range. See [`Handle::pageout`].
    pub fn pageout(&self, range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_element_range(range, MADV_PAGEOUT_STRATEGY)
    }

    fn advise_element_range(
        &self,
        range: Range<usize>,
        advice: libc::c_int,
    ) -> Result<(), AdviseError> {
        let elem_size = std::mem::size_of::<T>();
        let byte_offset = range.start.checked_mul(elem_size);
        let byte_len = range
            .end
            .checked_sub(range.start)
            .and_then(|n| n.checked_mul(elem_size));
        let (byte_offset, byte_len) = match (byte_offset, byte_len) {
            (Some(o), Some(l)) => (o, l),
            _ => {
                let allocation_len = self.capacity.saturating_mul(elem_size);
                return Err(AdviseError::OutOfBounds {
                    byte_offset: range.start.saturating_mul(elem_size),
                    byte_len: range.end.saturating_mul(elem_size).saturating_sub(
                        range.start.saturating_mul(elem_size),
                    ),
                    allocation_len,
                });
            }
        };
        match &self.handle {
            Some(h) => h.advise_range(byte_offset..byte_offset + byte_len, advice),
            None => {
                // Heap-backed: bounds-check against capacity in bytes; no madvise call.
                let allocation_len = self.capacity.saturating_mul(elem_size);
                if byte_offset.saturating_add(byte_len) > allocation_len {
                    Err(AdviseError::OutOfBounds {
                        byte_offset,
                        byte_len,
                        allocation_len,
                    })
                } else {
                    Ok(())
                }
            }
        }
    }
}

impl<T> Drop for RawBuffer<T> {
    fn drop(&mut self) {
        if self.handle.is_some() {
            // The inner Handle's own Drop returns the allocation to the pool when
            // self.handle is dropped along with Self.
        } else {
            // Heap fallback: reconstruct the Vec we originally made and let it drop.
            // SAFETY: heap allocations were made via Vec::<MaybeUninit<T>>::with_capacity;
            // reconstructing with len=0 + same capacity matches the original layout.
            unsafe {
                let _ = Vec::<MaybeUninit<T>>::from_raw_parts(
                    self.ptr.as_ptr().cast::<MaybeUninit<T>>(),
                    0,
                    self.capacity,
                );
            }
        }
    }
}

/// Fixed-capacity length-tracking buffer. Like `Vec<T>` but cannot grow
/// past its initial capacity.
///
/// Backed by a [`RawBuffer<T>`]. Its `Drop` drops the first `len` elements
/// in order, then releases the backing.
pub struct Buffer<T> {
    raw: RawBuffer<T>,
    len: usize,
}

impl<T> Buffer<T> {
    /// Allocate a buffer with capacity for `n` elements of `T`.
    pub fn with_capacity(n: usize) -> Self {
        Self {
            raw: RawBuffer::with_capacity(n),
            len: 0,
        }
    }

    /// Allocate a buffer from lgalloc. See [`RawBuffer::try_lgalloc`].
    pub fn try_lgalloc(n: usize) -> Result<Self, AllocError> {
        Ok(Self {
            raw: RawBuffer::try_lgalloc(n)?,
            len: 0,
        })
    }

    /// Allocate a buffer from the system heap. See [`RawBuffer::heap`].
    pub fn heap(n: usize) -> Self {
        Self {
            raw: RawBuffer::heap(n),
            len: 0,
        }
    }

    /// Length of the buffer (number of initialized elements).
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the buffer contains no elements.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Capacity, in elements.
    pub fn capacity(&self) -> usize {
        self.raw.capacity()
    }

    /// Whether the backing is an lgalloc handle (true) or a heap fallback (false).
    pub fn is_lgalloc(&self) -> bool {
        self.raw.is_lgalloc()
    }

    /// Append a value to the end of the buffer.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is at capacity.
    pub fn push(&mut self, value: T) {
        assert!(self.len < self.raw.capacity(), "buffer at capacity");
        self.raw.as_uninit_slice_mut()[self.len].write(value);
        self.len += 1;
    }

    /// Append the entire slice to the end of the buffer.
    ///
    /// # Panics
    ///
    /// Panics if the buffer has insufficient remaining capacity.
    pub fn extend_from_slice(&mut self, s: &[T])
    where
        T: Copy,
    {
        assert!(
            self.len.checked_add(s.len()).map_or(false, |n| n <= self.raw.capacity()),
            "buffer capacity exceeded"
        );
        let slot = &mut self.raw.as_uninit_slice_mut()[self.len..self.len + s.len()];
        // SAFETY: T: Copy means the source bytes are safe to copy; slot is distinct memory.
        unsafe {
            std::ptr::copy_nonoverlapping(
                s.as_ptr(),
                slot.as_mut_ptr().cast::<T>(),
                s.len(),
            );
        }
        self.len += s.len();
    }

    /// Drop all elements, setting `len` to 0 but preserving capacity.
    pub fn clear(&mut self) {
        // SAFETY: we own the first `len` elements and are about to reset len to 0.
        unsafe {
            let slice: *mut [T] = std::slice::from_raw_parts_mut(
                self.raw.as_uninit_slice_mut().as_mut_ptr().cast::<T>(),
                self.len,
            );
            std::ptr::drop_in_place(slice);
        }
        self.len = 0;
    }

    /// Consume the buffer into its underlying [`RawBuffer`] and length.
    pub fn into_raw_parts(self) -> (RawBuffer<T>, usize) {
        let len = self.len;
        // SAFETY: move raw out without running the Buffer's Drop (which would re-drop elements).
        let raw = unsafe { std::ptr::read(&self.raw) };
        std::mem::forget(self);
        (raw, len)
    }

    /// See [`RawBuffer::prefetch`]. Bounds are against `len`, not `capacity`.
    pub fn prefetch(&self, range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_element_range(range, libc::MADV_WILLNEED)
    }

    /// See [`RawBuffer::cold`]. Bounds are against `len`, not `capacity`.
    pub fn cold(&self, range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_element_range(range, MADV_COLD_STRATEGY)
    }

    /// See [`RawBuffer::pageout`]. Bounds are against `len`, not `capacity`.
    pub fn pageout(&self, range: Range<usize>) -> Result<(), AdviseError> {
        self.advise_element_range(range, MADV_PAGEOUT_STRATEGY)
    }

    fn advise_element_range(
        &self,
        range: Range<usize>,
        advice: libc::c_int,
    ) -> Result<(), AdviseError> {
        // Bounds check against len; anything beyond is always out of bounds for Buffer.
        if range.end > self.len {
            let elem_size = std::mem::size_of::<T>();
            return Err(AdviseError::OutOfBounds {
                byte_offset: range.start.saturating_mul(elem_size),
                byte_len: (range.end.saturating_sub(range.start)).saturating_mul(elem_size),
                allocation_len: self.len.saturating_mul(elem_size),
            });
        }
        self.raw.advise_element_range(range, advice)
    }
}

impl<T> std::ops::Deref for Buffer<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: elements 0..len are initialized.
        unsafe {
            std::slice::from_raw_parts(
                self.raw.as_uninit_slice().as_ptr().cast::<T>(),
                self.len,
            )
        }
    }
}

impl<T> std::ops::DerefMut for Buffer<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: elements 0..len are initialized.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.raw.as_uninit_slice_mut().as_mut_ptr().cast::<T>(),
                self.len,
            )
        }
    }
}

impl<T> Drop for Buffer<T> {
    fn drop(&mut self) {
        // Drop the first `len` initialized elements. The RawBuffer's Drop
        // then releases the backing.
        self.clear();
    }
}

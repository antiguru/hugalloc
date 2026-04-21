use hugalloc::{AllocError, Buffer, RawBuffer};
use std::mem::MaybeUninit;
use std::sync::Mutex;

/// Serializes tests that toggle global lgalloc state (enable/disable).
static GLOBAL_STATE_LOCK: Mutex<()> = Mutex::new(());

fn initialize() {
    hugalloc::builder().enable().apply().expect("apply");
}

#[test]
fn rawbuffer_with_capacity_lgalloc() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap();
    initialize();
    let raw: RawBuffer<u8> = RawBuffer::with_capacity(2 << 20);
    assert!(raw.capacity() >= 2 << 20);
    assert!(raw.is_lgalloc());
}

#[test]
fn rawbuffer_heap_forces_fallback() {
    let raw: RawBuffer<u64> = RawBuffer::heap(1024);
    assert_eq!(raw.capacity(), 1024);
    assert!(!raw.is_lgalloc());
}

#[test]
fn rawbuffer_try_lgalloc_returns_when_disabled() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap();
    // Disable the allocator. try_lgalloc should err with Disabled.
    hugalloc::builder().disable().apply().expect("apply");
    let r: Result<RawBuffer<u8>, AllocError> = RawBuffer::try_lgalloc(2 << 20);
    assert!(matches!(r, Err(AllocError::Disabled)));
    // Restore for subsequent tests.
    initialize();
}

#[test]
fn rawbuffer_into_raw_parts_roundtrip() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap();
    initialize();
    let raw: RawBuffer<u32> = RawBuffer::with_capacity(4096);
    let cap = raw.capacity();
    let is_lg = raw.is_lgalloc();
    let (ptr, cap2, handle) = raw.into_raw_parts();
    assert_eq!(cap, cap2);
    assert_eq!(handle.is_some(), is_lg);
    // Rebuild — must not double-free because into_raw_parts moved out.
    // SAFETY: the parts came from into_raw_parts above.
    let _rebuilt = unsafe { RawBuffer::<u32>::from_raw_parts(ptr, cap2, handle) };
}

#[test]
fn rawbuffer_uninit_slice_roundtrip() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap();
    initialize();
    let mut raw: RawBuffer<u64> = RawBuffer::with_capacity(128);
    let slice: &mut [MaybeUninit<u64>] = raw.as_uninit_slice_mut();
    assert!(slice.len() >= 128);
    slice[0].write(42);
    slice[1].write(99);
    // SAFETY: we wrote the first two elements.
    unsafe {
        assert_eq!(slice[0].assume_init_read(), 42);
        assert_eq!(slice[1].assume_init_read(), 99);
    }
}

#[test]
fn buffer_push_extend_clear() {
    initialize();
    let mut buf: Buffer<u32> = Buffer::with_capacity(16);
    assert_eq!(buf.len(), 0);
    assert!(buf.capacity() >= 16);
    buf.push(1);
    buf.push(2);
    buf.extend_from_slice(&[3, 4, 5]);
    assert_eq!(&*buf, &[1, 2, 3, 4, 5]);
    buf.clear();
    assert_eq!(buf.len(), 0);
    assert!(buf.is_empty());
}

#[test]
fn buffer_drop_runs_element_destructors() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static DROPS: AtomicUsize = AtomicUsize::new(0);

    struct Counter;
    impl Drop for Counter {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    initialize();
    DROPS.store(0, Ordering::Relaxed);
    {
        let mut buf: Buffer<Counter> = Buffer::heap(8);
        buf.push(Counter);
        buf.push(Counter);
        buf.push(Counter);
    }
    assert_eq!(DROPS.load(Ordering::Relaxed), 3);
}

#[test]
fn buffer_assume_init_roundtrip() {
    initialize();
    let mut raw: RawBuffer<u64> = RawBuffer::with_capacity(64);
    for i in 0..64 {
        raw.as_uninit_slice_mut()[i].write(i as u64);
    }
    // SAFETY: we initialized elements 0..64.
    let buf: Buffer<u64> = unsafe { raw.assume_init_buffer(64) };
    assert_eq!(buf.len(), 64);
    assert_eq!(buf[0], 0);
    assert_eq!(buf[63], 63);
}

#[test]
fn rawbuffer_zero_capacity_is_not_lgalloc() {
    initialize();
    let raw: RawBuffer<u8> = RawBuffer::with_capacity(0);
    // Zero-capacity allocations return a dangling handle; is_lgalloc should
    // not lie and claim these are pool-backed.
    assert!(!raw.is_lgalloc());
    assert_eq!(raw.capacity(), 0);
}

use std::mem::{ManuallyDrop, MaybeUninit};
use std::ptr::NonNull;
use std::time::Duration;

use hugalloc::{allocate, AllocError, Handle};

fn initialize() {
    hugalloc::builder()
        .enable()
        .background_interval(Duration::from_secs(1))
        .background_clear_bytes(4 << 20)
        .growth_dampener(1)
        .apply()
        .expect("apply config");
}

struct Wrapper<T> {
    handle: MaybeUninit<Handle>,
    ptr: NonNull<MaybeUninit<T>>,
    cap: usize,
}

unsafe impl<T: Send> Send for Wrapper<T> {}
unsafe impl<T: Sync> Sync for Wrapper<T> {}

impl<T> Wrapper<T> {
    fn allocate(capacity: usize) -> Result<Self, AllocError> {
        let (ptr, cap, handle) = allocate(capacity)?;
        assert!(cap > 0);
        let handle = MaybeUninit::new(handle);
        Ok(Self { ptr, cap, handle })
    }

    fn as_slice(&mut self) -> &mut [MaybeUninit<T>] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.cap) }
    }
}

impl<T> Drop for Wrapper<T> {
    fn drop(&mut self) {
        // SAFETY: the handle was initialized in `allocate`.
        unsafe { self.handle.assume_init_drop() };
    }
}

#[test]
fn test_readme() -> Result<(), AllocError> {
    initialize();

    // Allocate memory
    let (ptr, cap, handle) = allocate::<u8>(2 << 20)?;
    // SAFETY: `allocate` returns a valid memory region and errors otherwise.
    let mut vec = ManuallyDrop::new(unsafe { Vec::from_raw_parts(ptr.as_ptr(), 0, cap) });

    // Write into region, make sure not to reallocate vector.
    vec.extend_from_slice(&[1, 2, 3, 4]);

    // We can read from the vector.
    assert_eq!(&*vec, &[1, 2, 3, 4]);

    // Deallocate after use
    drop(handle);
    Ok(())
}

#[test]
fn allocate_and_write() -> Result<(), AllocError> {
    initialize();
    <Wrapper<u8>>::allocate(4 << 20)?.as_slice()[0] = MaybeUninit::new(1);
    Ok(())
}

#[test]
fn cross_thread_dealloc() -> Result<(), AllocError> {
    hugalloc::builder()
        .enable()
        .apply()
        .expect("apply config");
    let r = <Wrapper<u8>>::allocate(2 << 20)?;

    let thread = std::thread::spawn(move || drop(r));

    thread.join().unwrap();
    Ok(())
}

#[test]
fn zst() -> Result<(), AllocError> {
    initialize();
    <Wrapper<()>>::allocate(10)?;
    Ok(())
}

#[test]
fn zero_capacity_zst() -> Result<(), AllocError> {
    initialize();
    <Wrapper<()>>::allocate(0)?;
    Ok(())
}

#[test]
fn zero_capacity_nonzst() -> Result<(), AllocError> {
    initialize();
    <Wrapper<()>>::allocate(0)?;
    Ok(())
}

#[test]
fn stats() -> Result<(), AllocError> {
    initialize();
    let (_ptr, _cap, handle) = allocate::<usize>(1 << 18)?;
    drop(handle);

    let stats = hugalloc::stats();

    assert!(!stats.size_class.is_empty());

    Ok(())
}

#[test]
fn handle_prefetch_byte_range() -> Result<(), AllocError> {
    initialize();

    let (_, cap, handle) = hugalloc::allocate::<u8>(2 << 20)?;

    handle.prefetch(0..cap).unwrap();
    handle.prefetch(0..64 * 1024).unwrap();
    handle.prefetch(4096..8192).unwrap();
    handle.prefetch(0..0).unwrap();

    assert!(handle.prefetch(0..cap + 1).is_err());
    assert!(handle.prefetch(cap..cap + 1).is_err());

    Ok(())
}

#[test]
fn handle_drop_returns_to_pool() {
    initialize();

    let before = hugalloc::stats()
        .size_class
        .iter()
        .map(|(_, s)| s.deallocations)
        .sum::<u64>();

    {
        let (_, _, _handle) = hugalloc::allocate::<u8>(2 << 20).expect("allocate");
    }

    let after = hugalloc::stats()
        .size_class
        .iter()
        .map(|(_, s)| s.deallocations)
        .sum::<u64>();

    // Other tests in the same binary may deallocate on the same size class in
    // parallel, so the counter can jump by more than one. We only need to
    // confirm Drop actually fired at least once.
    assert!(
        after > before,
        "expected at least one deallocation after handle dropped (before={before}, after={after})"
    );
}

#[test]
fn handle_into_raw_parts_roundtrip() {
    initialize();

    let (_, cap, handle) = hugalloc::allocate::<u8>(2 << 20).expect("allocate");
    let byte_len = cap;

    let (ptr, len) = handle.into_raw_parts();
    assert_eq!(len, byte_len);

    // Reconstruct. Must not double-deallocate; the previous Handle's Drop was
    // suppressed by into_raw_parts calling mem::forget internally.
    // SAFETY: ptr and len just came from into_raw_parts above.
    let rebuilt = unsafe { hugalloc::Handle::from_raw_parts(ptr, len) };
    drop(rebuilt);
    // If double-free happens, subsequent allocations or stats would misbehave.
}

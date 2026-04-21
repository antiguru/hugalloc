use hugalloc::{AdviseError, AllocError};
use std::time::Duration;

fn initialize() {
    hugalloc::builder()
        .enable()
        .background_interval(Duration::from_secs(1))
        .background_clear_bytes(4 << 20)
        .growth_dampener(1)
        .apply()
        .expect("apply config");
}

#[test]
fn handle_cold_ok() -> Result<(), AllocError> {
    initialize();
    let (_, cap, handle) = hugalloc::allocate::<u8>(2 << 20)?;
    handle.cold(0..cap).unwrap();
    handle.cold(0..0).unwrap();
    assert!(matches!(
        handle.cold(0..cap + 1),
        Err(AdviseError::OutOfBounds { .. })
    ));
    Ok(())
}

#[test]
fn handle_pageout_ok() -> Result<(), AllocError> {
    initialize();
    let (_, cap, handle) = hugalloc::allocate::<u8>(2 << 20)?;
    handle.pageout(0..cap).unwrap();
    handle.pageout(0..0).unwrap();
    assert!(matches!(
        handle.pageout(0..cap + 1),
        Err(AdviseError::OutOfBounds { .. })
    ));
    Ok(())
}

#[test]
fn handle_advise_zero_length_noop() -> Result<(), AllocError> {
    initialize();
    let (_, _cap, handle) = hugalloc::allocate::<u8>(2 << 20)?;
    handle.prefetch(0..0).unwrap();
    handle.cold(0..0).unwrap();
    handle.pageout(0..0).unwrap();
    handle.restore_thp(0..0).unwrap();
    Ok(())
}

#[test]
fn handle_restore_thp_after_pageout() -> Result<(), AllocError> {
    initialize();
    let (ptr, cap, handle) = hugalloc::allocate::<u8>(2 << 20)?;
    // Populate so `pageout` has something to act on.
    // SAFETY: pointer is a live 2 MiB mapping.
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), cap) };
    for i in (0..slice.len()).step_by(4096) {
        slice[i] = 1;
    }
    handle.pageout(0..cap).unwrap();
    // restore_thp should succeed regardless of kernel THP/collapse support.
    handle.restore_thp(0..cap).unwrap();
    // Re-touch to confirm the range is still usable.
    for i in (0..slice.len()).step_by(4096) {
        slice[i] = 2;
    }
    assert!(matches!(
        handle.restore_thp(0..cap + 1),
        Err(AdviseError::OutOfBounds { .. })
    ));
    Ok(())
}

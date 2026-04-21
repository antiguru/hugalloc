use std::time::Duration;
use hugalloc::{AdviseError, AllocError, BackgroundWorkerConfig, LgAlloc, lgalloc_set_config};

// TEMPORARY: the old config API is still in place until Task 9.
fn initialize() {
    lgalloc_set_config(
        LgAlloc::new()
            .enable()
            .with_background_config(BackgroundWorkerConfig {
                interval: Duration::from_secs(1),
                clear_bytes: 4 << 20,
            })
            .growth_dampener(1),
    );
}

#[test]
fn handle_cold_ok() -> Result<(), AllocError> {
    initialize();
    let (_, cap, handle) = hugalloc::allocate::<u8>(2 << 20)?;
    handle.cold(0..cap).unwrap();
    handle.cold(0..0).unwrap();
    assert!(matches!(handle.cold(0..cap + 1), Err(AdviseError::OutOfBounds { .. })));
    Ok(())
}

#[test]
fn handle_pageout_ok() -> Result<(), AllocError> {
    initialize();
    let (_, cap, handle) = hugalloc::allocate::<u8>(2 << 20)?;
    handle.pageout(0..cap).unwrap();
    handle.pageout(0..0).unwrap();
    assert!(matches!(handle.pageout(0..cap + 1), Err(AdviseError::OutOfBounds { .. })));
    Ok(())
}

#[test]
fn handle_advise_zero_length_noop() -> Result<(), AllocError> {
    initialize();
    let (_, _cap, handle) = hugalloc::allocate::<u8>(2 << 20)?;
    handle.prefetch(0..0).unwrap();
    handle.cold(0..0).unwrap();
    handle.pageout(0..0).unwrap();
    Ok(())
}

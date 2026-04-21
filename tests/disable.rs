use hugalloc::{allocate, AllocError};

#[test]
fn disabled_allocator_returns_error() {
    hugalloc::builder()
        .disable()
        .apply()
        .expect("apply config");
    assert!(matches!(allocate::<u8>(2 << 20), Err(AllocError::Disabled)));
}

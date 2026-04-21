use std::time::Duration;
use hugalloc::ConfigError;

fn reset_to_defaults() {
    hugalloc::builder()
        .enable()
        .eager_return(false)
        .growth_dampener(1)
        .local_buffer_bytes(32 << 20)
        .background_interval(Duration::from_secs(1))
        .background_clear_bytes(4 << 20)
        .apply()
        .expect("apply full config");
}

#[test]
fn builder_chain_is_expression() -> Result<(), ConfigError> {
    hugalloc::builder()
        .enable()
        .eager_return(true)
        .growth_dampener(4)
        .local_buffer_bytes(64 << 20)
        .background_interval(Duration::from_secs(1))
        .background_clear_bytes(1 << 20)
        .apply()?;
    Ok(())
}

#[test]
fn builder_partial_update_preserves_others() -> Result<(), ConfigError> {
    reset_to_defaults();
    hugalloc::builder().growth_dampener(7).apply()?;
    let _ = hugalloc::allocate::<u8>(2 << 20).expect("allocate");
    Ok(())
}

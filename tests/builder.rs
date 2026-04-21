use std::sync::Mutex;
use std::time::Duration;
use hugalloc::ConfigError;
use hugalloc::stats;

/// Serializes tests that write to the global hugalloc configuration or rely on
/// the background-worker tick rate, which is process-global state.
static GLOBAL_STATE_LOCK: Mutex<()> = Mutex::new(());

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
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap();
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
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap();
    reset_to_defaults();
    hugalloc::builder().growth_dampener(7).apply()?;
    let _ = hugalloc::allocate::<u8>(2 << 20).expect("allocate");
    Ok(())
}

#[test]
fn background_decay_drains_backlog() {
    let _guard = GLOBAL_STATE_LOCK.lock().unwrap();
    // Configure short ticks, small floor, full decay.
    hugalloc::builder()
        .enable()
        .background_interval(Duration::from_millis(50))
        .background_clear_bytes(1 << 21)   // 1 region at min size class
        .background_decay(0.5)
        .apply()
        .expect("apply");

    // Seed backlog: allocate and immediately drop N handles.
    const N: usize = 64;
    for _ in 0..N {
        let (_, _, h) = hugalloc::allocate::<u8>(2 << 20).expect("alloc");
        drop(h);
    }

    // With decay=0.5 and floor=1, backlog drains in ~log2(N) ticks.
    let max_ticks = (N as f64).log2().ceil() as u64 + 4;
    std::thread::sleep(Duration::from_millis(50 * max_ticks));

    let s = stats();
    let backlog: usize = s
        .size_class
        .iter()
        .map(|(_, stat)| stat.global_regions)
        .sum();
    assert!(
        backlog <= 2,
        "backlog still {backlog} after {max_ticks} ticks; decay not draining"
    );
}

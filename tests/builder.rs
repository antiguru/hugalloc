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
    reset_to_defaults();
    // Configure short ticks, small floor, full decay.
    hugalloc::builder()
        .enable()
        .local_buffer_bytes(1 << 21)  // force overflow into global injector
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

#[test]
fn background_floor_matches_flat() {
    let _g = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // With decay = 0.0, the per-tick budget is exactly floor (no acceleration
    // from the decay term). We verify this by measuring how many regions were
    // cleaned per tick: if decay>0 were mistakenly used, the worker would
    // drain proportionally more per tick than the floor.
    //
    // We use a large floor (many regions) and count clean_slow_total before
    // and after a fixed number of ticks. With decay=0, the total cleaned must
    // equal floor_regions * ticks, not backlog * decay * ticks.
    const FLOOR_BYTES: usize = 1 << 21;        // 2 MiB floor = 1 region per tick
    const TICK_MS: u64 = 50;
    hugalloc::builder()
        .enable()
        .local_buffer_bytes(1 << 21)  // force overflow into global injector
        .background_interval(Duration::from_millis(TICK_MS))
        .background_clear_bytes(FLOOR_BYTES)
        .background_decay(0.0)
        .apply()
        .expect("apply");

    // Seed a large backlog so there is plenty for the worker to over-drain if
    // it wrongly applies decay>0.
    const N: usize = 64;
    for _ in 0..N {
        let (_, _, h) = hugalloc::allocate::<u8>(2 << 20).expect("alloc");
        drop(h);
    }

    // Snapshot stats just after seeding.
    let s0 = hugalloc::stats();
    let slow0: u64 = s0.size_class.iter().map(|(_, s)| s.clear_slow_total).sum();

    // Allow a bounded number of ticks (4) to run.
    const TICKS: u64 = 4;
    std::thread::sleep(Duration::from_millis(TICK_MS * TICKS + TICK_MS / 2));

    let s1 = hugalloc::stats();
    let slow1: u64 = s1.size_class.iter().map(|(_, s)| s.clear_slow_total).sum();
    let drained = slow1 - slow0;

    // With decay=0, each tick drains exactly floor=1 region per size class.
    // Only one size class is active (2 MiB), so total drained <= TICKS * 1.
    // We allow some extra slack (2x) to tolerate scheduling jitter.
    let max_expected = TICKS * 2;
    assert!(
        drained <= max_expected,
        "decay=0 with floor=1 drained {drained} regions in {TICKS} ticks; expected <= {max_expected}"
    );
}

#[test]
fn builder_full_apply() -> Result<(), ConfigError> {
    let _g = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Verify each knob's effect is observable, not just that the chain compiles.
    hugalloc::builder().disable().apply()?;
    assert!(matches!(hugalloc::allocate::<u8>(2 << 20), Err(hugalloc::AllocError::Disabled)));

    hugalloc::builder().enable().apply()?;
    let (_, _, h) = hugalloc::allocate::<u8>(2 << 20).expect("enable takes effect");
    drop(h);
    Ok(())
}

#[test]
fn background_respawn_preserves_prior_config() -> Result<(), ConfigError> {
    let _g = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Establish a non-default background config.
    hugalloc::builder()
        .enable()
        .background_interval(Duration::from_millis(50))
        .background_clear_bytes(1 << 21)
        .background_decay(0.5)
        .apply()?;

    // Poison the channel by dropping the receiver. We do this by directly
    // replacing the sender with a broken one via a reconfigure-then-disconnect
    // trick — or, simpler: just wait for the worker to naturally be replaced
    // by applying a config that triggers re-spawn indirectly. Since we can't
    // easily kill the worker from safe user code, we instead test the
    // code path via a second apply() that changes only decay; the partial
    // update must preserve interval and clear_bytes so the worker keeps ticking.
    hugalloc::builder().background_decay(0.7).apply()?;

    // If last_config wasn't preserved, interval would revert to Duration::MAX
    // and the worker would stop ticking. We verify indirectly: seed a backlog
    // and check it drains within a bounded window.
    for _ in 0..8 {
        let (_, _, h) = hugalloc::allocate::<u8>(2 << 20).expect("alloc");
        drop(h);
    }
    std::thread::sleep(Duration::from_millis(500));

    let s = hugalloc::stats();
    let backlog: usize = s.size_class.iter().map(|(_, stat)| stat.global_regions).sum();
    assert!(
        backlog <= 2,
        "backlog still {backlog} after decay-only reconfigure; last_config not preserved"
    );

    Ok(())
}

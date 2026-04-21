# hugalloc

Transparent-huge-page large-object allocator. Forked from `lgalloc` 0.7.0.

## Integration tests serialize global state

`tests/builder.rs` and `tests/buffer.rs` each hold a `GLOBAL_STATE_LOCK: Mutex<()>`. Any test that calls `hugalloc::builder()...apply()` or otherwise mutates the allocator's globals (`enabled`, background config, `local_buffer_bytes`, etc.) must acquire that lock at the top of the test:

```rust
let _g = GLOBAL_STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
```

Cargo runs tests-within-a-binary in parallel; tests that race on the globals without the lock flake. Use `unwrap_or_else(|e| e.into_inner())` so a prior panic in another test doesn't poison subsequent runs.

## Size-class floor is 2 MiB

`MIN_ALLOCATION_BYTES = 1 << 21 = 2 MiB`; `MAX_ALLOCATION_BYTES = 1 << 37 = 128 GiB` (see the public constants in `src/lib.rs`). The smallest region the allocator hands out is `MIN_ALLOCATION_BYTES` bytes. Consequences:

- `hugalloc::allocate::<u8>(small_n)` rounds up to 2 MiB.
- Tests that assert on `global_regions` counts need `local_buffer_bytes` set small enough that dropped handles overflow the thread-local cache into the global injector. `local_buffer_bytes(1 << 21)` keeps one region per thread; further drops go global.
- A `background_clear_bytes` of less than 2 MiB produces a floor of 0 regions; the worker falls back to the ceiling-1 minimum. Set explicitly in tests.

## Platform advisories use runtime sentinels, not `cfg`

`MADV_COLD` and `MADV_PAGEOUT` are Linux-only. Rather than `cfg`-gating the `madvise` call site, the constants are declared with a sentinel on non-Linux:

```rust
#[cfg(target_os = "linux")]
const MADV_COLD_STRATEGY: libc::c_int = libc::MADV_COLD;
#[cfg(not(target_os = "linux"))]
const MADV_COLD_STRATEGY: libc::c_int = -1;
```

Inside `Handle::advise_range`, `if advice != -1 { libc::madvise(...) }` skips the syscall when the sentinel is passed. Bounds checks still run on all platforms. `-1` is never a valid `madvise` advice, so the sentinel cannot escape to the kernel.

If you add another Linux-only advisory, follow the same pattern.

## Commit message style

Lowercase scoped prefix, then a short imperative description:

```
bg: re-spawn worker with full last-applied config instead of defaults
docs: clarify AdviseError::OutOfBounds allocation_len semantics per layer
Handle: byte-level prefetch via private advise_range helper
rename: PrefetchError -> AdviseError
```

Scopes in use: `bg` (background worker), `buffer` (Buffer/RawBuffer), `config` (Builder), `docs`, `rename`, `tests`, `deps`, `ci`, `cleanup`, and type-prefixed scopes for significant API changes (`Handle`, `Buffer`, `RawBuffer`).

Multi-line messages via HEREDOC. Include the `Co-Authored-By` trailer for Claude-assisted commits.

## Testing

```
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps
```

All three must be clean before pushing.

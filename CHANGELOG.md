# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-04-21

**Forked from [`lgalloc`](https://github.com/antiguru/rust-lgalloc) 0.7.0.**
`lgalloc` is in maintenance mode; `hugalloc` is the successor line.

### Added

- `Handle::cold` and `Handle::pageout` — cooperative residency advisories
  (`MADV_COLD` / `MADV_PAGEOUT` on Linux, silent no-op on non-Linux).
- `RawBuffer<T>`: fixed-capacity uninitialized allocation with lgalloc-or-heap backing.
- `Buffer<T>`: length-tracking wrapper over `RawBuffer<T>` with a Vec-like surface.
- Typed advisory wrappers (`prefetch` / `cold` / `pageout`) on `RawBuffer<T>` and `Buffer<T>`
  that take element-unit ranges and forward to the byte-level `Handle` methods.
- `hugalloc::builder()` fluent consuming-self configuration API.
- `Handle::into_raw_parts` / `Handle::from_raw_parts` for FFI / advanced lifecycle.
- Adaptive geometric-decay background clear (`Builder::background_decay`), drains
  backlogs in `O(log N)` ticks instead of linearly.

### Changed

- `Handle::prefetch` now takes a **byte** `Range<usize>` (was an element-unit range
  via turbofish `T`).
- `Handle` gains a `Drop` impl: allocations are returned to the pool when the handle
  is dropped. Callers who want the old "leak unless deallocate is called" behavior
  use `std::mem::forget(handle)` explicitly.
- Rust edition 2024, MSRV 1.85.

### Renamed

- `PrefetchError` → `AdviseError`.
- `LgAllocStats` → `Stats`; `lgalloc_stats()` → `hugalloc::stats()`.

### Removed

- Public `deallocate(Handle)` free function — `Handle::drop` replaces it.
- `LgAlloc` config struct — replaced by private `Builder` returned from `builder()`.
- `BackgroundWorkerConfig` public type — knobs are flattened into `Builder` methods.
- `lgalloc_set_config` free function — replaced by `Builder::apply`.

# hugalloc

Transparent-huge-page large-object allocator with cooperative residency control.

## Relationship to `lgalloc`

`hugalloc` is a fork of [`lgalloc`](https://github.com/antiguru/rust-lgalloc) 0.7.0.
Upstream `lgalloc` is in maintenance mode; new development happens here.

## Features

- 2 MiB–128 GiB allocations backed by anonymous `mmap` + `MADV_HUGEPAGE` on Linux.
- Cooperative residency control: `prefetch`, `cold`, `pageout` advisories for fine-grained
  kernel page hints.
- `Buffer<T>` / `RawBuffer<T>` wrappers with automatic heap fallback when the allocator
  is disabled or out of memory.
- Fluent consuming-self configuration.
- Per-thread region cache; adaptive backlog drain.

## Install

```toml
[dependencies]
hugalloc = "0.1"
```

## Usage

```rust,no_run
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    hugalloc::builder()
        .enable()
        .eager_return(true)
        .background_interval(Duration::from_secs(1))
        .background_clear_bytes(4 << 20)
        .apply()?;

    let mut buf: hugalloc::Buffer<u8> = hugalloc::Buffer::with_capacity(2 << 20);
    buf.extend_from_slice(b"hello hugalloc");
    buf.cold(0..buf.len())?;  // Hint: this range is unlikely to be accessed soon.
    Ok(())
}
```

## Platform notes

- Linux ≥ 5.4: full feature set, including `MADV_COLD` / `MADV_PAGEOUT`.
- Older Linux: advisories still work at the API level; `MADV_COLD` / `MADV_PAGEOUT`
  return `EINVAL` silently, which is consistent with the "hint, never affects correctness"
  contract.
- macOS / BSD: `prefetch` works (via `MADV_WILLNEED`); `cold` / `pageout` are silent
  no-ops. No transparent huge pages; the allocator pool still functions.

## License

MIT OR Apache-2.0.

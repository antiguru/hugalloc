# Running hugalloc benchmarks on a target host

## Prerequisites

* Rust toolchain (stable)
* `systemd-run` (systemd 232+, any modern Linux)
* Swap enabled (for paging benchmarks)

## Build

```sh
cargo build --release --bench alloc_bench
```

The binary is at `target/release/deps/alloc_bench-*` (pick the newest non-`.d` file).

## Throughput benchmarks

Measures hugalloc, system allocator, and raw mmap across 1/2/4/8/16 threads.
Limits: 4G RAM, no swap, 16 CPUs.

```sh
BENCH=$(ls -t target/release/deps/alloc_bench-* | grep -v '\.d$' | head -1)
systemd-run --user -p MemoryMax=4G -p MemorySwapMax=0 -p CPUQuota=1600% \
  --wait --pipe "$BENCH"
```

Adjust `CPUQuota` to match the host (100% per core, e.g. 800% for 8 cores).

## Paging benchmarks

Allocates 3x the memory limit, forces swap, measures random read latency (CCDF)
and realloc+touch latency from a swapped pool.

```sh
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G -p CPUQuota=1600% \
  --wait --pipe "$BENCH" -- --paging
```

For heavier swap pressure:

```sh
systemd-run --user -p MemoryMax=128M -p MemorySwapMax=4G -p CPUQuota=1600% \
  --wait --pipe "$BENCH" -- --paging
```

## What to look at

* **hugalloc vs sysalloc+touch**: the main comparison.
  hugalloc reuses faulted pages; the system allocator mmaps/munmaps on each cycle.
* **sysalloc+touch vs sysalloc+nohuge+touch**: quantifies THP benefit for the system allocator.
  If the host has THP=`never`, these should be similar.
* **Scaling**: hugalloc should scale linearly (lock-free work-stealing);
  sysalloc and mmap degrade due to kernel mmap_lock contention.
* **Paging CCDF**: bimodal distribution — resident pages (µs) vs swap-in (ms).
  The tail shows swap I/O latency of the target host's storage.
* **realloc+touch from swapped pool**: hugalloc recycles virtual addresses without syscalls,
  so the cost is just page faults, not mmap+fault.

## Ratio sweep

Fixes a working set size and varies the cgroup RAM limit to explore the
relationship between resident-to-swap ratio and latency.

```sh
for RAM in 4096 2048 1024 512 256; do
  SWAP=$((8192 - RAM))
  systemd-run --user \
    -p MemoryMax=${RAM}M -p MemorySwapMax=${SWAP}M -p CPUQuota=1600% \
    --wait --pipe "$BENCH" -- --ratio-sweep --total-mib 4096
done
```

### Findings — EBS vs NVMe swap (16-vCPU r6gd, THP=madvise)

#### Random reads (1 thread, 4 GiB working set)

| RAM    | Ratio | EBS ops/s | NVMe ops/s | NVMe p50 | NVMe p99  |
|--------|-------|-----------|------------|----------|-----------|
| 4096M  | 1:1   |   232,041 |  1,545,189 |   181ns  |    533ns  |
| 2048M  | 1:2   |       156 |     23,174 |    75µs  |    241µs  |
| 1024M  | 1:4   |         — |     16,504 |    77µs  |    167µs  |
|  512M  | 1:8   |        93 |     12,996 |    78µs  |    251µs  |
|  256M  | 1:16  |       144 |     12,578 |    77µs  |    242µs  |

On EBS, throughput collapsed to ~100 ops/s once swapping, dominated by
per-page swap-in latency (~130 ms p99). On NVMe, swap-in completes in
~80 µs (p50) with sub-300 µs p99 — a **100–150× throughput improvement**
and **~500× p99 improvement**.

The ratio still matters on NVMe (23K → 13K from 1:2 → 1:8), but the
degradation is gradual rather than catastrophic.

Multi-threaded scaling is near-linear on NVMe:

| RAM    | 1 thr    | 4 thr    | 8 thr     | 16 thr    |
|--------|----------|----------|-----------|-----------|
| 2048M  |  23,174  |  91,094  |  161,827  |  268,920  |
|  512M  |  12,996  |  52,354  |   94,063  |  156,873  |

#### Realloc+touch is unaffected by swap pressure

hugalloc recycles the same virtual pages; the kernel keeps the hot working page
resident since it is touched immediately after dealloc:

| RAM    | Ratio | ops/s   | p50   |
|--------|-------|---------|-------|
| 4096M  | 1:1   | 465,828 | 2.2µs |
|  512M  | 1:8   | 453,985 | 2.2µs |

Scaling is near-linear up to 16 threads (~7.8 M ops/s) at every ratio.

### Paging benchmark (CCDF)

512 MiB RAM, 1536 MiB allocated (3× overcommit), NVMe swap:

| Metric                 | 1 thread |
|------------------------|----------|
| random_read ops/s      |   17,392 |
| random_read p50        |   76.2µs |
| random_read p99        |  240.5µs |
| random_read max        |    3.7ms |
| realloc+touch ops/s    |  447,291 |
| realloc+touch p50      |    2.3µs |

CCDF is bimodal: ~50% of reads hit resident pages (< 300 ns), ~50% swap in
at ~76–87 µs. The tail (p99.9 = 252 µs, max = 3.7 ms) reflects NVMe device
latency under queue depth, not EBS network round-trips.

### madvise strategy experiments

#### EBS (1 thread, 512 M RAM / 4 GiB working set)

| Strategy              | ops/s | p50    | p99    | p999   | max    |
|-----------------------|-------|--------|--------|--------|--------|
| baseline              | 2,155 | 468µs  | 777µs  | 2.3ms  | 209ms  |
| MADV_RANDOM           | 2,334 | 497µs  | 738µs  | 1.1ms  | 4.4ms  |
| MADV_SEQUENTIAL       | 2,363 | 498µs  | 729µs  | 925µs  | 2.8ms  |
| prefetch (32 ahead)   | 3,618 | 322µs  | 503µs  | 1.6ms  | 19ms   |
| batch8+WILLNEED       | 3,724 | 314µs  | 562µs  | 949µs  | 2.2ms  |
| batch32+WILLNEED      | 3,605 | 319µs  | 520µs  | 790µs  | 2.9ms  |
| batch128+WILLNEED     | 3,764 | 1.3µs  | 445µs  | 637µs  | 10ms   |
| MADV_RANDOM+pf (8thr) | 7,032 | 377ns  | 15.3ms | 30ms   | 263ms  |

#### NVMe (1 thread, 512 M RAM / 4 GiB working set)

| Strategy              | ops/s   | p50    | p99    | p999   | max    |
|-----------------------|---------|--------|--------|--------|--------|
| baseline              |  12,996 | 78µs   | 251µs  | 298µs  | 5.8ms  |
| MADV_RANDOM           |  12,725 | 78µs   | 251µs  | 452µs  | 1.9ms  |
| MADV_SEQUENTIAL       |  12,825 | 77µs   | 250µs  | 279µs  | 817µs  |
| prefetch (32 ahead)   | 150,411 | 1.5µs  | 33µs   | 109µs  | 23.9ms |
| batch8+WILLNEED       |  30,362 | 4.4µs  | 162µs  | 219µs  | 402µs  |
| batch32+WILLNEED      |  78,060 | 837ns  | 147µs  | 164µs  | 412µs  |
| batch128+WILLNEED     | 136,947 | 846ns  | 31µs   | 135µs  | 330µs  |
| MADV_RANDOM+pf (8thr) | 241,035 | 1.3µs  | 591µs  | 830µs  | 12ms   |

#### Analysis

* **MADV_RANDOM/SEQUENTIAL** barely help on either storage backend (~2–8%).
  Default readahead is already small for random patterns.
* **MADV_WILLNEED prefetch** is the clear winner on both backends, but the
  benefit is dramatically larger on NVMe: **150K ops/s** (12× baseline) vs
  3.6K (1.7× baseline) on EBS. NVMe's low latency means the prefetch thread
  can keep the I/O pipeline full — most pages are already resident by the time
  the reader reaches them.
* **batch128+WILLNEED** achieves sub-microsecond p50 on both backends, but
  NVMe sustains **137K ops/s** vs 3.8K on EBS.
* **MADV_RANDOM + prefetch at 8 threads** reaches **241K ops/s** on NVMe
  (19× baseline), approaching the all-resident rate. On EBS the same strategy
  managed 7K ops/s with a 263 ms max — the network round-trip dominates.
* No strategy degrades the all-fits case (~1.5M ops/s at 1 thread on NVMe).

**Takeaway**: NVMe instance storage transforms swap from "emergency fallback"
to a viable tier. With prefetch, swapped data on NVMe approaches DRAM
throughput for batch workloads. EBS swap is only practical for cold data
that is rarely accessed.

These results motivate the `prefetch_hint` API: callers who know their access
pattern can issue MADV_WILLNEED on specific page ranges ahead of time.

## THP configuration

Check:

```sh
cat /sys/kernel/mm/transparent_hugepage/enabled
```

* `always` — THP on all anonymous mappings (hugalloc and sysalloc both benefit)
* `madvise` — THP only for regions with MADV_HUGEPAGE (hugalloc benefits, sysalloc does not)
* `never` — no THP (hugalloc's MADV_HUGEPAGE hint is a no-op, warns once)

The `sysalloc+nohuge` variants force 4K pages via MADV_NOHUGEPAGE regardless of this setting.

## Pageout / recycle investigation

See <https://github.com/antiguru/hugalloc/issues/1>. This experiment characterizes how Linux advisories, transparent huge pages, and reclaim interact with `MAP_PRIVATE|MAP_ANONYMOUS` regions the way hugalloc uses them. It informs what the allocator should do when a user pages out a region before deallocating.

The harness is a separate bench binary that grows incrementally — each subcommand answers one bounded question and emits a results table. Sections below document current findings; the harness is pinned by commit in this repo so results can be regenerated.

### Build

```sh
cargo build --release --bench pageout_bench
```

Binary at `target/release/deps/pageout_bench-*` (newest non-`.d`).

### `PR_SET_THP_DISABLE` gotcha

Some parent processes set `prctl(PR_SET_THP_DISABLE, 1)` on their children, which silently disables anonymous THP for the whole subtree even when `/sys/kernel/mm/transparent_hugepage/enabled` is `always` or `madvise`. Observed in practice with the Claude Code CLI wrapper — every child shell inherits the flag and `madvise(MADV_HUGEPAGE)` becomes a no-op. `thp_fault_alloc` in `/proc/vmstat` stays at 0 because the fault handler doesn't even attempt a huge page.

The bench clears the flag at startup (`prctl(PR_SET_THP_DISABLE, 0)`) and prints the transition so the run is reproducible regardless of parent. To check manually before running anything:

```sh
grep THP_enabled /proc/self/status
```

A value of `0` means THP is disabled for your shell.

### Subcommands

```sh
BENCH=$(ls -t target/release/deps/pageout_bench-* | grep -v '\.d$' | head -1)

"$BENCH"                    # runs the idle-safe experiments (1, 2+3, 5)
"$BENCH" --baseline         # experiment 1
"$BENCH" --advise           # experiments 2 + 3
"$BENCH" --pool             # experiment 5 (real hugalloc pool)
"$BENCH" --swap-probe       # experiment 6 (THP through swap)
"$BENCH" --split-recovery   # experiment 7 (can a split PMD be coalesced?)
"$BENCH" --collapse-probe   # experiment 8 (COLLAPSE on paged data)
"$BENCH" --recovery         # experiment 9 (recovery path costs)
```

Experiments that need a cgroup memory limit to be meaningful:

```sh
# experiment 4 (advisory + pressure)
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G --wait --pipe "$BENCH" --pressure

# experiment 8 scenarios C / C2 (COLLAPSE after real eviction)
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G --wait --pipe "$BENCH" --collapse-probe

# experiment 9 under pressure (recovery cost when pages are truly evicted)
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G --wait --pipe "$BENCH" --recovery
```

### Experiment 1: baseline first-touch

Populate a fresh `mmap`+`MADV_HUGEPAGE` region. Time the first-page fault and the full populate (writing one byte per 4 KiB slot). Confirms THP is engaging at each size and measures per-PMD fault cost in isolation.

Findings on r6gd.4xlarge (aarch64, kernel 6.17.0-1010-aws, Ubuntu `6.17.0-1010-aws`, THP = `madvise`):

| size   | first_med | full_med  | thp%   | per-2-MiB-fault |
|--------|-----------|-----------|--------|-----------------|
|  2 MiB |    40.9µs |    46.0µs | 100%   | ~46 µs          |
| 16 MiB |    39.4µs |   366.2µs | 100%   | ~47 µs          |
|128 MiB |    40.9µs |    2.92ms | 100%   | ~46 µs          |
|  1 GiB |    44.9µs |   25.20ms | 100%   | ~49 µs          |

`first_med` is the time to write byte 0 on a freshly-mmap'd region — i.e. exactly one PMD fault. `full_med` is the full populate (first PMD fault + the remaining PMDs faulted on demand as the loop walks forward).

**Takeaways**

* Per-PMD (2 MiB) fault + zero-fill cost is ~47 µs, flat from 2 MiB to 1 GiB. No sub-PMD variance — every 2 MiB slot costs the same.
* `full_p99` is within ~10% of `full_med` at every size — low noise, no reclaim/compaction tail on an idle host.
* 100% THP coverage at every size confirms that hugalloc's 2 MiB allocation floor aligns with the kernel's PMD order and that `MADV_HUGEPAGE` is doing its job once `PR_SET_THP_DISABLE` is cleared.

### Experiments 2 + 3: advisory effect + re-touch

For each of `MADV_COLD` / `MADV_DONTNEED` / `MADV_FREE` / `MADV_PAGEOUT`, on a populated THP-backed region:

1. Apply the advisory (timed).
2. Immediately observe residency (via `mincore`) and `AnonHugePages` (via `/proc/self/smaps`).
3. Re-touch the full region (timed).
4. Observe `AnonHugePages` again.

Findings on the same host:

| size   | advice          | adv_med | retouch_med | retouch_p99 | resA% | thpA% | thpB% |
|--------|-----------------|---------|-------------|-------------|-------|-------|-------|
|  2 MiB | `MADV_COLD`     |   2.7µs |       2.2µs |       2.3µs |  100% |  100% |  100% |
|  2 MiB | `MADV_DONTNEED` |   7.0µs |      44.6µs |     110.1µs |    0% |    0% |  100% |
|  2 MiB | `MADV_FREE`     |   3.0µs |       2.3µs |       2.4µs |  100% |  100% |  100% |
|  2 MiB | `MADV_PAGEOUT`  | 336.4µs |      15.6µs |      26.0µs |  100% |    0% |    0% |
| 16 MiB | `MADV_COLD`     |  10.9µs |      17.8µs |      20.3µs |  100% |  100% |  100% |
| 16 MiB | `MADV_DONTNEED` |  44.4µs |     340.3µs |     343.5µs |    0% |    0% |  100% |
| 16 MiB | `MADV_FREE`     |  11.0µs |      18.2µs |      19.2µs |  100% |  100% |  100% |
| 16 MiB | `MADV_PAGEOUT`  |  4.86ms |     367.1µs |     419.8µs |  100% |    0% |    0% |
|128 MiB | `MADV_COLD`     |  91.4µs |     236.6µs |     257.7µs |  100% |  100% |  100% |
|128 MiB | `MADV_DONTNEED` | 361.8µs |      2.99ms |      3.63ms |    0% |    0% |  100% |
|128 MiB | `MADV_FREE`     |  86.7µs |     255.9µs |     274.0µs |  100% |  100% |  100% |
|128 MiB | `MADV_PAGEOUT`  | 101.98ms|      3.10ms |      3.16ms |  100% |    0% |    0% |
|  1 GiB | `MADV_COLD`     | 516.0µs |      1.87ms |      1.89ms |  100% |  100% |  100% |
|  1 GiB | `MADV_DONTNEED` |  2.86ms |     24.48ms |     24.88ms |    0% |    0% |  100% |
|  1 GiB | `MADV_FREE`     | 496.8µs |      1.87ms |      1.88ms |  100% |  100% |  100% |
|  1 GiB | `MADV_PAGEOUT`  |  1.96s  |     25.35ms |     25.58ms |  100% |    0% |    0% |

Column guide:

* `adv_med` — median madvise syscall latency.
* `retouch_med` / `retouch_p99` — time to rewrite every 4 KiB slot after the advisory.
* `resA%` — `mincore` residency right after the advisory (before re-touch).
* `thpA%` — `AnonHugePages` / region size right after the advisory.
* `thpB%` — `AnonHugePages` / region size after re-touch.

**Takeaways**

* **`MADV_DONTNEED` rebuilds THP cleanly.** Retouch cost matches fresh first-touch (~47 µs per PMD) and `AnonHugePages` returns to 100%. The VMA retains its `MADV_HUGEPAGE` hint and fault-in honors it. This is the recycle primitive that preserves hugalloc's performance contract.
* **`MADV_PAGEOUT` splits THP and retouch does not rebuild it.** Post-PAGEOUT `thpA%` = 0, and even after re-touch `thpB%` stays 0. The VMA keeps its `hg` flag (confirmed via `/proc/self/smaps` `VmFlags`) but the pages stay 4 KiB. This is the pool-recycle bug the issue flags.
* **`MADV_PAGEOUT` did not evict to swap on an idle host.** `resA%` is 100% after PAGEOUT, and retouch latency (15 µs on 2 MiB, ~25 ms on 1 GiB) is ~47 µs per PMD — the same cost as re-faulting clean anon pages, nowhere near NVMe swap-in latency (~80 µs per 4 KiB page from the existing paging benchmark). The kernel split the PMDs and marked them cold but never actually wrote anything to swap. Confirming this under memory pressure is experiment 4.
* **`MADV_PAGEOUT` syscall cost is huge and super-linear.** 1 GiB takes ~2 s. Per-PMD cost: ~1.6 ms at 128 MiB, ~3.8 ms at 1 GiB. The act of splitting each PMD and walking its page-table subtree is the dominant cost.
* **`MADV_COLD` and `MADV_FREE` preserve THP and keep pages resident** in the absence of pressure — neither is useful as a "recycle" primitive on its own, because the region hasn't actually been released. They're cheap (~1 µs per MiB) and safe.
* **A `mincore`-gated mitigation does not detect the bug scenario.** Since post-PAGEOUT residency is still 100% on an idle host, any mitigation keying off "`mincore` says the region is cold" would mis-classify a user's pageout'd region as hot and skip DONTNEED.

### Experiment 4: advisory + memory pressure

Same advisories as experiment 2+3, but with a large dirty pressure buffer allocated immediately after the advisory. The pressure buffer exceeds the cgroup memory limit so the kernel is forced to reclaim cold pages.

Required invocation — the bench auto-detects `memory.max` from the cgroup and sizes pressure to ~1.5× the limit:

```sh
BENCH=$(ls -t target/release/deps/pageout_bench-* | grep -v '\.d$' | head -1)
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G --wait --pipe "$BENCH" --pressure
```

Findings (same host, 512 MiB cgroup, 4 GiB swap cap, NVMe swap):

| size   | advice          | resA%  | resAp% | thpA%  | retouch_med | retouch_p99 | thpB%  |
|--------|-----------------|--------|--------|--------|-------------|-------------|--------|
|  2 MiB | `MADV_COLD`     |  100%  |    0%  |  100%  |      6.97ms |      7.06ms |    0%  |
|  2 MiB | `MADV_DONTNEED` |    0%  |    0%  |    0%  |      77.0µs |     15.77ms |  100%  |
|  2 MiB | `MADV_FREE`     |  100%  |    0%  |  100%  |      77.7µs |     366.2µs |  100%  |
|  2 MiB | `MADV_PAGEOUT`  |  100%  |    0%  |    0%  |      7.33ms |     21.87ms |    0%  |
| 16 MiB | `MADV_COLD`     |  100%  |    0%  |  100%  |     55.63ms |     58.07ms |    0%  |
| 16 MiB | `MADV_DONTNEED` |    0%  |    0%  |    0%  |     32.23ms |     32.60ms |  100%  |
| 16 MiB | `MADV_FREE`     |  100%  |    0%  |  100%  |     31.96ms |     32.31ms |  100%  |
| 16 MiB | `MADV_PAGEOUT`  |  100%  |    0%  |    0%  |     56.77ms |     58.61ms |    0%  |
|128 MiB | `MADV_COLD`     |  100%  |    0%  |  100%  |    459.21ms |    460.38ms |    0%  |
|128 MiB | `MADV_DONTNEED` |    0%  |    0%  |    0%  |    256.70ms |    257.74ms |  100%  |
|128 MiB | `MADV_FREE`     |  100%  |    0%  |  100%  |    256.95ms |    257.26ms |  100%  |
|128 MiB | `MADV_PAGEOUT`  |  100%  |    0%  |    0%  |    459.64ms |    462.92ms |    0%  |

`resA%` / `resAp%` are probe residency before and after the pressure buffer is dirtied. `thpB%` is probe `AnonHugePages` after the post-pressure retouch.

**Takeaways**

* **Every advisory reclaims under pressure** (`resAp%` = 0 across the board).
* **`MADV_FREE` under pressure rebuilds THP.** Kernel discards dirty content without swap writeback (that's the whole point of `MADV_FREE`), refault allocates fresh THP. Retouch cost matches `MADV_DONTNEED`'s retouch (they're the same end-state: empty PMDs).
* **`MADV_COLD` and `MADV_PAGEOUT` under pressure do NOT rebuild THP.** Their dirty pages end up in swap as 4 KiB slots; refault reads back from swap at 4 KiB granularity and the PMDs stay split. `thpB%` = 0.
* **`MADV_DONTNEED` is robust across regimes.** Rebuilds THP on refault regardless of pressure. One extra syscall upfront, deterministic result.
* **`MADV_FREE` is conditional on pressure.** On an idle system the advisory is effectively a no-op (experiment 2+3 showed `resA%` = 100 after FREE with no pressure). It only becomes equivalent to `DONTNEED` when the kernel actually reclaims.
* Absolute retouch cost inflates under cgroup thrashing — the pressure buffer is ~1.5× `memory.max`, so every refault competes with ongoing swap. The relative ordering between advisories is the durable signal.

### Experiment 5: pool recycle through the real `hugalloc` API

Three scenarios per size, driven through `hugalloc::allocate` and `Handle::pageout`, with `local_buffer_bytes(0)` so every drop promotes to the global injector (so the allocator's eager-clear path on promotion is actually exercised):

1. **recycle (no pageout)** — baseline, default config.
2. **recycle after pageout (bug)** — caller does `handle.pageout(0..len)` before drop; default config.
3. **+ `eager_return=true`** — same as (2) but the allocator calls `MADV_DONTNEED` on every drop (matches mitigation proposal #2 from the issue).

Each scenario warms up for 5 cycles, then measures 30 cycles. Each cycle: allocate → populate → (maybe pageout) → drop → allocate → time retouch → read `AnonHugePages` for the probe's VMA.

```sh
BENCH=$(ls -t target/release/deps/pageout_bench-* | grep -v '\.d$' | head -1)
"$BENCH" --pool
```

Findings (same host):

| size   | scenario                      | retouch_med | retouch_p99 | thp_kb   | thp%   |
|--------|-------------------------------|-------------|-------------|----------|--------|
|  2 MiB | recycle (no pageout)          |      2.1 µs |      2.1 µs |  2 048   |  100%  |
|  2 MiB | recycle after pageout (bug)   |      2.1 µs |     15.1 µs |      0   |    0%  |
|  2 MiB | + `eager_return=true` (mitig) |    490.1 µs |    506.8 µs |      0   |    0%  |
| 16 MiB | recycle (no pageout)          |     16.7 µs |     23.1 µs | 16 384   |  100%  |
| 16 MiB | recycle after pageout (bug)   |    305.7 µs |    357.1 µs |      0   |    0%  |
| 16 MiB | + `eager_return=true` (mitig) |     4.94 ms |     6.98 ms |      0   |    0%  |
|128 MiB | recycle (no pageout)          |    234.5 µs |    269.1 µs |131 072   |  100%  |
|128 MiB | recycle after pageout (bug)   |     3.00 ms |     3.07 ms |      0   |    0%  |
|128 MiB | + `eager_return=true` (mitig) |    35.88 ms |    45.05 ms |      0   |    0%  |

**Takeaways**

* **The bug is real through the real pool.** `recycle after pageout` hands back a region whose VMA is 4 KiB-backed (`thp%=0`). Retouch latency is low *in absolute terms* only because the pages are still resident at 4 KiB — no fault, just memory-bandwidth writes. Subsequent workloads on the region pay a TLB / walker tax we don't measure here.
* **`eager_return=true` does NOT fix the bug.** Retouch cost is ~10× the baseline `recycle (no pageout)` — i.e. 512 individual 4 KiB faults per 2 MiB PMD instead of one 2 MiB fault. `thp%` is still 0. This was surprising and motivated experiment 7.

### Experiment 7: can a split PMD be re-coalesced?

Given experiment 5's surprise, this experiment checks whether `MADV_DONTNEED` can un-split a PMD that `MADV_PAGEOUT` already split — or if we need a different primitive.

```sh
"$BENCH" --split-recovery
```

Findings:

| size   | sequence                               | final_touch | p99     | thp%   |
|--------|----------------------------------------|-------------|---------|--------|
|  2 MiB | `DONTNEED` (no prior PAGEOUT)          |     44.9 µs | 54.0 µs |  100%  |
|  2 MiB | `PAGEOUT`, `DONTNEED`                  |    462.0 µs |678.0 µs |    0%  |
|  2 MiB | `PAGEOUT`, `DONTNEED`, touch, `COLLAPSE` |  469.4 µs |568.4 µs |  100%  |
| 16 MiB | `DONTNEED` (no prior PAGEOUT)          |    342.4 µs |352.2 µs |  100%  |
| 16 MiB | `PAGEOUT`, `DONTNEED`                  |     4.22 ms | 4.91 ms |    0%  |
| 16 MiB | `PAGEOUT`, `DONTNEED`, touch, `COLLAPSE` |  4.26 ms | 4.74 ms |  100%  |
|128 MiB | `DONTNEED` (no prior PAGEOUT)          |     2.75 ms | 2.85 ms |  100%  |
|128 MiB | `PAGEOUT`, `DONTNEED`                  |    36.06 ms |38.85 ms |    0%  |
|128 MiB | `PAGEOUT`, `DONTNEED`, touch, `COLLAPSE` | 36.29 ms |36.64 ms |  100%  |
|  1 GiB | `DONTNEED` (no prior PAGEOUT)          |    24.30 ms |24.36 ms |  100%  |
|  1 GiB | `PAGEOUT`, `DONTNEED`                  |   288.88 ms |295.79 ms|    0%  |
|  1 GiB | `PAGEOUT`, `DONTNEED`, touch, `COLLAPSE` |284.91 ms|292.24 ms|  100%  |

**Takeaways**

* **`MADV_DONTNEED` cannot un-split a PMD.** Once `MADV_PAGEOUT` has installed a PTE-subtable in place of the PMD entry, `DONTNEED` just zaps the PTEs within that subtable — the subtable itself persists. Subsequent fault-in uses 4 KiB granularity. Retouch cost is ~10× the baseline THP retouch (for 2 MiB: 462 µs vs 45 µs; for 128 MiB: 36 ms vs 2.75 ms).
* **`MADV_COLLAPSE` (kernel ≥ 6.1) does restore THP** — `thp%` back to 100% after `PAGEOUT → DONTNEED → touch → COLLAPSE`. Requires the range to be populated first.
* **Takeaway for hugalloc:** once a caller has pageout'd a region, clearing it before recycling requires more than `MADV_DONTNEED`. The options are:
  1. `MADV_COLLAPSE` after the next user re-populates (Linux 6.1+, opportunistic).
  2. `munmap` + `mmap` the region and re-apply `MADV_HUGEPAGE` — loses VA re-use for this chunk but guarantees a clean PMD.
  3. Track "tainted" handles (those that saw `pageout`) and treat them specially on drop.
* Side note: the probe for `DONTNEED (no prior PAGEOUT)` matches experiment 2+3's `MADV_DONTNEED` row exactly — sanity-check that the setups are consistent.

### Experiment 6: THP survival through a swap round-trip

Direct question: does a 2 MiB transparent huge page survive a `MADV_PAGEOUT` → refault cycle as a unit, or does it always split? Kernel has `CONFIG_THP_SWAP` compiled in, and the `SwapTotal` / NVMe backing exist, so this is a pure behavior question.

For each size, one rep: populate the probe, snapshot `thp_swpout` / `thp_swpout_fallback` in `/proc/vmstat`, run `MADV_PAGEOUT`, read `Swap:` and `AnonHugePages:` from `/proc/self/smaps`, diff the vmstat counters, retouch, read smaps again.

```sh
BENCH=$(ls -t target/release/deps/pageout_bench-* | grep -v '\.d$' | head -1)
"$BENCH" --swap-probe   # no cgroup needed
```

Findings (same host, no cgroup, `SwapTotal = 944 GiB` NVMe):

| size   | thpA_KB | swapA_KB | resA% | Δswpout | Δswp_fb | thpB_KB | swapB_KB | retouch_med |
|--------|---------|----------|-------|---------|---------|---------|----------|-------------|
|  2 MiB |       0 |    2 048 | 100%  |     1   |      0  |       0 |        0 |      17.6µs |
| 16 MiB |       0 |   16 384 | 100%  |     8   |      0  |       0 |        0 |     381.5µs |
|128 MiB |       0 |  131 072 | 100%  |    64   |      0  |       0 |        0 |      3.12ms |
|  1 GiB |       0 |1 048 576 | 100%  |   512   |      0  |       0 |        0 |     25.57ms |

* `thpA_KB` — `AnonHugePages` after PAGEOUT
* `swapA_KB` — `Swap:` (smaps) after PAGEOUT — proves bytes hit swap
* `Δswpout` — increment in `thp_swpout`: count of THPs written to swap as whole PMDs
* `Δswp_fb` — increment in `thp_swpout_fallback`: count of THPs split before swap
* `thpB_KB` / `swapB_KB` — `AnonHugePages` / `Swap:` after retouch

**Takeaways**

* **Swap-out preserves THP as units.** `Δswpout` equals the PMD count of the region exactly (region / 2 MiB), and `Δswp_fb = 0`. The `CONFIG_THP_SWAP` path writes whole 2 MiB units to swap without splitting. `Swap:` in smaps confirms real bytes written.
* **`AnonHugePages` drops to 0 immediately after PAGEOUT** even though the swap-out itself is PMD-granular. The kernel tears down the PMD page-table entry as part of the pageout; from the process's view the mapping is no longer "huge".
* **Pages stay mapped in the idle case.** `resA% = 100` after PAGEOUT and retouch cost is ~pure memory bandwidth (17.6µs for 2 MiB — no fault, no swap I/O). The kernel staged swap entries but didn't unmap the pages; only memory pressure forces unmapping.
* **Swap-in never rebuilds THP.** `thpB = 0` after retouch in every row. Combined with experiment 4's result (`thpB = 0` after cgroup-forced real swap-in of COLD/PAGEOUT pages), this confirms swap-in is 4 KiB-granular on this kernel regardless of how pages got to swap.
* **Round-trip summary: 2 MiB structure is lost.** A region that goes through pageout + refault returns as 512 × 4 KiB pages. The VMA's `MADV_HUGEPAGE` hint governs fresh anon faults only, not swap-in.

### Experiment 8: `MADV_COLLAPSE` on paged data

Follow-up to experiment 7: what happens when `MADV_COLLAPSE` is called on pages that are swap-staged, actually in swap, or otherwise not present as normal 4 KiB PTEs? Does it page them in? Does it bring them back as 2 MiB?

```sh
BENCH=$(ls -t target/release/deps/pageout_bench-* | grep -v '\.d$' | head -1)
"$BENCH" --collapse-probe                                          # idle: scenarios A, B, B2 only
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G --wait \
  --pipe "$BENCH" --collapse-probe                                 # cgroup: also C, C2
```

Findings under a 512 MiB cgroup (4 GiB swap):

| size   | scenario                                | ret     | collapse | resBef | resAft | thpAft |
|--------|-----------------------------------------|---------|----------|--------|--------|--------|
|  2 MiB | `populated THP → COLLAPSE`              |    0    |    2.1µs | 100%   | 100%   |  100%  |
|  2 MiB | `PAGEOUT (idle) → COLLAPSE`             |  EINVAL |    4.8µs | 100%   | 100%   |    0%  |
|  2 MiB | `PAGEOUT → retouch → COLLAPSE`          |    0    |  368.6µs | 100%   | 100%   |  100%  |
|  2 MiB | `PAGEOUT + pressure → COLLAPSE`         |  EINVAL |   41.1µs |   0%   |   0%   |    0%  |
|  2 MiB | `PAGEOUT + pressure + retouch → COLLAPSE` |   0   |  709.4µs | 100%   | 100%   |  100%  |
| 16 MiB | `populated THP → COLLAPSE`              |    0    |    3.2µs | 100%   | 100%   |  100%  |
| 16 MiB | `PAGEOUT (idle) → COLLAPSE`             |  EINVAL |   20.7µs | 100%   | 100%   |    0%  |
| 16 MiB | `PAGEOUT → retouch → COLLAPSE`          |    0    |   2.75ms | 100%   | 100%   |  100%  |
| 16 MiB | `PAGEOUT + pressure → COLLAPSE`         |  EINVAL |   52.8µs |   0%   |   0%   |    0%  |
| 16 MiB | `PAGEOUT + pressure + retouch → COLLAPSE` |   0   |   6.04ms | 100%   | 100%   |  100%  |
|128 MiB | `populated THP → COLLAPSE`              |    0    |    7.6µs | 100%   | 100%   |  100%  |
|128 MiB | `PAGEOUT (idle) → COLLAPSE`             |  EINVAL |  157.5µs | 100%   | 100%   |    0%  |
|128 MiB | `PAGEOUT → retouch → COLLAPSE`          |    0    |  22.66ms | 100%   | 100%   |  100%  |
|128 MiB | `PAGEOUT + pressure → COLLAPSE`         |  EINVAL |  155.2µs |   0%   |   0%   |    0%  |
|128 MiB | `PAGEOUT + pressure + retouch → COLLAPSE` |   0   |  35.64ms | 100%   | 100%   |  100%  |
|  1 GiB | `PAGEOUT + pressure + retouch → COLLAPSE` |  EAGAIN |160.40ms |  50%   |  49%   |   33%  |

**Direct answers**

* **Does `MADV_COLLAPSE` page in?** **No.** It returns `EINVAL` on any range whose PTEs are swap entries (pages either "staged for eviction" by `MADV_PAGEOUT` on an idle host, or fully evicted under pressure). COLLAPSE operates strictly on already-resident normal 4 KiB PTEs.
* **Does it page in as 2 MiB?** **No, because it doesn't page in at all.** Swap-in always happens at 4 KiB granularity (experiment 6 confirms this independently). The caller must first touch the range to force 4 KiB swap-in, then call COLLAPSE, which then coalesces the 4 KiB PTEs into a 2 MiB PMD.
* **Does the retouch+collapse sequence actually restore THP?** Yes, for fully-paged regions that fit in RAM. `PAGEOUT + pressure + retouch → COLLAPSE` ends at 100% THP. The cost is dominated by the retouch (which includes real swap-in from NVMe), not by COLLAPSE itself.
* **Cost envelope:** for a 2 MiB region, retouch-from-swap + COLLAPSE is ~709 µs — about 15× a fresh THP fault (~47 µs). For 128 MiB: ~35.64 ms vs 2.92 ms baseline. A `munmap` + fresh `mmap` + first-touch path would be cheaper for truly-evicted regions because it skips the swap-in entirely.
* **Edge case (1 GiB under 512 MiB cgroup):** retouch can't fully re-populate a 1 GiB region in a 512 MiB budget; the kernel evicts as fast as we fault in. COLLAPSE then returns `EAGAIN` with only partial THP coverage (~33%). This is a kernel policy choice — COLLAPSE won't evict other memory to make room for a THP.

### Experiment 9: recovery path costs

Given experiments 7 and 8 established the set of primitives that can restore THP (COLLAPSE after a touch, or a fresh mapping), this experiment puts end-to-end wall-time numbers on each candidate recovery path, so we can see the cost envelope of each strategy a mitigation could choose.

Setup per rep: `Probe::new` → populate → `MADV_PAGEOUT`, optionally followed by a dirty pressure buffer sized to ~1.5× the cgroup's `memory.max` (which forces the pages to actually evict to swap, not just stage). Recovery sequence is then timed end-to-end.

```sh
BENCH=$(ls -t target/release/deps/pageout_bench-* | grep -v '\.d$' | head -1)
"$BENCH" --recovery                                        # idle regime (pages still mapped)
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G \
  --wait --pipe "$BENCH" --recovery                        # pressure regime (real eviction)
```

#### Pressure regime (cgroup = 512 MiB, pages truly in swap)

| size   | path                               | time_med | time_p99 | res%   | thp%   |
|--------|------------------------------------|----------|----------|--------|--------|
|  2 MiB | touch only (bug baseline)          |  5.65 ms |  5.90 ms | 100%   |    0%  |
|  2 MiB | touch + `COLLAPSE`                 |  6.47 ms |  8.68 ms | 100%   |  100%  |
|  2 MiB | `WILLNEED` + touch                 |  1.49 ms |  1.54 ms | 100%   |    0%  |
|  2 MiB | `WILLNEED` + touch + `COLLAPSE`    |  2.13 ms |  2.23 ms | 100%   |  100%  |
|  2 MiB | **`MAP_FIXED` remap + touch**      | **107.1 µs** | 107.1 µs | 100%   | **100%** |
| 16 MiB | touch only (bug baseline)          | 51.92 ms | 53.28 ms | 100%   |    0%  |
| 16 MiB | touch + `COLLAPSE`                 | 57.29 ms | 58.30 ms | 100%   |  100%  |
| 16 MiB | `WILLNEED` + touch                 | 11.43 ms | 11.94 ms | 100%   |    0%  |
| 16 MiB | `WILLNEED` + touch + `COLLAPSE`    | 16.81 ms | 18.07 ms | 100%   |  100%  |
| 16 MiB | **`MAP_FIXED` remap + touch**      | **696.8 µs** | 712.7 µs | 100%   | **100%** |
|128 MiB | touch only (bug baseline)          |421.21 ms |424.76 ms | 100%   |    0%  |
|128 MiB | touch + `COLLAPSE`                 |459.01 ms |464.44 ms | 100%   |  100%  |
|128 MiB | `WILLNEED` + touch                 | 86.16 ms | 87.37 ms | 100%   |    0%  |
|128 MiB | `WILLNEED` + touch + `COLLAPSE`    |121.70 ms |125.57 ms | 100%   |  100%  |
|128 MiB | **`MAP_FIXED` remap + touch**      |**5.64 ms**| 5.87 ms | 100%   | **100%** |

#### Idle regime (no cgroup, pages stay mapped despite PAGEOUT)

| size   | path                               | time_med | time_p99 | res%   | thp%   |
|--------|------------------------------------|----------|----------|--------|--------|
|  2 MiB | touch only (bug baseline)          |  15.4 µs |  15.8 µs | 100%   |    0%  |
|  2 MiB | touch + `COLLAPSE`                 | 423.4 µs | 719.3 µs | 100%   |  100%  |
|  2 MiB | `WILLNEED` + touch                 |  61.5 µs |  62.7 µs | 100%   |    0%  |
|  2 MiB | `WILLNEED` + touch + `COLLAPSE`    | 494.3 µs |  1.62 ms | 100%   |  100%  |
|  2 MiB | **`MAP_FIXED` remap + touch**      |  62.0 µs |  62.5 µs | 100%   |  100%  |
|128 MiB | touch only (bug baseline)          |  3.06 ms |  3.17 ms | 100%   |    0%  |
|128 MiB | touch + `COLLAPSE`                 | 25.35 ms | 27.24 ms | 100%   |  100%  |
|128 MiB | `WILLNEED` + touch                 |  6.04 ms |  6.08 ms | 100%   |    0%  |
|128 MiB | `WILLNEED` + touch + `COLLAPSE`    | 29.06 ms | 29.49 ms | 100%   |  100%  |
|128 MiB | **`MAP_FIXED` remap + touch**      |  5.78 ms |  5.89 ms | 100%   |  100%  |

**Takeaways**

* **`MAP_FIXED` remap is by far the cheapest path in the pressure regime.** For 128 MiB: 5.6 ms vs 121 ms for WILLNEED+touch+COLLAPSE (~21×) and vs 421 ms for touch-only (~75×). Works because the kernel discards the swap entries associated with the old mapping and the new mapping faults in clean zero pages — no swap read-in, no PMD coalescence step. Matches the baseline fresh-fault cost of ~47 µs per PMD.
* **`MADV_WILLNEED` does accelerate swap-in.** In the pressure regime it cuts touch-only cost by ~3–5× across sizes. The kernel's async swap readahead overlaps I/O effectively. No benefit in the idle regime (pages never left memory), and never restores THP on its own.
* **`MADV_COLLAPSE` overhead is small and predictable.** The collapse work itself takes ~800 µs for 2 MiB to ~40 ms for 128 MiB (add-on on top of the touch). The expensive part of any touch+collapse path is the preceding touch/swap-in, not COLLAPSE itself.
* **In the idle regime, all paths except "touch only" restore 100% THP.** Touch-only is the bug — it just dirties the 4 KiB pages the kernel staged.
* **The difference between idle and pressure regimes is the cost of swap-in.** The bug's cost to callers is therefore load-dependent: on an idle system the only observable damage is 4 KiB TLB pressure in subsequent workloads; under memory pressure the damage compounds because every recycled region has to pay swap-in latency that a fresh mapping wouldn't.

### Implications for hugalloc

The headline finding that changes the calculus: **`MADV_PAGEOUT` permanently splits the PMD, and `MADV_DONTNEED` does not un-split it.** None of the three DONTNEED-based mitigations from the issue restore THP backing after a caller has pageout'd a region. The current `eager_return=true` config is not a fix — experiment 5 shows it still yields `thp%=0` through the pool.

* **The bug is not repaired by `MADV_DONTNEED` alone.** Once the caller's `MADV_PAGEOUT` splits the PMD, any subsequent single-advisory recycle leaves the PMD as a 4 KiB subtable. Re-fault uses 4 KiB granularity and retouch cost is ~10× baseline THP retouch for 2 MiB, ~13× for 128 MiB.
* **Mitigation proposals from the issue revisited:**
  * #1 (unconditional `DONTNEED` on dealloc) — **does not fix THP loss**, though it cleanly zeros residency. Still worth doing for memory-accounting reasons, but doesn't restore the allocator's performance contract.
  * #2 (`DONTNEED` only on promotion to global injector, currently behind `eager_return=true`) — same: **does not fix THP loss**. Experiment 5 confirms.
  * #3 (mincore-gated `DONTNEED`) — residency stays 100% on an idle host after user's PAGEOUT, so the gate would never fire; the advisory that does fire (DONTNEED) wouldn't fix it anyway.
  * #4 (doc-only) — remains a valid choice if the policy is to push the burden to callers. See "Guidance for callers" below.
* **Primitives that do restore THP backing** (end-to-end costs measured in experiment 9 under a 512 MiB cgroup with pages truly in swap):
  1. **`MAP_FIXED` remap + re-apply `MADV_HUGEPAGE`** — by far the cheapest. 2 MiB: ~107 µs; 128 MiB: ~5.6 ms. Works because the mmap call discards the old mapping's swap entries, so the next fault gets fresh zero-page THPs instead of having to read back from swap at 4 KiB. Preserves the region's virtual address (no change visible to the allocator's slicing).
  2. **`MADV_WILLNEED` + touch + `MADV_COLLAPSE`** — middle ground. 2 MiB: ~2 ms; 128 MiB: ~122 ms. Preserves the user's data (swap-ins the original bytes), then coalesces. Use if dropping data is unacceptable.
  3. **touch + `MADV_COLLAPSE`** without `WILLNEED` — same correctness but ~3–5× slower than WILLNEED variant because swap-in is unoptimized.
* **Handle-level tracking is the cleanest fix.** `Handle::pageout` sets a "tainted" bit; `Handle::drop` reads that bit and, for tainted regions, does `mmap(addr, len, ..., MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS)` + `madvise(MADV_HUGEPAGE)` before pushing the region to the pool. Untainted drops stay on the zero-overhead recycle path. Cost for tainted drops (measured in experiment 9): one mmap + one madvise syscall ≈ a few µs for the syscalls, plus the next caller pays a normal fresh THP fault (~47 µs per PMD). Amortized, the tainted path matches the performance of a never-pageout'd region — the user just pays for exactly what they asked the kernel to do.
* **Swap round-trip is a separate axis.** Experiment 6 confirms `CONFIG_THP_SWAP` preserves THP on the swap-out write (`thp_swpout` increments by PMD count, `thp_swpout_fallback = 0`) but the swap-in path returns 4 KiB pages regardless. Experiment 4 confirms real cgroup-forced reclaim of COLD/PAGEOUT pages also ends at 4 KiB after refault. In other words: any path that takes a THP through swap (either direction) ends with a split.

### Guidance for callers (until a mitigation lands)

Until hugalloc ships the handle-level mitigation, callers who use `Handle::pageout` should be aware that:

* The region's PMDs are split by the advisory, permanently for that region.
* If the handle is dropped, the pool recycles a 4 KiB-backed region to the next caller.
* `eager_return=true` does not repair this (confirmed in experiment 5).

If the caller needs THP backing to persist across the pageout, the options in decreasing order of cost are:

* **Don't drop — recover in place.** After re-using the region, call `madvise(MADV_WILLNEED)` + touch + `madvise(MADV_COLLAPSE)` on the raw pointer to re-coalesce. Cost ~2 ms for a 2 MiB region evicted to swap.
* **Discard + fresh allocation.** `drop(handle); hugalloc::allocate(...)` — still hands back a tainted region from the pool. To truly discard, the user has to allocate *more* than the pool holds, which defeats the point.

Experiments to inform the mitigation design (not in this set):

1. Hit rate of the "tainted handle" state in realistic Materialize workloads — is PAGEOUT common enough to justify the bit and the branch in drop?
2. End-to-end effect of adding `MAP_FIXED` remap on tainted drops through the real pool, measured against experiment 5's baseline.
3. Whether to surface a `Handle::reclaim_for_reuse()` shim that the caller can invoke after re-populating, exposing COLLAPSE without needing the allocator to know about pageout state.

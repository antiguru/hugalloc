# Issue #1 — Pool recycling of pageout'd regions

Status: investigation + prototype mitigation shipped on branch
`issue-1-tainted-handle-remap`. Main branch carries bench harness + docs +
`Handle::restore_thp` helper (additive).

Upstream: <https://github.com/antiguru/hugalloc/issues/1>

This document is the checkpoint so the next person (or a future session)
can pick up where we left off without re-running everything. All numbers
below were collected on **r6gd.4xlarge** (aarch64, 16 vCPU, 128 GiB RAM,
944 GiB NVMe swap) running **kernel 6.17.0-1010-aws**, THP policy
`madvise`, `CONFIG_THP_SWAP=y`. The bench harness is
`benches/pageout_bench.rs`.

## 1. The original problem

A caller `allocate`s a region, writes to it, calls `Handle::pageout`
(`MADV_PAGEOUT`), then drops the handle. The pool recycles the region.
The next caller writes to it and pays swap-in cost they never asked for.
The pool's implicit "cheap to re-hand-out" contract is silently violated.

The issue proposed four mitigations:

1. Unconditional `MADV_DONTNEED` on every dealloc.
2. `MADV_DONTNEED` only on promotion to the global injector.
3. `mincore`-gated `DONTNEED` (only if significantly non-resident).
4. Documentation warning, no runtime change.

Plus measurements to validate the cost envelope.

## 2. Headline findings

All four proposals are unsound as a THP-preserving fix.

### 2.1 `MADV_PAGEOUT` splits the PMD permanently

Once `MADV_PAGEOUT` splits a 2 MiB PMD into a 4 KiB PTE subtable, the
subtable persists even after `MADV_DONTNEED` zaps the PTEs. Re-fault uses
4 KiB granularity. Verified end-to-end:

```
2 MiB    DONTNEED alone (no prior PAGEOUT)                44.9 µs   thp=100%
2 MiB    PAGEOUT, DONTNEED                               462.0 µs   thp=0%
2 MiB    PAGEOUT, DONTNEED, touch, MADV_COLLAPSE         469.4 µs   thp=100%
```

Kernel reclaim driven by memory pressure takes the same path: if the
kernel reclaims a THP'd region under pressure, the swap-in refault
returns 4 KiB pages and `AnonHugePages` stays 0. The split is not
reversed automatically. Only `MADV_COLLAPSE` (Linux ≥6.1) or a fresh
mapping (`munmap` / `MAP_FIXED`) restores PMD-order backing, and both
require the region to be populated at 4 KiB first.

### 2.2 Consequence for the four original proposals

| # | Proposal | Verdict |
|---|---|---|
| 1 | Unconditional `DONTNEED` on every dealloc | does NOT fix THP loss; cleans residency but split persists |
| 2 | `DONTNEED` on promotion (`eager_return=true`, already in code) | same defect — confirmed via `--pool` |
| 3 | `mincore`-gated `DONTNEED` | gate never fires on idle host (residency stays 100% after PAGEOUT until actual pressure) |
| 4 | Doc-only | valid fallback but leaves the bug |

### 2.3 What does work

| primitive | cost (2 MiB) | cost (128 MiB) | preserves data | notes |
|---|---|---|---|---|
| `MADV_COLLAPSE` after re-populate | ~2 ms swap-in + ~1 ms collapse | ~122 ms total | yes | Linux ≥6.1, range must be populated |
| `MAP_FIXED` remap + `MADV_HUGEPAGE` | ~107 µs (swap evicted) | ~5.64 ms | **no** | discards swap entries, no swap-in |
| `munmap` + fresh `mmap` | similar to MAP_FIXED | similar | no | loses VA reuse |

`MAP_FIXED` remap is ~20× faster than `WILLNEED + touch + COLLAPSE` for
truly-evicted regions at 128 MiB, because it skips the swap read entirely.
This is the core realisation that drove the mitigation.

### 2.4 Swap concurrency is serialized

Threaded swap behaviour surfaced only under the `--swap-concurrency`
experiment but changes the design calculus.

`MADV_PAGEOUT` (swap-out) aggregate throughput is **flat** at
~1.8 GB/s regardless of thread count. Single `swap_info` lock held
across reservation + I/O submission. 16 threads doing concurrent
PAGEOUT run **53× slower per-thread** than 1 thread.

Swap-in scales better but still bottlenecked: ~380 MB/s single-thread
touch-only, ~2.2 GB/s aggregate at 16 threads. `MADV_WILLNEED`
improves that to ~3.3 GB/s aggregate, but per-thread still drops
~7× from 1 to 16 threads.

Meaning: any mitigation that **touches pages back from swap** inherits
the bottleneck. `MAP_FIXED` remap sidesteps it — it zaps page tables,
never reads swap. Under multi-threaded pressure, this is the kill
switch, not merely the faster option.

### 2.5 TiB-heap observability constraint

`/proc/self/smaps` and `/proc/self/numa_maps` walk the whole VMA tree
under `mmap_sem`. On TiB heaps this stalls the process for minutes.
`/proc/self/status` (RssAnon) same cost. These are **not usable** from
any fast path.

`mincore` locks only the target range. Measured cost 690 ns for a
single-page probe, 46 µs for a full-range probe over 1 GiB. Usable.

## 3. Experiment catalogue

All in `benches/pageout_bench.rs`. Each subcommand writes a table to
stdout. Detailed tables also reproduced in `BENCH.md` under
"Pageout / recycle investigation".

| # | flag | question answered |
|---|---|---|
| 1 | `--baseline` | THP fault cost on fresh `mmap + MADV_HUGEPAGE` |
| 2+3 | `--advise` | Per-advisory cost + retouch cost + residency + THP state |
| 4 | `--pressure` | Same as #2 under cgroup reclaim |
| 5 | `--pool` | Bug reproduction + `eager_return=true` check through real `hugalloc` API |
| 6 | `--swap-probe` | Does `CONFIG_THP_SWAP` preserve PMDs on swap-out? |
| 7 | `--split-recovery` | Can a split PMD be re-coalesced? |
| 8 | `--collapse-probe` | What does `MADV_COLLAPSE` do on paged-data? |
| 9 | `--recovery` | End-to-end cost of candidate recovery paths |
| 10 | `--syscall-cost` | Per-drop mincore and MAP_FIXED remap costs |
| 11 | `--mmap-vs-pool` | How much does the pool actually save |
| 12 | `--swap-concurrency` | Per-thread and aggregate swap-in / swap-out scaling |

### 3.1 Key numbers

**Baseline (exp 1)** — fresh populate cost. ~47 µs per 2 MiB PMD, flat
from 2 MiB to 1 GiB. 100% THP, low noise (`p99` within ~10% of median).

**Advisory round-trip (exp 2+3)**:

| advice | 2 MiB retouch | thp after retouch | notes |
|---|---|---|---|
| COLD | 2 µs | 100% | no-op without pressure |
| DONTNEED | 45 µs | 100% | rebuilds THP cleanly (no prior PAGEOUT) |
| FREE | 2 µs | 100% | no-op without pressure |
| PAGEOUT | 16 µs | **0%** | splits PMD, pages stay mapped on idle host |

**Pressure round-trip (exp 4)** — 512 MiB cgroup, pages truly evicted:

| advice | 2 MiB retouch | thp after retouch |
|---|---|---|
| COLD | 6.97 ms | 0% (swap-in 4 KiB) |
| DONTNEED | 77 µs | 100% (no swap, refault is fresh) |
| FREE | 77 µs | 100% (kernel discards dirty without swap writeback) |
| PAGEOUT | 7.33 ms | 0% |

So any path that goes through swap returns 4 KiB.

**THP swap-out structure (exp 6)** — whole PMDs written:

| size | Δ`thp_swpout` | Δ`thp_swpout_fallback` |
|---|---|---|
| 2 MiB | 1 | 0 |
| 128 MiB | 64 | 0 |
| 1 GiB | 512 | 0 |

`CONFIG_THP_SWAP` writes PMD-granular, but the kernel tears down the
PMD entry as part of the write. `AnonHugePages` drops to 0 immediately.
Swap-in is always 4 KiB. So round-tripping through swap loses THP
regardless of direction.

**Recovery cost (exp 9)** — 512 MiB cgroup, pages truly in swap,
128 MiB probe:

| path | cost |
|---|---|
| touch only (bug baseline) | 421 ms |
| `WILLNEED + touch` | 86 ms |
| `touch + COLLAPSE` | 459 ms |
| `WILLNEED + touch + COLLAPSE` | 122 ms |
| `POPULATE_READ + COLLAPSE` | 475 ms |
| `WILLNEED + POPULATE_READ + COLLAPSE` | 116 ms |
| **`MAP_FIXED` remap + touch** | **5.64 ms** |

MAP_FIXED is ~20× faster than the best data-preserving path (WILLNEED +
POPULATE_READ + COLLAPSE).

**Syscall cost (exp 10)**:

| size | mincore (full) | mincore (1 page) | MAP_FIXED remap |
|---|---|---|---|
| 2 MiB | 706 ns | 690 ns | 8.9 µs |
| 128 MiB | 6.0 µs | 690 ns | 359 µs |
| 1 GiB | 46 µs | 690 ns | 2.86 ms |

**Pool value (exp 11)**:

| size | fresh mmap path | pool path | pool speedup |
|---|---|---|---|
| 2 MiB | 57 µs | 2.2 µs | 26× |
| 128 MiB | 3.28 ms | 233 µs | 14× |
| 1 GiB | 27.5 ms | 1.80 ms | 15× |

**Swap concurrency (exp 12, 1 GiB cgroup)**:

Swap-out (MADV_PAGEOUT):

| threads | per-thread PAGEOUT (2 MiB) | agg MB/s |
|---|---|---|
| 1 | 331 µs | 5 604 |
| 2 | 1.65 ms | 2 364 |
| 16 | 14.97 ms | 1 740 (per-thread 108) |

Swap-in (touch after PAGEOUT + pressure):

| threads | touch only | WILLNEED 2M chunks |
|---|---|---|
| 1 | 384 MB/s | 1 359 MB/s |
| 16 | 2 230 MB/s (140 per-thread) | 3 540 MB/s (221 per-thread) |

Prefetch chunk granularity barely matters for ≤ 16 MiB handles.
Granularity > 16 MiB not measured; worth testing at 128 MiB+ before
deciding chunk size in `restore_thp`.

## 4. Environmental gotcha

The Claude Code CLI wrapper (and some other harness processes) set
`prctl(PR_SET_THP_DISABLE, 1)` on their children. This silently disables
anon THP for the subtree even when `/sys/kernel/mm/transparent_hugepage/enabled`
is `always` or `madvise`. `thp_fault_alloc` stays at 0.

```sh
grep THP_enabled /proc/self/status   # 0 means THP is off for this process
```

The bench calls `prctl(PR_SET_THP_DISABLE, 0)` at startup and prints the
transition. Anyone running the harness under a wrapper script needs to
be aware.

## 5. Work shipped

### 5.1 On `main` (additive, safe)

- `benches/pageout_bench.rs` (harness, 12 experiments).
- `BENCH.md` "Pageout / recycle investigation" section.
- `Handle::restore_thp` — caller-facing helper that issues `MADV_WILLNEED + MADV_POPULATE_READ + MADV_COLLAPSE` over a byte range. Pairs with `Handle::pageout`. Element-range wrappers on `RawBuffer::restore_thp` / `Buffer::restore_thp`. Tests in `tests/advise.rs`, `tests/advise_typed.rs`. No behavioural changes to the allocator itself.
- `Handle::pageout` docs now reference `restore_thp` and explain the pool interaction.

### 5.2 On branch `issue-1-tainted-handle-remap`

- Handle gets a `tainted: AtomicBool`. `Handle::pageout` sets it; `Handle::restore_thp` clears it.
- `Handle::drop`, on a tainted handle, runs a new `remap_in_place(ptr, len)` helper that does `mmap(addr, len, ..., MAP_FIXED|MAP_PRIVATE|MAP_ANONYMOUS)` + `madvise(MADV_HUGEPAGE)` before the handle enters the pool.
- `Handle::into_raw_parts` / `from_raw_parts` explicitly don't preserve the tainted bit — documented.

Verified via `--pool` that all 2/16/128 MiB `recycle after pageout`
scenarios now end at `thp%=100` post-fix, retouch cost ≈ baseline THP
fault. Clippy + all-tests-single-threaded green.

### 5.3 What branch does NOT do yet

- **Does not detect regions reclaimed by kernel pressure without a prior `Handle::pageout` call.** The tainted bit is only set by the user's explicit advisory. A region that was THP-backed, got hit by `khugepaged` reclaim under memory pressure, came back 4 KiB via swap-in, and is then dropped — pool recycles it as-is. This is the gap the user flagged near end of session.
- No mincore-on-drop gate. Prototyped on paper but not coded.
- No background scrubber.
- No PR opened yet.

## 6. Open design: catching kernel-reclaim-driven degradation

Without a user `pageout` call, nothing sets the tainted bit today. Under
sustained memory pressure every pool region will gradually degrade to
4 KiB.

### 6.1 Signals

| signal | cost | catches | misses |
|---|---|---|---|
| mincore (full range) on drop | 690 ns (2 MiB) → 46 µs (1 GiB) | evicted and not-yet-refaulted regions | evicted-then-refaulted-while-still-in-pool |
| `/proc/vmstat` pswpin delta | single read, global | any recent swap activity (coarse) | — (global, not per-region) |
| `/proc/self/pagemap` range | ~µs per page | per-page resident + swap state | no THP bit |
| `/proc/self/smaps` | minutes on TiB heaps | everything | **not usable** |

### 6.2 Strategy options

**A — mincore-on-drop gate.**
`if tainted || any_page_nonresident → remap`. Fast. Catches the
"evicted but not yet refaulted" case. Mincore is bounded and lock-local,
so it works on TiB heaps. Misses the narrow
"evicted → refaulted at 4 KiB → dropped while resident" window. Cost
envelope: mincore adds 0.1–2% overhead to the normal drop path even
at 1 GiB handles.

**B — background THP-scrub worker.**
Periodically samples pool regions via mincore, or reads `thp_fault_alloc`
from vmstat, and remaps regions that look degraded. More moving parts,
but catches the case A misses.

**C — vmstat-gated global taint.**
Worker reads `pswpin` periodically. Delta > 0 for N seconds → set a
"recently under pressure" flag → pool taints every region on next drop
for that window. Coarse but cheap. Catches everything in pressure-bursts.

**D — always remap on drop.**
Simplest. Kills hot-recycle workloads (alloc→touch→drop→alloc in a loop
now pays fresh fault every cycle, ~47 µs/PMD extra). Probably
unacceptable for throughput-sensitive callers.

Recommended path: **A + C**. Tainted bit is fine-grained and catches
most cases cheaply; vmstat watchdog closes the mincore blind spot.
Skip B unless measurement shows A+C still misses real workloads.

### 6.3 Measurement we haven't done

- **Degradation rate.** Bench that loops `allocate → touch → drop`
  under cgroup pressure, samples `AnonHugePages` at the end (one smaps
  read outside the hot loop, acceptable). Plot THP% vs iteration
  count. Informs whether degradation is gradual (seconds) or
  catastrophic (ms), and how fast a mitigation needs to react.
- **mincore-gate under multi-threaded drop.** 16 threads dropping
  concurrently, does mincore become a contention point? Expectation:
  no, each mincore call locks its own range, and ranges don't overlap.
  Should confirm.
- **`Handle::restore_thp` at 128 MiB / 1 GiB.** Measured at 16 MiB,
  but chunking may matter at larger sizes. WILLNEED on a 1 GiB range
  may overwhelm the kernel's readahead queue and we should test
  whether chunking (2 MiB or 16 MiB) beats a single WILLNEED.

## 7. Follow-up work list

Rough priority order.

**Ship the mitigation:**
1. Add mincore-on-drop gate to the branch. Reuse `remap_in_place`.
   Benchmark the overhead on `--pool` to confirm the 0.1–2% envelope.
2. Open a PR with a summary of the headline findings and the two-part
   fix (tainted bit + mincore gate).
3. Decide on the vmstat watchdog — I lean yes, but the mincore gate
   may cover enough cases that it becomes optional.

**Close the data gaps:**
4. Degradation-rate bench under pressure (§6.3).
5. Multi-threaded drop benchmark.
6. `restore_thp` chunking at 128 MiB / 1 GiB.

**Polish:**
7. Consider chunking inside `Handle::restore_thp` for big handles. If
   chunk size matters, add it.
8. Pre-existing race in `tests/buffer.rs`: `buffer_push_extend_clear`
   / `buffer_assume_init_roundtrip` call `initialize()` without
   `GLOBAL_STATE_LOCK`. Not introduced by this work but blocks a clean
   `cargo test --all-targets`.
9. CHANGELOG entry once the mitigation lands.

**Nice to have:**
10. Write a short kernel FAQ on `CONFIG_THP_SWAP` asymmetry — swap-out
    is PMD-granular, swap-in never is. This is load-bearing for the
    whole investigation and is not widely documented.
11. Upstream a `MADV_COLLAPSE`-that-populates-for-you to the kernel?
    That would obsolete part of `restore_thp`. Out of scope for us
    but worth noting.

## 8. How to reproduce

```sh
# Clone + build
cargo build --release --bench pageout_bench
BENCH=$(ls -t target/release/deps/pageout_bench-* | grep -v '\.d$' | head -1)

# Verify PR_THP_DISABLE:
grep THP_enabled /proc/self/status   # 0 = THP disabled, harness will clear

# Idle-safe experiments:
"$BENCH" --baseline --advise --pool --swap-probe --split-recovery --collapse-probe \
          --recovery --syscall-cost --mmap-vs-pool

# Cgroup-required experiments (need systemd --user):
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G --wait --pipe "$BENCH" --pressure
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G --wait --pipe "$BENCH" --collapse-probe
systemd-run --user -p MemoryMax=512M -p MemorySwapMax=4G --wait --pipe "$BENCH" --recovery
systemd-run --user -p MemoryMax=1G   -p MemorySwapMax=8G --wait --pipe "$BENCH" --swap-concurrency
```

Branch `issue-1-tainted-handle-remap` carries the Handle mitigation.
Check it out and re-run `--pool` to see the before/after.

## 9. Commit log

### Main (2026-04-21):
```
6326134 Handle: restore_thp after pageout via WILLNEED + POPULATE_READ + COLLAPSE
b21eba9 docs: experiment 9 findings — MAP_FIXED remap is the cheapest recovery
60f2b70 bench: add --recovery measuring MAP_FIXED / WILLNEED / COLLAPSE paths
9c5a338 docs: add BENCH.md findings for pageout/recycle investigation
80078e9 bench: add pageout-recycle investigation harness for #1
```

### Branch `issue-1-tainted-handle-remap` (diverges from main):
```
622875f bench: add --syscall-cost, --mmap-vs-pool, --swap-concurrency
55556ef Handle: MAP_FIXED-remap tainted regions before recycling
b9c6f8b bench: add POPULATE_READ paths to --recovery
```

Pushed to `origin/issue-1-tainted-handle-remap`. No PR yet.

//! Investigation harness for <https://github.com/antiguru/hugalloc/issues/1>.
//!
//! Characterizes how Linux advisories, THP, and reclaim interact with anonymous
//! memory the way hugalloc uses it, so we can decide what the allocator's
//! default behavior should be when a user pages out a region before
//! deallocating.
//!
//! The experiments are built incrementally: each subcommand measures one
//! well-bounded question and writes a findings section to stdout. Results feed
//! into `BENCH.md` and direct the next experiment.
//!
//! # Subcommands
//!
//! * `--baseline` — fresh `mmap` + `MADV_HUGEPAGE` + first-touch. First-page
//!   fault latency, full-region populate cost, and post-touch residency / THP
//!   state.
//! * `--advise` — for each of `MADV_COLD` / `MADV_DONTNEED` / `MADV_FREE` /
//!   `MADV_PAGEOUT`, on a populated THP-backed region: syscall cost,
//!   residency + THP state immediately after the advisory, re-touch cost,
//!   THP state after re-touch.
//! * `--pressure` — the same advisories, but with a dirty pressure buffer
//!   allocated after the advisory to force the kernel to reclaim. Only
//!   meaningful under a cgroup memory limit (see `systemd-run` example below).
//! * `--pool` — exercises the real `hugalloc` API: allocate → touch → (maybe
//!   `pageout`) → drop → allocate → retouch. Compares default config, bug
//!   reproduction, and `eager_return=true` mitigation.
//! * `--swap-probe` — per-size: does `MADV_PAGEOUT` write THPs as whole units
//!   or as 4 KiB pages? Reads `thp_swpout` / `thp_swpout_fallback` deltas
//!   from `/proc/vmstat` and `Swap:` from `/proc/self/smaps` across the cycle.
//! * `--split-recovery` — once a PMD has been split by `MADV_PAGEOUT`, can it
//!   be coalesced back into a THP? Tests `MADV_DONTNEED`-then-retouch and
//!   `MADV_COLLAPSE` as recovery paths.
//! * `--collapse-probe` — what does `MADV_COLLAPSE` do on paged data? Does it
//!   page in? Does it reconstitute PMDs? Runs three scenarios: idle (pages
//!   still mapped), under pressure (pages evicted to swap — requires cgroup),
//!   and a PMD-split baseline.

use std::fs;
use std::time::{Duration, Instant};

/// Region sizes exercised. All powers of two and >= 2 MiB.
const SIZES: &[usize] = &[2 << 20, 16 << 20, 128 << 20, 1 << 30];

/// One huge page on aarch64 / x86_64 Linux.
const HPAGE: usize = 2 << 20;

/// Reps per size. Larger regions take proportionally longer, so we scale down
/// rep counts to keep total runtime bounded. Tuned for ~a few seconds per
/// experiment per size.
fn reps_for(size: usize) -> usize {
    match size {
        s if s <= 2 << 20 => 50,
        s if s <= 16 << 20 => 20,
        s if s <= 128 << 20 => 10,
        _ => 5,
    }
}

// ---------- Probe: hugepage-aligned anonymous mmap with introspection ----------

/// A single `MAP_PRIVATE|MAP_ANONYMOUS` region, hugepage-aligned, with
/// `MADV_HUGEPAGE` applied. Dropping `munmap`s.
struct Probe {
    ptr: *mut u8,
    len: usize,
    page_size: usize,
}

impl Probe {
    /// Allocate a fresh region. The returned region is untouched — no pages
    /// are resident yet.
    fn new(len: usize) -> Self {
        assert!(len.is_power_of_two() && len >= HPAGE);
        let ptr = mmap_hpage_aligned(len);
        // THP hint. Advisory; a no-op if THP is off or sized can't back.
        let ret = unsafe { libc::madvise(ptr.cast(), len, libc::MADV_HUGEPAGE) };
        assert_eq!(ret, 0, "MADV_HUGEPAGE: {}", std::io::Error::last_os_error());
        Self {
            ptr,
            len,
            page_size: page_size::get(),
        }
    }

    /// Parse `/proc/self/smaps`, find the VMA containing `self.ptr`, return
    /// `AnonHugePages` in bytes. 0 if the field is absent (= no THP backing).
    fn anon_huge_bytes(&self) -> usize {
        read_smaps_field(self.ptr as usize, "AnonHugePages:").unwrap_or(0)
    }

    /// Return the `Swap:` field (bytes in swap for this VMA) from
    /// `/proc/self/smaps`.
    fn swap_bytes(&self) -> usize {
        read_smaps_field(self.ptr as usize, "Swap:").unwrap_or(0)
    }

    /// Return `(resident_pages, total_pages)` via `mincore`.
    fn residency(&self) -> (usize, usize) {
        let total = self.len / self.page_size;
        let mut vec = vec![0u8; total];
        let ret = unsafe { libc::mincore(self.ptr.cast(), self.len, vec.as_mut_ptr()) };
        assert_eq!(ret, 0, "mincore: {}", std::io::Error::last_os_error());
        let resident = vec.iter().filter(|b| (*b & 1) != 0).count();
        (resident, total)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// mmap of `size` bytes, hugepage-aligned, by over-allocating and trimming
/// the head/tail slack. Ends up as a single VMA so `smaps` lookup is clean.
fn mmap_hpage_aligned(size: usize) -> *mut u8 {
    let total = size + HPAGE;
    let raw = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            total,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(
        raw,
        libc::MAP_FAILED,
        "mmap: {}",
        std::io::Error::last_os_error()
    );
    let raw_addr = raw as usize;
    let aligned = (raw_addr + HPAGE - 1) & !(HPAGE - 1);
    let before = aligned - raw_addr;
    let after = total - before - size;
    if before > 0 {
        unsafe { libc::munmap(raw, before) };
    }
    if after > 0 {
        unsafe { libc::munmap((aligned + size) as *mut libc::c_void, after) };
    }
    aligned as *mut u8
}

/// Scan `/proc/self/smaps` for the VMA that contains `addr`; return the
/// `field:` value (in bytes, assuming the smaps unit is kB).
///
/// Returns `None` if the address isn't in any known VMA. Returns `Some(0)` if
/// the field is absent in the matching VMA.
fn read_smaps_field(addr: usize, field: &str) -> Option<usize> {
    let content = fs::read_to_string("/proc/self/smaps").ok()?;
    let mut in_vma = false;
    for line in content.lines() {
        // VMA header: "7f....-7f.... rw-p ..."
        if let Some((range, _)) = line.split_once(' ')
            && let Some((start_hex, end_hex)) = range.split_once('-')
            && let (Ok(start), Ok(end)) = (
                usize::from_str_radix(start_hex, 16),
                usize::from_str_radix(end_hex, 16),
            )
        {
            if in_vma {
                // Passed the matching VMA without seeing the field.
                return Some(0);
            }
            in_vma = addr >= start && addr < end;
            continue;
        }
        if in_vma && let Some(rest) = line.strip_prefix(field) {
            let kb: usize = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb * 1024);
        }
    }
    if in_vma { Some(0) } else { None }
}

/// Read `(thp_swpout, thp_swpout_fallback, thp_fault_alloc,
/// thp_fault_fallback)` from `/proc/vmstat`.
fn read_vmstat_thp() -> (u64, u64, u64, u64) {
    let content = fs::read_to_string("/proc/vmstat").unwrap_or_default();
    let mut out = (0u64, 0u64, 0u64, 0u64);
    for line in content.lines() {
        let mut it = line.split_whitespace();
        let Some(name) = it.next() else { continue };
        let Some(val) = it.next().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        match name {
            "thp_swpout" => out.0 = val,
            "thp_swpout_fallback" => out.1 = val,
            "thp_fault_alloc" => out.2 = val,
            "thp_fault_fallback" => out.3 = val,
            _ => {}
        }
    }
    out
}

// ---------- stats helpers ----------

fn stats(samples: &mut [u64]) -> Stats {
    samples.sort_unstable();
    let n = samples.len();
    Stats {
        min: samples[0],
        median: samples[n / 2],
        p99: samples[((n as f64 * 0.99) as usize).min(n - 1)],
        mean: samples.iter().sum::<u64>() / n as u64,
    }
}

struct Stats {
    min: u64,
    median: u64,
    p99: u64,
    mean: u64,
}

fn format_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    }
}

fn format_size(bytes: usize) -> String {
    if bytes >= 1 << 30 {
        format!("{} GiB", bytes >> 30)
    } else {
        format!("{} MiB", bytes >> 20)
    }
}

/// `prctl` option numbers. Not in libc's public re-exports on every target.
const PR_GET_THP_DISABLE: libc::c_int = 42;
const PR_SET_THP_DISABLE: libc::c_int = 41;

/// Clear `PR_SET_THP_DISABLE` on this process so `MADV_HUGEPAGE` can take
/// effect. Some parent processes (notably the Claude Code harness) set this
/// flag, which silently disables anon THP for the whole process tree.
///
/// Returns the prior value so the caller can report it.
fn clear_thp_disable() -> i32 {
    // SAFETY: prctl with PR_GET/SET_THP_DISABLE takes zero extra args.
    let before = unsafe { libc::prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0) };
    let _ = unsafe { libc::prctl(PR_SET_THP_DISABLE, 0, 0, 0, 0) };
    before
}

fn print_env() {
    println!("# environment");
    println!("  arch:     {}", std::env::consts::ARCH);
    if let Ok(thp) = fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled") {
        println!("  THP:      {}", thp.trim());
    }
    let before = clear_thp_disable();
    let after = unsafe { libc::prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0) };
    println!("  PR_THP_DISABLE: {before} → {after} (cleared at startup if set)");
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if line.starts_with("MemTotal") || line.starts_with("SwapTotal") {
                println!("  {line}");
            }
        }
    }
    println!("  page size: {} bytes", page_size::get());
}

// ---------- Experiment 1: baseline characterization ----------

/// Baseline: what does a fresh `mmap` + `MADV_HUGEPAGE` + first-touch actually
/// cost, and what does the kernel give us?
///
/// For each size, many reps. Per rep we:
///   1. mmap hugepage-aligned, MADV_HUGEPAGE
///   2. Write one byte at offset 0, time just that store
///   3. Write one byte per 4 KiB slot across the rest of the region, time the
///      combined total
///   4. Read AnonHugePages from smaps
///   5. Read residency from mincore
///
/// The first/rest split tells us whether the initial fault backed the region
/// with a huge page (first-touch is ~huge-page-wide, rest is cheap) or with a
/// 4 KiB page (first-touch is one small fault, rest is many more small faults).
fn run_baseline() {
    println!("\n=== experiment 1: baseline mmap + MADV_HUGEPAGE + first-touch ===\n");
    println!(
        "{:>8}  {:>9}  {:>9}  {:>10}  {:>10}  {:>10}  {:>7}  {:>7}",
        "size", "first_mn", "first_med", "full_med", "full_p99", "full_mean", "thp%", "resid%"
    );
    println!("{}", "-".repeat(92));

    for &size in SIZES {
        let reps = reps_for(size);
        let mut first = Vec::with_capacity(reps);
        let mut full = Vec::with_capacity(reps);
        let mut thp = Vec::with_capacity(reps);
        let mut resid = Vec::with_capacity(reps);

        for _ in 0..reps {
            let p = Probe::new(size);
            let start = Instant::now();
            // SAFETY: p.ptr is a live mmap of size bytes; writing one byte is
            // in-bounds and uninitialized memory is OK to overwrite.
            unsafe { std::ptr::write_volatile(p.ptr, 1) };
            let t_first = start.elapsed();
            // Write the rest of the region, one byte per system page.
            let slice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
            for i in (p.page_size..slice.len()).step_by(p.page_size) {
                unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
            }
            let t_full = start.elapsed();

            first.push(t_first.as_nanos() as u64);
            full.push(t_full.as_nanos() as u64);
            thp.push(p.anon_huge_bytes());
            let (r, t) = p.residency();
            resid.push((r as f64 / t as f64) * 100.0);
        }

        let sf = stats(&mut first);
        let sa = stats(&mut full);
        let thp_avg = thp.iter().sum::<usize>() / thp.len();
        let resid_avg = resid.iter().sum::<f64>() / resid.len() as f64;

        println!(
            "{:>8}  {:>9}  {:>9}  {:>10}  {:>10}  {:>10}  {:>6.1}%  {:>6.1}%",
            format_size(size),
            format_ns(sf.min),
            format_ns(sf.median),
            format_ns(sa.median),
            format_ns(sa.p99),
            format_ns(sa.mean),
            (thp_avg as f64 / size as f64) * 100.0,
            resid_avg,
        );
    }

    println!("\nLegend:");
    println!("  first_mn   min time to write offset 0 after mmap (pure first-fault)");
    println!("  first_med  median of same");
    println!("  full_*     time to write every {:>4}-B slot (first fault + rest)", page_size::get());
    println!("  thp%       AnonHugePages / region size (after full touch)");
    println!("  resid%     resident pages / total pages, via mincore (after full touch)");
    println!("\nInterpretation:");
    println!("  If the kernel faulted in a huge page on first touch:");
    println!("    first_* is large (~proportional to 2 MiB zeroing)");
    println!("    full_med - first_med is small (remaining pages already resident)");
    println!("    thp% approaches 100");
    println!("  If the kernel used 4 KiB pages on first touch:");
    println!("    first_* is small (~single-page fault)");
    println!("    full_med is large (many individual faults)");
    println!("    thp% may still rise later via khugepaged");
}

// ---------- Experiment 2+3: advisory effect + re-touch ----------

/// For each size × each advisory, on a populated THP-backed region:
///
///   mmap → populate → advise → observe (residency, THP) → re-touch → observe
///
/// Tells us the cost of each advisory, what state it leaves the region in,
/// and whether re-touching rebuilds THP or degrades to 4 KiB.
fn run_advise_matrix() {
    println!("\n=== experiment 2+3: advisory effect + re-touch ===\n");
    println!(
        "{:>8}  {:<14}  {:>9}  {:>10}  {:>10}  {:>8}  {:>8}  {:>8}",
        "size", "advice", "adv_med", "retouch_med", "retouch_p99", "resA%", "thpA%", "thpB%"
    );
    println!("{}", "-".repeat(92));

    let advisories: &[(&str, libc::c_int)] = &[
        ("MADV_COLD", libc::MADV_COLD),
        ("MADV_DONTNEED", libc::MADV_DONTNEED),
        ("MADV_FREE", libc::MADV_FREE),
        ("MADV_PAGEOUT", libc::MADV_PAGEOUT),
    ];

    for &size in SIZES {
        let reps = reps_for(size);
        for &(label, adv) in advisories {
            let mut adv_times = Vec::with_capacity(reps);
            let mut rt_times = Vec::with_capacity(reps);
            let mut res_after = Vec::with_capacity(reps);
            let mut thp_after_adv = Vec::with_capacity(reps);
            let mut thp_after_rt = Vec::with_capacity(reps);

            for _ in 0..reps {
                let p = Probe::new(size);
                // Populate.
                let slice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
                for i in (0..slice.len()).step_by(p.page_size) {
                    unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
                }
                // Advisory.
                let t = Instant::now();
                // SAFETY: advisory hint on our own VMA, length matches mapping.
                let r = unsafe { libc::madvise(p.ptr.cast(), p.len, adv) };
                let dt = t.elapsed();
                assert_eq!(
                    r,
                    0,
                    "madvise({label}): {}",
                    std::io::Error::last_os_error()
                );
                adv_times.push(dt.as_nanos() as u64);
                // State after advisory.
                let (resident, total) = p.residency();
                res_after.push((resident as f64 / total as f64) * 100.0);
                thp_after_adv.push(p.anon_huge_bytes());
                // Re-touch.
                let start = Instant::now();
                for i in (0..slice.len()).step_by(p.page_size) {
                    unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
                }
                rt_times.push(start.elapsed().as_nanos() as u64);
                thp_after_rt.push(p.anon_huge_bytes());
            }

            let sa = stats(&mut adv_times);
            let sr = stats(&mut rt_times);
            let res_avg = res_after.iter().sum::<f64>() / res_after.len() as f64;
            let thpa = thp_after_adv.iter().sum::<usize>() / thp_after_adv.len();
            let thpb = thp_after_rt.iter().sum::<usize>() / thp_after_rt.len();

            println!(
                "{:>8}  {:<14}  {:>9}  {:>10}  {:>10}  {:>7.1}%  {:>7.1}%  {:>7.1}%",
                format_size(size),
                label,
                format_ns(sa.median),
                format_ns(sr.median),
                format_ns(sr.p99),
                res_avg,
                (thpa as f64 / size as f64) * 100.0,
                (thpb as f64 / size as f64) * 100.0,
            );
        }
        println!();
    }

    println!("Legend:");
    println!("  adv_med       median advisory syscall latency");
    println!("  retouch_med   median time to rewrite every {} B slot after advisory", page_size::get());
    println!("  retouch_p99   p99 of above");
    println!("  resA%         mincore residency, immediately after advisory");
    println!("  thpA%         AnonHugePages / region size, immediately after advisory");
    println!("  thpB%         AnonHugePages / region size, after re-touch");
}

/// Read the effective `memory.max` for the current cgroup v2 group. Returns
/// `None` if no limit is set (root cgroup or `max`).
fn read_cgroup_memory_max() -> Option<usize> {
    let raw = fs::read_to_string("/proc/self/cgroup").ok()?;
    // cgroup v2 lines look like "0::/user.slice/user-1000.slice/..."
    let line = raw.lines().next()?;
    let path = line.splitn(3, ':').nth(2)?;
    let candidate = format!("/sys/fs/cgroup{}/memory.max", path);
    let contents = fs::read_to_string(&candidate).ok()?;
    let s = contents.trim();
    if s == "max" {
        return None;
    }
    s.parse().ok()
}

// ---------- Experiment 4: advisory + memory pressure ----------

/// For each advisory on a populated probe, allocate and dirty a pressure
/// buffer afterwards, then observe whether the probe's pages actually got
/// reclaimed / paged out. Distinguishes "kernel marked these for eviction but
/// didn't actually evict" from "kernel reclaimed".
///
/// Only produces meaningful results under a cgroup memory limit. On an
/// unbound host the pressure buffer fits alongside the probe and nothing
/// gets reclaimed.
fn run_pressure() {
    // Skip 1 GiB — a cgroup tight enough to force reclaim on 1 GiB probe +
    // 4× pressure buffer wouldn't leave headroom for the allocator itself.
    let sizes: &[usize] = &[2 << 20, 16 << 20, 128 << 20];
    let advisories: &[(&str, libc::c_int)] = &[
        ("MADV_COLD", libc::MADV_COLD),
        ("MADV_DONTNEED", libc::MADV_DONTNEED),
        ("MADV_FREE", libc::MADV_FREE),
        ("MADV_PAGEOUT", libc::MADV_PAGEOUT),
    ];

    println!("\n=== experiment 4: advisory + memory pressure ===\n");
    let cgroup_limit = read_cgroup_memory_max();
    match cgroup_limit {
        Some(l) => println!("cgroup memory.max: {} MiB", l >> 20),
        None => println!(
            "WARNING: no cgroup memory limit detected. Reclaim unlikely; \
             run under `systemd-run --user -p MemoryMax=<N>M -p MemorySwapMax=<M>G`."
        ),
    }
    println!();
    println!(
        "{:>8}  {:<14}  {:>7}  {:>8}  {:>7}  {:>11}  {:>11}  {:>7}",
        "size", "advice", "resA%", "resAp%", "thpA%", "retouch_med", "retouch_p99", "thpB%"
    );
    println!("{}", "-".repeat(92));

    for &size in sizes {
        let reps = reps_for(size).min(5);
        for &(label, adv) in advisories {
            let mut res_a = Vec::with_capacity(reps);
            let mut res_ap = Vec::with_capacity(reps);
            let mut thp_a = Vec::with_capacity(reps);
            let mut retouch = Vec::with_capacity(reps);
            let mut thp_b = Vec::with_capacity(reps);

            for _ in 0..reps {
                let p = Probe::new(size);
                let slice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
                for i in (0..slice.len()).step_by(p.page_size) {
                    unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
                }
                // SAFETY: our own VMA, matching length.
                let r = unsafe { libc::madvise(p.ptr.cast(), p.len, adv) };
                assert_eq!(
                    r,
                    0,
                    "madvise({label}): {}",
                    std::io::Error::last_os_error()
                );

                let (resident, total) = p.residency();
                res_a.push((resident as f64 / total as f64) * 100.0);
                thp_a.push(p.anon_huge_bytes());

                // Pressure: fresh mmap, dirty every page. Under a cgroup
                // memory limit this forces the kernel to reclaim cold pages
                // (our probe, if the advisory marked it reclaimable).
                //
                // Sizing: if a cgroup limit is present, aim for probe +
                // pressure ≈ 1.5 × limit so the kernel has to reclaim
                // somebody. Floor at 4 × probe. Cap at 8 GiB for sanity.
                let pressure_target = cgroup_limit
                    .map(|l| (l.saturating_mul(3) / 2).saturating_sub(size))
                    .unwrap_or(0);
                let pressure_size = pressure_target
                    .max(size.saturating_mul(4))
                    .min(8usize << 30)
                    .next_power_of_two();
                let pr = Probe::new(pressure_size);
                let pslice = unsafe { std::slice::from_raw_parts_mut(pr.ptr, pr.len) };
                for i in (0..pslice.len()).step_by(pr.page_size) {
                    unsafe { std::ptr::write_volatile(&mut pslice[i], 1) };
                }

                let (resident, total) = p.residency();
                res_ap.push((resident as f64 / total as f64) * 100.0);

                let start = Instant::now();
                for i in (0..slice.len()).step_by(p.page_size) {
                    unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
                }
                retouch.push(start.elapsed().as_nanos() as u64);
                thp_b.push(p.anon_huge_bytes());

                drop(pr);
                drop(p);
            }

            let ra = res_a.iter().sum::<f64>() / res_a.len() as f64;
            let rap = res_ap.iter().sum::<f64>() / res_ap.len() as f64;
            let ta = thp_a.iter().sum::<usize>() / thp_a.len();
            let tb = thp_b.iter().sum::<usize>() / thp_b.len();
            let sr = stats(&mut retouch);

            println!(
                "{:>8}  {:<14}  {:>6.1}%  {:>7.1}%  {:>6.1}%  {:>11}  {:>11}  {:>6.1}%",
                format_size(size),
                label,
                ra,
                rap,
                (ta as f64 / size as f64) * 100.0,
                format_ns(sr.median),
                format_ns(sr.p99),
                (tb as f64 / size as f64) * 100.0,
            );
        }
        println!();
    }

    println!("Legend:");
    println!("  resA%         probe residency right after advisory (before pressure)");
    println!("  resAp%        probe residency after 4×size pressure buffer dirtied");
    println!("  thpA%         probe AnonHugePages right after advisory");
    println!("  retouch_*     time to rewrite probe after pressure");
    println!("  thpB%         probe AnonHugePages after retouch");
    println!();
    println!("Signal: if resAp% << resA%, the advisory made the probe reclaimable and");
    println!("pressure forced actual reclaim. If resAp% == resA%, pressure wasn't tight");
    println!("enough OR the advisory did not mark pages reclaimable.");
}

// ---------- Experiment 7: split recovery ----------

/// `MADV_COLLAPSE` was added in 6.1; libc may not export it on older targets.
const MADV_COLLAPSE_NUM: libc::c_int = 25;

/// Probe whether a PMD that was split by `MADV_PAGEOUT` can be re-coalesced
/// into a THP through various recovery paths:
///   (a) `MADV_DONTNEED` + retouch — relies on re-fault picking the PMD path
///   (b) `MADV_COLLAPSE` after retouch — explicit collapse
///   (c) munmap + fresh mmap — baseline, always works
fn run_split_recovery() {
    println!("\n=== experiment 7: can a split PMD be re-coalesced? ===\n");
    println!(
        "{:>8}  {:<34}  {:>11}  {:>11}  {:>7}",
        "size", "sequence", "final_touch", "p99", "thp%"
    );
    println!("{}", "-".repeat(78));

    for &size in SIZES {
        let reps = reps_for(size).max(5);

        // Baseline reference: MADV_DONTNEED only (no prior PAGEOUT).
        run_split_sequence(size, reps, "DONTNEED (no prior PAGEOUT)", |p| {
            unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_DONTNEED) };
        });

        // Path (a): PAGEOUT then DONTNEED then retouch.
        run_split_sequence(size, reps, "PAGEOUT, DONTNEED", |p| {
            unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_PAGEOUT) };
            unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_DONTNEED) };
        });

        // Path (b): PAGEOUT, DONTNEED, retouch, COLLAPSE (still expects 4 KiB
        // initial touch, then collapse attempts to merge).
        run_split_sequence_with_post(
            size,
            reps,
            "PAGEOUT, DONTNEED, touch, COLLAPSE",
            |p| {
                unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_PAGEOUT) };
                unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_DONTNEED) };
            },
            |p| {
                // After the retouch, try to collapse.
                let r = unsafe { libc::madvise(p.ptr.cast(), p.len, MADV_COLLAPSE_NUM) };
                if r != 0 {
                    let e = std::io::Error::last_os_error();
                    // Not fatal — emit once for visibility.
                    eprintln!("  (note: MADV_COLLAPSE returned {r}: {e})");
                }
            },
        );

        // Path (c): PAGEOUT, then munmap+remmap (trivially works; for sanity).
        // We can't munmap inside Probe, so skip in the structured runner.

        println!();
    }

    println!("Legend:");
    println!("  final_touch  median time to (re-)write the region at the end of the sequence");
    println!("  thp%         AnonHugePages / region size at the end of the sequence");
}

fn run_split_sequence<F: Fn(&Probe)>(size: usize, reps: usize, label: &str, prep: F) {
    let mut touches = Vec::with_capacity(reps);
    let mut thps = Vec::with_capacity(reps);
    for _ in 0..reps {
        let p = Probe::new(size);
        // Initial populate to get to THP state.
        let slice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
        for i in (0..slice.len()).step_by(p.page_size) {
            unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
        }
        prep(&p);
        // Measure final re-touch.
        let start = Instant::now();
        for i in (0..slice.len()).step_by(p.page_size) {
            unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
        }
        touches.push(start.elapsed().as_nanos() as u64);
        thps.push(p.anon_huge_bytes());
    }
    let s = stats(&mut touches);
    let thp_avg = thps.iter().sum::<usize>() / thps.len();
    println!(
        "{:>8}  {:<34}  {:>11}  {:>11}  {:>6.1}%",
        format_size(size),
        label,
        format_ns(s.median),
        format_ns(s.p99),
        (thp_avg as f64 / size as f64) * 100.0,
    );
}

fn run_split_sequence_with_post<F: Fn(&Probe), G: Fn(&Probe)>(
    size: usize,
    reps: usize,
    label: &str,
    prep: F,
    post: G,
) {
    let mut touches = Vec::with_capacity(reps);
    let mut thps = Vec::with_capacity(reps);
    for _ in 0..reps {
        let p = Probe::new(size);
        let slice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
        for i in (0..slice.len()).step_by(p.page_size) {
            unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
        }
        prep(&p);
        let start = Instant::now();
        for i in (0..slice.len()).step_by(p.page_size) {
            unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
        }
        touches.push(start.elapsed().as_nanos() as u64);
        post(&p);
        thps.push(p.anon_huge_bytes());
    }
    let s = stats(&mut touches);
    let thp_avg = thps.iter().sum::<usize>() / thps.len();
    println!(
        "{:>8}  {:<34}  {:>11}  {:>11}  {:>6.1}%",
        format_size(size),
        label,
        format_ns(s.median),
        format_ns(s.p99),
        (thp_avg as f64 / size as f64) * 100.0,
    );
}

// ---------- Experiment 8: MADV_COLLAPSE on paged data ----------

/// What happens when you call `MADV_COLLAPSE` on a region whose PMDs have
/// been split by `MADV_PAGEOUT`? Does it page data back in if pages are in
/// swap? Does it reconstitute the PMD as a fresh THP?
///
/// Scenarios per size:
///   A) populated THP → COLLAPSE (sanity: already THP, expect no-op / success)
///   B) populated → PAGEOUT (idle: 4 KiB resident, swap staged) → COLLAPSE
///   C) populated → PAGEOUT → cgroup pressure that forces real eviction →
///      drop pressure → COLLAPSE. Runs only when a cgroup memory limit is
///      configured.
fn run_collapse_probe() {
    println!("\n=== experiment 8: MADV_COLLAPSE on paged data ===\n");
    let cgroup_limit = read_cgroup_memory_max();
    match cgroup_limit {
        Some(l) => println!("cgroup memory.max: {} MiB — scenario C will run", l >> 20),
        None => println!("no cgroup limit — scenarios A and B only"),
    }
    println!();
    println!(
        "{:>8}  {:<38}  {:>5}  {:>9}  {:>7}  {:>7}  {:>7}",
        "size", "scenario", "ret", "collapse", "resBef", "resAft", "thpAft"
    );
    println!("{}", "-".repeat(96));

    for &size in SIZES {
        let reps = reps_for(size).clamp(5, 10);

        // A) populated THP → COLLAPSE.
        run_collapse_scenario(size, reps, "populated THP → COLLAPSE", |_p| {});

        // B) populated → PAGEOUT → COLLAPSE.
        run_collapse_scenario(size, reps, "PAGEOUT (idle) → COLLAPSE", |p| {
            unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_PAGEOUT) };
        });

        // B2) populated → PAGEOUT → retouch → COLLAPSE.
        // The retouch should convert swap-entry PTEs back into normal page
        // PTEs, which COLLAPSE can then coalesce.
        run_collapse_scenario(size, reps, "PAGEOUT → retouch → COLLAPSE", |p| {
            unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_PAGEOUT) };
            let slice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
            for i in (0..slice.len()).step_by(p.page_size) {
                unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
            }
        });

        // C) populated → PAGEOUT → pressure → drop pressure → COLLAPSE. Only
        // under a cgroup memory limit.
        if let Some(limit) = cgroup_limit {
            let pressure = (limit.saturating_mul(3) / 2)
                .saturating_sub(size)
                .max(size * 2)
                .next_power_of_two();
            run_collapse_scenario(size, reps.min(3), "PAGEOUT + pressure → COLLAPSE", |p| {
                unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_PAGEOUT) };
                let pr = Probe::new(pressure);
                let slice = unsafe { std::slice::from_raw_parts_mut(pr.ptr, pr.len) };
                for i in (0..slice.len()).step_by(pr.page_size) {
                    unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
                }
                drop(pr);
            });

            // C2) same as C, but retouch (force swap-in) before COLLAPSE.
            run_collapse_scenario(
                size,
                reps.min(3),
                "PAGEOUT + pressure + retouch → COLLAPSE",
                |p| {
                    unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_PAGEOUT) };
                    let pr = Probe::new(pressure);
                    let slice = unsafe { std::slice::from_raw_parts_mut(pr.ptr, pr.len) };
                    for i in (0..slice.len()).step_by(pr.page_size) {
                        unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
                    }
                    drop(pr);
                    // Retouch probe — forces swap-in at 4 KiB granularity.
                    let pslice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
                    for i in (0..pslice.len()).step_by(p.page_size) {
                        unsafe { std::ptr::write_volatile(&mut pslice[i], 1) };
                    }
                },
            );
        }

        println!();
    }

    println!("Legend:");
    println!("  ret          MADV_COLLAPSE return code (0=ok, -1=error)");
    println!("  collapse     median time spent in the COLLAPSE syscall");
    println!("  resBef       mincore residency just before COLLAPSE");
    println!("  resAft       mincore residency right after COLLAPSE");
    println!("  thpAft       AnonHugePages / region size right after COLLAPSE");
}

fn run_collapse_scenario(size: usize, reps: usize, label: &str, prep: impl Fn(&Probe)) {
    let mut ret = Vec::with_capacity(reps);
    let mut durations = Vec::with_capacity(reps);
    let mut res_before = Vec::with_capacity(reps);
    let mut res_after = Vec::with_capacity(reps);
    let mut thp_after = Vec::with_capacity(reps);
    let mut last_errno = None;

    for _ in 0..reps {
        let p = Probe::new(size);
        let slice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
        for i in (0..slice.len()).step_by(p.page_size) {
            unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
        }
        prep(&p);

        let (r, t) = p.residency();
        res_before.push((r as f64 / t as f64) * 100.0);

        let start = Instant::now();
        // SAFETY: MADV_COLLAPSE on our own VMA, matching length.
        let rc = unsafe { libc::madvise(p.ptr.cast(), p.len, MADV_COLLAPSE_NUM) };
        let dt = start.elapsed();
        if rc != 0 {
            last_errno = Some(std::io::Error::last_os_error());
        }
        ret.push(rc);
        durations.push(dt.as_nanos() as u64);

        let (r, t) = p.residency();
        res_after.push((r as f64 / t as f64) * 100.0);
        thp_after.push(p.anon_huge_bytes());
    }

    let s = stats(&mut durations);
    let rb = res_before.iter().sum::<f64>() / res_before.len() as f64;
    let ra = res_after.iter().sum::<f64>() / res_after.len() as f64;
    let ta = thp_after.iter().sum::<usize>() / thp_after.len();
    let majority_ret = *ret.first().unwrap_or(&-1);

    println!(
        "{:>8}  {:<38}  {:>5}  {:>9}  {:>6.1}%  {:>6.1}%  {:>6.1}%",
        format_size(size),
        label,
        majority_ret,
        format_ns(s.median),
        rb,
        ra,
        (ta as f64 / size as f64) * 100.0,
    );
    if majority_ret != 0
        && let Some(e) = last_errno
    {
        println!("          (errno: {e})");
    }
}

// ---------- Experiment 5: hugalloc pool recycle ----------

/// One cycle of the pool scenario: allocate → touch → (maybe pageout) → drop,
/// then allocate-and-time-retouch. Returns retouch duration and, optionally,
/// the post-retouch `AnonHugePages` for the VMA containing the probe.
fn pool_cycle(size: usize, do_pageout: bool, collect_thp: bool) -> (Duration, usize) {
    let ps = page_size::get();

    let (ptr, cap, h) = hugalloc::allocate::<u8>(size).unwrap();
    let slice = unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), cap) };
    for i in (0..slice.len()).step_by(ps) {
        unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
    }
    if do_pageout {
        h.pageout(0..cap).unwrap();
    }
    drop(h);

    let (ptr2, cap2, h2) = hugalloc::allocate::<u8>(size).unwrap();
    let slice2 = unsafe { std::slice::from_raw_parts_mut(ptr2.as_ptr(), cap2) };
    let start = Instant::now();
    for i in (0..slice2.len()).step_by(ps) {
        unsafe { std::ptr::write_volatile(&mut slice2[i], 1) };
    }
    let dt = start.elapsed();
    let thp = if collect_thp {
        read_smaps_field(ptr2.as_ptr() as usize, "AnonHugePages:").unwrap_or(0)
    } else {
        0
    };
    drop(h2);
    (dt, thp)
}

/// For each size, three scenarios:
///   1. baseline recycle (no pageout), default config
///   2. bug reproduction (pageout before drop), default config
///   3. mitigation: `eager_return=true` — promotion calls MADV_DONTNEED
///
/// Reports retouch latency and `AnonHugePages` per scenario.
fn run_pool() {
    // 1 GiB probe excluded: the INITIAL_SIZE / growth produces a big VMA and
    // the run becomes slow without new signal beyond what 128 MiB shows.
    let sizes: &[usize] = &[2 << 20, 16 << 20, 128 << 20];
    let reps = 30;
    let warmup = 5;

    println!("\n=== experiment 5: hugalloc pool recycle scenarios ===\n");
    println!(
        "{:>8}  {:<32}  {:>11}  {:>11}  {:>10}  {:>7}",
        "size", "scenario", "retouch_med", "retouch_p99", "thp_kb", "thp%"
    );
    println!("{}", "-".repeat(86));

    for &size in sizes {
        // Scenario 1: baseline recycle (no pageout).
        hugalloc::builder()
            .enable()
            .eager_return(false)
            .local_buffer_bytes(0)
            .apply()
            .expect("apply");
        let (rt, thp) = measure_pool(size, false, reps, warmup);
        print_pool_row(size, "recycle (no pageout)", &rt, thp);

        // Scenario 2: bug — pageout then drop, default config.
        let (rt, thp) = measure_pool(size, true, reps, warmup);
        print_pool_row(size, "recycle after pageout (bug)", &rt, thp);

        // Scenario 3: mitigation — eager_return=true forces DONTNEED on
        // every drop (since local_buffer_bytes=0 promotes on every push).
        hugalloc::builder().eager_return(true).apply().expect("apply");
        let (rt, thp) = measure_pool(size, true, reps, warmup);
        print_pool_row(size, "+ eager_return=true (mitig)", &rt, thp);

        println!();
    }

    println!("Legend:");
    println!("  retouch_med   median time to re-touch after drop + realloc");
    println!("  thp_kb        AnonHugePages of the VMA containing the probe, kB");
    println!("  thp%          thp_kb / probe size");
}

fn measure_pool(size: usize, do_pageout: bool, reps: usize, warmup: usize) -> (Stats, usize) {
    for _ in 0..warmup {
        let _ = pool_cycle(size, do_pageout, false);
    }
    let mut retouch = Vec::with_capacity(reps);
    let mut thp = Vec::with_capacity(reps);
    for _ in 0..reps {
        let (dt, t) = pool_cycle(size, do_pageout, true);
        retouch.push(dt.as_nanos() as u64);
        thp.push(t);
    }
    let s = stats(&mut retouch);
    let thp_avg = thp.iter().sum::<usize>() / thp.len();
    (s, thp_avg)
}

fn print_pool_row(size: usize, scenario: &str, rt: &Stats, thp_bytes: usize) {
    println!(
        "{:>8}  {:<32}  {:>11}  {:>11}  {:>10}  {:>6.1}%",
        format_size(size),
        scenario,
        format_ns(rt.median),
        format_ns(rt.p99),
        thp_bytes >> 10,
        (thp_bytes as f64 / size as f64) * 100.0,
    );
}

// ---------- Experiment 6: THP survival through swap ----------

/// Does a 2 MiB THP survive an `MADV_PAGEOUT` → refault cycle, or does it
/// always come back as 4 KiB pages?
///
/// Per size, single rep (these numbers are large and deterministic):
///   1. Populate probe.
///   2. Snapshot vmstat counters.
///   3. `MADV_PAGEOUT`.
///   4. Read probe's `Swap:` and `AnonHugePages:` from smaps.
///   5. Delta vmstat counters.
///   6. Retouch probe, time it. Snapshot counters again.
///   7. Read `Swap:` and `AnonHugePages:` after retouch.
///
/// Answers:
///   * Does `MADV_PAGEOUT` actually write anon pages to swap on this kernel?
///     (`Swap` in smaps should match region size)
///   * Are THPs swapped out as units or split first? (`thp_swpout` vs
///     `thp_swpout_fallback`)
///   * Does swap-in rebuild THP? (`thp_fault_alloc` delta and post-retouch
///     `AnonHugePages`)
fn run_swap_probe() {
    println!("\n=== experiment 6: THP survival through swap round-trip ===\n");

    if !has_swap() {
        println!("NOTE: no swap configured on this host. MADV_PAGEOUT on anon will be a no-op.");
    }

    println!(
        "{:>8}  {:>8}  {:>8}  {:>8}  {:>10}  {:>10}  {:>8}  {:>8}  {:>11}",
        "size",
        "thpA_KB",
        "swapA_KB",
        "resA%",
        "Δswpout",
        "Δswp_fb",
        "thpB_KB",
        "swapB_KB",
        "retouch"
    );
    println!(
        "{:>8}  {:>8}  {:>8}  {:>8}  {:>10}  {:>10}  {:>8}  {:>8}  {:>11}",
        "", "(post-PO)", "(post-PO)", "(post-PO)", "(PO)", "(PO)", "(post-RT)", "(post-RT)", "med"
    );
    println!("{}", "-".repeat(102));

    for &size in SIZES {
        let p = Probe::new(size);
        let slice = unsafe { std::slice::from_raw_parts_mut(p.ptr, p.len) };
        for i in (0..slice.len()).step_by(p.page_size) {
            unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
        }

        let (swp0, fb0, fa0, _ff0) = read_vmstat_thp();
        // SAFETY: PAGEOUT on our own VMA.
        let r = unsafe { libc::madvise(p.ptr.cast(), p.len, libc::MADV_PAGEOUT) };
        assert_eq!(r, 0, "PAGEOUT: {}", std::io::Error::last_os_error());
        let (swp1, fb1, fa1, _ff1) = read_vmstat_thp();

        let thp_a = p.anon_huge_bytes();
        let swap_a = p.swap_bytes();
        let (res, tot) = p.residency();
        let res_pct = (res as f64 / tot as f64) * 100.0;

        let start = Instant::now();
        for i in (0..slice.len()).step_by(p.page_size) {
            unsafe { std::ptr::write_volatile(&mut slice[i], 1) };
        }
        let retouch = start.elapsed();
        let (_swp2, _fb2, _fa2, _ff2) = read_vmstat_thp();

        let thp_b = p.anon_huge_bytes();
        let swap_b = p.swap_bytes();

        println!(
            "{:>8}  {:>8}  {:>8}  {:>7.1}%  {:>10}  {:>10}  {:>8}  {:>8}  {:>11}",
            format_size(size),
            thp_a >> 10,
            swap_a >> 10,
            res_pct,
            swp1 - swp0,
            fb1 - fb0,
            thp_b >> 10,
            swap_b >> 10,
            format_ns(retouch.as_nanos() as u64),
        );
        // Quiet the unused-binding lint for fault_alloc; keep for future use.
        let _ = (fa0, fa1);
    }

    println!();
    println!("Legend:");
    println!("  thpA_KB    AnonHugePages after PAGEOUT, KB");
    println!("  swapA_KB   Swap bytes (smaps) after PAGEOUT, KB — nonzero = actually written to swap");
    println!("  resA%      mincore residency after PAGEOUT");
    println!("  Δswpout    thp_swpout delta — count of THPs swapped out as whole PMDs");
    println!("  Δswp_fb    thp_swpout_fallback delta — count of THPs split-before-swap");
    println!("  thpB_KB    AnonHugePages after retouch");
    println!("  swapB_KB   Swap bytes after retouch — should be 0 if all pages refaulted back");
    println!();
    println!("Interpretation:");
    println!("  * swapA_KB ≈ size → PAGEOUT did write pages to swap");
    println!("  * swapA_KB ≈ 0  → pages only marked cold; still resident (no pressure)");
    println!("  * Δswpout > 0  → kernel swapped whole THPs (CONFIG_THP_SWAP path)");
    println!("  * Δswp_fb > 0  → kernel had to split THP before swap");
    println!("  * thpB_KB = 0 after retouch confirms swap-in is 4 KiB-granular (THP lost)");
}

/// Is there any swap configured on this system?
fn has_swap() -> bool {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("SwapTotal"))?
                .split_whitespace()
                .nth(1)?
                .parse::<u64>()
                .ok()
        })
        .is_some_and(|kb| kb > 0)
}

// ---------- main ----------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let want = |flag: &str| args.iter().any(|a| a == flag);

    print_env();

    let any_flag = want("--baseline")
        || want("--advise")
        || want("--pressure")
        || want("--swap-probe")
        || want("--pool")
        || want("--split-recovery")
        || want("--collapse-probe");
    if !any_flag || want("--baseline") {
        run_baseline();
    }
    if !any_flag || want("--advise") {
        run_advise_matrix();
    }
    if want("--pressure") {
        run_pressure();
    }
    if want("--swap-probe") {
        run_swap_probe();
    }
    if !any_flag || want("--pool") {
        run_pool();
    }
    if want("--split-recovery") {
        run_split_recovery();
    }
    if want("--collapse-probe") {
        run_collapse_probe();
    }
}

// Silence dead-code warnings for helpers held for upcoming experiments.
#[allow(dead_code)]
fn _keep_alive(p: &Probe) -> Duration {
    let _ = p.residency();
    let _ = p.anon_huge_bytes();
    Duration::ZERO
}

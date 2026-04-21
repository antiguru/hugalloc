//! Example that demonstrates hugalloc's anonymous memory allocations with THP hints.
fn main() {
    let buffer_size = 32 << 20;

    let buffers = 32;

    hugalloc::builder()
        .enable()
        .eager_return(true)
        .apply()
        .expect("apply config");

    println!("Allocating {buffers} regions of {buffer_size} size...");
    let mut regions: Vec<_> = (0..32)
        .map(|_| hugalloc::allocate::<u8>(32 << 20).unwrap())
        .collect();
    print_stats();

    for (ptr, cap, _handle) in &regions {
        println!("Setting region at {ptr:?}...");
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr.as_ptr(), *cap) };
        for i in slice {
            *i = 1;
        }
    }
    print_stats();

    let mut s = String::new();
    let stdin = std::io::stdin();

    println!("Enter to continue");
    stdin.read_line(&mut s).unwrap();
    print_stats();

    println!("Dropping regions");
    for (_ptr, _cap, handle) in regions.drain(..) {
        drop(handle);
    }

    println!("Enter to continue");
    stdin.read_line(&mut s).unwrap();
    print_stats();
}

fn print_stats() {
    let stats = hugalloc::stats();

    for (size_class, stats) in &stats.size_class {
        if stats.areas > 0 {
            println!(
                "size_class {size_class}: areas={}, total_bytes={}, free={}, clean={}, global={}, thread={}",
                stats.areas,
                stats.area_total_bytes,
                stats.free_regions,
                stats.clean_regions,
                stats.global_regions,
                stats.thread_regions,
            );
        }
    }
}

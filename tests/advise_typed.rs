use hugalloc::{AdviseError, Buffer, RawBuffer};

fn initialize() {
    hugalloc::builder()
        .enable()
        .background_interval(std::time::Duration::from_secs(1))
        .background_clear_bytes(4 << 20)
        .apply()
        .expect("apply");
}

#[test]
fn rawbuffer_prefetch_element_range() {
    initialize();
    let raw: RawBuffer<u64> = RawBuffer::with_capacity(1024);
    raw.prefetch(0..raw.capacity()).unwrap();
    raw.prefetch(100..200).unwrap();
    raw.cold(0..512).unwrap();
    raw.pageout(512..1024).unwrap();
    raw.restore_thp(0..raw.capacity()).unwrap();
    assert!(raw.prefetch(0..raw.capacity() + 1).is_err());
    assert!(raw.restore_thp(0..raw.capacity() + 1).is_err());
}

#[test]
fn buffer_prefetch_element_range() {
    initialize();
    let mut buf: Buffer<u32> = Buffer::with_capacity(1024);
    buf.extend_from_slice(&vec![0u32; 1024]);
    buf.prefetch(0..buf.len()).unwrap();
    buf.cold(0..buf.len()).unwrap();
    buf.pageout(0..buf.len()).unwrap();
    buf.restore_thp(0..buf.len()).unwrap();
    assert!(buf.prefetch(0..buf.len() + 1).is_err());
    assert!(buf.restore_thp(0..buf.len() + 1).is_err());
}

#[test]
fn buffer_prefetch_checked_mul_overflow() {
    initialize();
    let buf: Buffer<u64> = Buffer::heap(1024);
    // Element ranges whose byte conversion overflows usize should return OutOfBounds
    // with saturating byte values.
    let huge = usize::MAX / 2;
    let err = buf.prefetch(0..huge).unwrap_err();
    assert!(matches!(err, AdviseError::OutOfBounds { .. }));
}

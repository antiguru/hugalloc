//! `#[should_panic]` tests for `Buffer<T>`.
//!
//! These tests live in their own binary so that their intentional panics do
//! not poison the `GLOBAL_STATE_LOCK` mutex shared by `tests/buffer.rs`. Each
//! `tests/*.rs` file is compiled to a separate binary and runs in its own
//! process, giving these tests full isolation from the rest of the suite.

use hugalloc::Buffer;

fn initialize() {
    hugalloc::builder().enable().apply().expect("apply");
}

#[test]
#[should_panic(expected = "capacity")]
fn buffer_push_over_capacity_panics() {
    initialize();
    let mut buf: Buffer<u32> = Buffer::heap(2);
    buf.push(1);
    buf.push(2);
    buf.push(3); // Should panic.
}

#[test]
#[should_panic(expected = "capacity")]
fn buffer_extend_over_capacity_panics() {
    initialize();
    let mut buf: Buffer<u32> = Buffer::heap(4);
    buf.extend_from_slice(&[1, 2, 3, 4, 5]); // Should panic.
}

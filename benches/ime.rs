use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use zinput::ime::{Composition, ContextId, Selection};
use zinput::linux::{Endian, EvdevDecoder, Layout as EventLayout};

struct CountingAllocator;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Every allocation operation delegates to the process allocator with
// the original layout and pointer; only a separate atomic counter is updated.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn main() {
    const ITERATIONS: usize = 1_000_000;
    let mut storage = [0; 128];
    let mut output = [0; 128];
    let mut composition = Composition::new(&mut storage);
    composition.set_focus(Some(ContextId::new(1).unwrap()));
    let text = black_box("你好🙂");
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        let token = black_box(&mut composition).begin().unwrap();
        let token = black_box(&mut composition)
            .set_preedit(token, text, Selection::caret(text.len()))
            .unwrap();
        let result = black_box(&mut composition)
            .commit_preedit(token, &mut output)
            .unwrap();
        black_box(&output[..result.bytes_written]);
    }
    let elapsed = started.elapsed();
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(allocations, 0, "composition hot path must not allocate");
    println!(
        "composition: iterations={ITERATIONS} bytes={} capacity=128 elapsed_ns={} ns_per_lifecycle={:.2} allocations={allocations}",
        text.len(),
        elapsed.as_nanos(),
        elapsed.as_nanos() as f64 / ITERATIONS as f64
    );

    let mut record = [0; 24];
    record[16..18].copy_from_slice(&1_u16.to_le_bytes());
    record[18..20].copy_from_slice(&30_u16.to_le_bytes());
    record[20..24].copy_from_slice(&1_i32.to_le_bytes());
    let mut decoder = EvdevDecoder::new(EventLayout::Time64, Endian::Little);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let started = Instant::now();
    for _ in 0..ITERATIONS {
        black_box(&mut decoder).feed(black_box(&record), |event| {
            black_box(event);
        });
    }
    let elapsed = started.elapsed();
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(allocations, 0, "evdev hot path must not allocate");
    println!(
        "evdev: iterations={ITERATIONS} record_bytes=24 elapsed_ns={} ns_per_record={:.2} allocations={allocations}",
        elapsed.as_nanos(),
        elapsed.as_nanos() as f64 / ITERATIONS as f64
    );
}

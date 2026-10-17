//! A conforming custom allocator can call safe metadata APIs. It must not
//! unwind, so it catches the runtime's rejection internally. This regression
//! runs against the dependency's NORMAL build, not its cfg(test) implementation.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind},
};
use typed_runtime_prototype::Runtime;

thread_local! {
    static TARGET: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    static OPERATION: Cell<u8> = const { Cell::new(u8::MAX) };
    static REJECTED: Cell<usize> = const { Cell::new(0) };
}
fn reenter(operation: u8) {
    let armed = OPERATION
        .try_with(|op| {
            if op.get() == operation {
                op.set(u8::MAX);
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if armed {
        let rejected = catch_unwind(AssertUnwindSafe(|| {
            TARGET.with(|target| target.borrow().as_ref().unwrap().node_count());
        }))
        .is_err();
        // No assertions or unwind escape the allocator. Recursive allocations
        // from the panic machinery are disarmed before the callback starts.
        let _ = REJECTED.try_with(|n| n.set(usize::from(rejected)));
    }
}
struct Allocator;
// SAFETY: allocation/deallocation are delegated unchanged to System. The
// callback catches its own panic and never accesses the allocated bytes.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        reenter(0);
        unsafe { System.alloc(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        reenter(1);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        reenter(2);
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[test]
fn allocator_cannot_reborrow_graph_during_alloc_realloc_or_dealloc() {
    for operation in 0..3 {
        let rt = Runtime::new();
        if operation != 0 {
            for _ in 0..8 {
                rt.create_root().unwrap();
            }
        }
        TARGET.with(|target| *target.borrow_mut() = Some(rt.clone()));
        REJECTED.set(0);
        OPERATION.set(operation);
        match operation {
            0 => {
                rt.create_root().unwrap();
            }
            1 => {
                for _ in 0..32 {
                    rt.create_root().unwrap();
                }
            }
            _ => rt.shutdown().unwrap(),
        }
        OPERATION.set(u8::MAX);
        assert_eq!(REJECTED.get(), 1, "operation={operation}");
        rt.verify_invariants().unwrap();
        TARGET.with(|target| {
            target.borrow_mut().take();
        });
        rt.shutdown().unwrap();
    }
}

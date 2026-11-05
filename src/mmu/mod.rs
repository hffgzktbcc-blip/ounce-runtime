pub mod fastmem;
pub mod memory;

#[allow(unused_imports)]
pub use fastmem::{FastmemArena, DEFAULT_FASTMEM_CAPACITY};
#[allow(unused_imports)]
pub use memory::{
    AtomicRmwOp, Memory, MemoryError, Permission, MEM_STATE_CODE_MUTABLE, MEM_STATE_CODE_STATIC,
    MEM_STATE_FREE, MEM_STATE_HEAP, MEM_STATE_SHARED, MEM_STATE_STACK, MEM_STATE_THREAD_LOCAL,
    PAGE_SIZE,
};

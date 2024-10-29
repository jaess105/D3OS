pub mod alloc;
pub mod global_persistent_allocator;
pub mod nvmem;
pub mod nvram_allocator;
pub mod physical;
pub mod r#virtual;

#[derive(Clone, Copy)]
pub enum MemorySpace {
    Kernel,
    User,
}

pub const PAGE_SIZE: usize = 0x1000;

pub use nvmem::{create_persistent_pool, get_persistent_pool};

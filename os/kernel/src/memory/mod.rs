pub mod alloc;
pub mod global_persistent_allocator;
pub mod nvmem;
pub mod physical;
pub mod r#virtual;
pub mod pool;
pub mod vmm;
pub mod pages;
pub mod frames;

pub mod nvmem;

pub mod kheap;
pub mod stack;
pub mod acpi_handler;

#[derive(PartialEq)]
#[derive(Clone, Copy)]
pub enum MemorySpace {
    Kernel,
    User,
}

pub const PAGE_SIZE: usize = 0x1000;


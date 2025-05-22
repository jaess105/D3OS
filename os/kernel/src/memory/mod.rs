pub mod frames;
pub mod global_persistent_allocator;
pub mod nvmem;
pub mod pages;
pub mod pool;
pub mod vmm;

pub mod acpi_handler;
pub mod kheap;
pub mod stack;

#[derive(PartialEq, Clone, Copy)]
pub enum MemorySpace {
    Kernel,
    User,
}

pub const PAGE_SIZE: usize = 0x1000;

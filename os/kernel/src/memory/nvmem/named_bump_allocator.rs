use core::{fmt, sync::atomic::AtomicBool};

use crate::memory::nvmem::named_bump_allocator::{
    alloc_name::{Name, make_name},
    results::NvmemAllocResult,
};

#[repr(C)]
struct BlockHeader {
    size: u64,          // size of this block's data region
    next: Option<u64>,  // offset of next block, 0/sentinel if none
    in_use: AtomicBool, // the atomic "commit" flag — see alloc.rs notes
    name: Name,         // fixed-size, not necessarily NUL-terminated — track len separately or require NUL padding
}

impl BlockHeader {
    fn empty(input: &[u8]) -> Self {
        Self {
            size: 0,
            next: None,
            in_use: AtomicBool::new(false),
            name: make_name(input),
        }
    }
}

pub struct NamedBumpAllocator {
    head: BlockHeader,
}

impl NamedBumpAllocator {
    fn new() -> Self {
        Self {
            head: BlockHeader::empty(b"root"),
        }
    }

    /// Always overrides the element with the given value
    fn clean_alloc<T: fmt::Debug>(name: Name, element: T) -> NvmemAllocResult<T> {
        todo!()
    }

    /// Allocates memory for the struct and stores it in it or retrieves the value
    fn alloc_or_get<T: fmt::Debug>(name: Name, element: T) -> NvmemAllocResult<T> {
        todo!()
    }
}

mod pool {
    use super::BlockHeader;

    pub struct Pool {
        base: *mut u8, // mapped base address of the NVRAM region (volatile, set at mount)
        len: u64,
    }

    impl Pool {
        pub fn offset_to_ptr(&self, offset: u64) -> *mut u8 {
            (self.base as u64 + offset) as *mut u8
        }

        pub fn ptr_to_offset(&self, ptr: *mut u8) -> u64 {
            ptr as u64 - self.base as u64
        }

        pub fn header_at(&self, offset: u64) -> *mut BlockHeader {
            self.offset_to_ptr(offset) as *mut BlockHeader
        }
    }
}

mod allocation {
    use crate::memory::nvmem::named_bump_allocator::{pool::Pool, results::NvmemResult};

    struct NamedAllocator {
        pool: Pool,
    }

    impl NamedAllocator {
        fn alloc(&self, name: &str, size: u64) -> NvmemResult<u64> {
            todo!()
        } // returns offset

        fn free(&self, name: &str) -> NvmemResult<()> {
            todo!()
        }

        fn lookup(&self, name: &str) -> Option<u64> {
            todo!()
        }

        fn walk_chain(&self) -> impl Iterator<Item = u64> {
            todo!()
        } // yields block offsets
    }
}

mod flush {
    fn flush(ptr: *const u8) { /* clwb or clflushopt */
    }
    fn fence() { /* sfence */
    }
    fn persist(ptr: *const u8) {
        flush(ptr);
        fence();
    }
}

mod alloc_name {
    const NAME_LEN: usize = 64;

    pub type Name = [u8; NAME_LEN];

    /// Builds a `[u8; NAME_LEN]` from a byte-string literal, NUL-padded.
    /// Panics at compile time if the input is longer than NAME_LEN.
    pub const fn make_name(input: &[u8]) -> Name {
        assert!(input.len() <= NAME_LEN, "name exceeds NAME_LEN");
        assert!(input.len() > 0, "name cannot be empty");

        let mut buf = [0u8; NAME_LEN];
        let mut i = 0;
        while i < input.len() {
            buf[i] = input[i];
            i += 1;
        }
        buf
    }
}

mod results {
    use core::{error::Error, fmt};

    use alloc::alloc::AllocError;

    pub type NvmemResult<T> = Result<T, NvmemError>;

    #[derive(Debug)]
    pub enum NvmemError {
        Alloc(AllocError),
    }

    impl fmt::Display for NvmemError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                NvmemError::Alloc(e) => write!(f, "allocation failed: {e:?}"),
            }
        }
    }

    impl Error for NvmemError {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            match self {
                NvmemError::Alloc(_) => None, // AllocError doesn't impl Error (see below)
            }
        }
    }

    impl From<AllocError> for NvmemError {
        fn from(e: AllocError) -> Self {
            NvmemError::Alloc(e)
        }
    }
}

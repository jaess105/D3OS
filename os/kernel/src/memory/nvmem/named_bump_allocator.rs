use crate::memory::nvmem::named_bump_allocator::{
    alloc_name::{Name, make_name},
    pool::Pool,
    results::NvmemAllocResult,
};
use core::fmt;

pub use results::{NvmemError, NvmemResult};
pub use superblock::SuperblockBehavior;

/// Mounts (or formats, or ignores — see `SuperblockBehavior`) the pool at
/// `base..base+len` and returns a ready-to-use allocator.
pub unsafe fn init(base: *mut u8, len: u64, behavior: SuperblockBehavior) -> NamedBumpAllocator {
    let pool = unsafe { Pool::new(base, len) };
    superblock::mount(&pool, behavior);
    NamedBumpAllocator::new(pool)
}

pub struct NamedBumpAllocator {
    inner: allocation::NamedAllocator,
}

impl NamedBumpAllocator {
    pub fn new(pool: pool::Pool) -> Self {
        Self {
            inner: allocation::NamedAllocator::new(pool),
        }
    }

    /// Always overwrites the stored value for `name` (allocates on first
    /// use). Errors if the name already exists under a different size —
    /// this bump allocator can't resize a block in place.
    pub fn alloc<T: Copy + fmt::Debug>(&self, name: Name, element: T) -> NvmemAllocResult<T> {
        let size = core::mem::size_of::<T>() as u64;
        let offset = match self.inner.lookup(&name) {
            Some(existing) => {
                if self.inner.stored_size(&name) != Some(size) {
                    return Err(results::NvmemError::SizeMismatch);
                }
                existing
            }
            None => self.inner.alloc(&name, size)?,
        };
        self.write_value(offset, size, element);
        Ok(element)
    }

    /// Returns the existing value for `name` if present, otherwise
    /// allocates and stores `element`.
    pub fn get_or_alloc<T: Copy + fmt::Debug>(&self, name: Name, element: T) -> NvmemAllocResult<T> {
        if let Some(element) = self.get::<T>(name) {
            return Ok(element);
        }

        let size = core::mem::size_of::<T>() as u64;
        let offset = self.inner.alloc(&name, size)?;
        self.write_value(offset, size, element);
        Ok(element)
    }

    pub fn get<T: Copy + fmt::Debug>(&self, name: Name) -> Option<T> {
        self.inner.lookup(&name).map(|offset| {
            let ptr = self.inner.pool().offset_to_ptr(offset) as *const T;
            unsafe { ptr.read() }
        })
    }

    pub fn dealloc(&self, name: Name) -> NvmemResult<()> {
        self.inner.free(&name)
    }

    fn write_value<T: Copy>(&self, offset: u64, size: u64, element: T) {
        let ptr = self.inner.pool().offset_to_ptr(offset) as *mut T;
        unsafe { ptr.write(element) };
        flush::persist_range(ptr as *const u8, size);
    }
}

mod pool {
    use core::mem::size_of;

    use crate::memory::nvmem::named_bump_allocator::block_header::BlockHeader;

    pub struct Pool {
        base: *mut u8, // mapped base address of the NVRAM region (volatile, set at mount)
        len: u64,
    }

    impl Pool {
        /// Caller must guarantee `base..base+len` is a valid, mapped,
        /// exclusively-owned NVRAM region for the lifetime of this `Pool`.
        pub unsafe fn new(base: *mut u8, len: u64) -> Self {
            Self { base, len }
        }

        pub fn len(&self) -> u64 {
            self.len
        }

        pub fn start(&self) -> u64 {
            self.base as u64
        }

        pub fn offset_to_ptr(&self, offset: u64) -> *mut u8 {
            (self.base as u64 + offset) as *mut u8
        }

        pub fn ptr_to_offset(&self, ptr: *mut u8) -> u64 {
            ptr as u64 - self.base as u64
        }

        pub fn header_at(&self, offset: u64) -> *mut BlockHeader {
            self.offset_to_ptr(offset) as *mut BlockHeader
        }

        /// Offset just past a block's header — where its data region starts.
        pub fn data_offset(&self, header_offset: u64) -> u64 {
            header_offset + size_of::<BlockHeader>() as u64
        }
    }
}

/// The fixed, always-findable pool prefix: a magic number + version. This
/// is the one thing `mount` can check without trusting anything else in
/// the pool — everything past it (the root block, the chain) is only
/// trusted once this checks out.
mod superblock {
    use core::mem::size_of;

    use crate::memory::nvmem::named_bump_allocator::{block_header::BlockHeader, flush::persist_range, pool::Pool};

    pub const MAGIC: u64 = 0x4E564D454D5F4844; // "NVMEM_HD" as bytes, arbitrary but fixed
    pub const VERSION: u32 = 1;

    /// Block headers (the chain) start right after the superblock.
    pub const ROOT_OFFSET: u64 = size_of::<Superblock>() as u64;

    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct Superblock {
        magic: u64,
        version: u32,
    }

    /// What to do when the pool's magic/version don't match what this
    /// allocator expects — e.g. first-ever boot (all zeros/garbage), a
    /// pool written by an older/incompatible version, or genuine
    /// corruption. There is no way to tell these apart from the magic
    /// check alone, so the caller decides how to handle "don't know".
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SuperblockBehavior {
        /// Wipe and reinitialize: write a fresh superblock and an empty
        /// root block. Correct for first-ever boot; destroys any existing
        /// data if this was actually a version mismatch or corruption.
        Format,
        /// Panic. Use when a mismatch should never happen in practice
        /// (e.g. you control both the writer and reader version) and you
        /// want to fail loudly rather than risk misinterpreting old data.
        Throw,
        /// Proceed as if the pool were valid, trusting whatever bytes are
        /// already at `ROOT_OFFSET` onward. Dangerous — a genuinely
        /// uninitialized or corrupt pool will produce garbage offsets
        /// (this is the failure mode that motivated adding the check in
        /// the first place). Only meaningful if you have some other way
        /// to be sure the layout is actually compatible.
        Ignore,
    }

    fn read(pool: &Pool) -> Superblock {
        let ptr = pool.offset_to_ptr(0) as *const Superblock;
        // Any bit pattern is a structurally valid Superblock (plain u64 +
        // u32), so this read itself can't fail — it may just contain
        // garbage, which is exactly what the magic/version check below
        // is for.
        unsafe { ptr.read() }
    }

    /// Writes a fresh superblock + empty root block, in the order that
    /// keeps a crash mid-format safe: the root block goes first and is
    /// persisted, and the magic/version — the single value that makes
    /// the pool look valid to `mount` — is written and persisted last.
    /// A crash before the final write leaves a pool that still reads as
    /// "not yet formatted", never as "formatted but with a garbage root".
    fn format(pool: &Pool) {
        let root_ptr = pool.header_at(ROOT_OFFSET);
        unsafe {
            root_ptr.write(BlockHeader::new(b"root", 0));
        }
        persist_range(root_ptr as *const u8, size_of::<BlockHeader>() as u64);

        let sb = Superblock {
            magic: MAGIC,
            version: VERSION,
        };
        let sb_ptr = pool.offset_to_ptr(0) as *mut Superblock;
        unsafe {
            sb_ptr.write(sb);
        }
        persist_range(sb_ptr as *const u8, size_of::<Superblock>() as u64);
    }

    /// Checks the pool's superblock and applies `behavior` on mismatch.
    /// Must run before any other allocator operation touches the pool —
    /// `alloc`/`lookup`/`walk_chain` all assume a valid root block already
    /// exists at `ROOT_OFFSET`, which is only guaranteed once this has
    /// either confirmed the existing one or formatted a new one.
    pub fn mount(pool: &Pool, behavior: SuperblockBehavior) {
        let sb = read(pool);
        if sb.magic == MAGIC && sb.version == VERSION {
            return;
        }

        match behavior {
            SuperblockBehavior::Format => format(pool),
            SuperblockBehavior::Throw => panic!("NVMEM superblock mismatch: expected magic {:#x} version {}, found {:#x?}", MAGIC, VERSION, sb),
            SuperblockBehavior::Ignore => {
                // Deliberately do nothing — proceed with whatever is at
                // ROOT_OFFSET, valid or not. See the enum doc comment.
            }
        }
    }
}

mod allocation {
    use log::info;

    use crate::memory::nvmem::named_bump_allocator::{
        alloc_name::Name,
        block_header::BlockHeader,
        flush::persist_range,
        pool::Pool,
        results::{NvmemError, NvmemResult},
        superblock::ROOT_OFFSET,
    };
    use core::{
        mem::size_of,
        sync::atomic::{AtomicBool, Ordering},
    };

    pub struct NamedAllocator {
        pool: Pool,
    }

    impl NamedAllocator {
        pub fn new(pool: Pool) -> Self {
            Self { pool }
        }

        pub fn pool(&self) -> &Pool {
            &self.pool
        }

        /// Internal: finds the *header* offset for a live (in_use) name.
        fn find_header_offset(&self, name: &Name) -> Option<u64> {
            self.walk_chain().find(|&offset| {
                let header = unsafe { &*self.pool.header_at(offset) };
                header.is_used() && header.name_is(&name)
            })
        }

        /// Public lookup returns a *data* offset (past the header),
        /// matching what `alloc` hands back — callers shouldn't need to
        /// know header size.
        pub fn lookup(&self, name: &Name) -> Option<u64> {
            self.find_header_offset(name).map(|h| self.pool.data_offset(h))
        }

        pub fn stored_size(&self, name: &Name) -> Option<u64> {
            self.find_header_offset(name).map(|h| unsafe { &*self.pool.header_at(h) }.size())
        }

        pub fn alloc(&self, name: &Name, size: u64) -> NvmemResult<u64> {
            if self.lookup(name).is_some() {
                return Err(NvmemError::DuplicateName);
            }

            // Bump allocator: always append after the current tail. No
            // reuse of freed blocks, no coalescing — that's a deliberate
            // simplification for now, not an oversight.
            let last_offset = self.walk_chain().last().expect("chain always has at least the root block");
            let last_header = unsafe { &*self.pool.header_at(last_offset) };
            let new_header_offset = self.pool.data_offset(last_offset) + last_header.size();

            let header_size = size_of::<BlockHeader>() as u64;
            let new_full_size = new_header_offset + header_size + size;
            if new_header_offset + header_size + size > self.pool.len() {
                info!(
                    "NVMEM:
                    pool_start: {},
                new_header_offset: {}, header_size: {}, size: {},
                new full size: {}; greater than pool len: {}",
                    self.pool.start(),
                    new_header_offset,
                    header_size,
                    size,
                    new_full_size,
                    self.pool.len()
                );
                return Err(NvmemError::OutOfSpace);
            }

            // Step 1: write the full new header at its offset, but leave
            // it UNLINKED (the previous tail's `next` still doesn't point
            // here). A crash here just looks like untouched free space.
            let new_header_ptr = self.pool.header_at(new_header_offset);
            unsafe {
                new_header_ptr.write(BlockHeader::new(name, size));
            }
            persist_range(new_header_ptr as *const u8, header_size);

            // Step 2: mark in_use. Still unreachable via walk_chain, so
            // this is safe to do before linking.
            unsafe { &*new_header_ptr }.set_used();
            persist_range(new_header_ptr as *const u8, header_size);

            // Step 3: the real commit — splice into the chain by updating
            // the previous tail's `next`. This single write is what makes
            // the new block reachable. If you crash before this lands,
            // the new header is orphaned but harmless (never walked,
            // never trusted, and its space just looks unused past the
            // old tail).
            let last_header_mut = unsafe { &mut *self.pool.header_at(last_offset) };
            last_header_mut.set_next(new_header_offset);
            persist_range(last_header_mut.next_start(), size_of::<Option<u64>>() as u64);

            Ok(self.pool.data_offset(new_header_offset))
        }

        pub fn free(&self, name: &Name) -> NvmemResult<()> {
            let header_offset = self.find_header_offset(name).ok_or(NvmemError::NotFound)?;
            let header = unsafe { &*self.pool.header_at(header_offset) };
            header.set_unused();
            persist_range(header.in_use_start(), size_of::<AtomicBool>() as u64);
            // Note: space is not reclaimed or reused — consistent with
            // the bump-allocator model. A freed block just stops being
            // visible to lookup().
            Ok(())
        }

        pub fn walk_chain(&self) -> impl Iterator<Item = u64> + '_ {
            let mut current = Some(ROOT_OFFSET);
            core::iter::from_fn(move || {
                let offset = current?;
                let header = unsafe { &*self.pool.header_at(offset) };
                current = header.next();
                Some(offset)
            })
        }
    }
}

mod block_header {
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::memory::nvmem::named_bump_allocator::alloc_name::{Name, make_name};

    #[repr(C)]
    pub struct BlockHeader {
        size: u64,          // size of this block's data region
        next: Option<u64>,  // offset of next block, None if this is the tail
        in_use: AtomicBool, // the atomic "commit" flag — see alloc() notes
        name: Name,         // fixed-size, not NUL-terminated — compared as a full array
    }

    impl BlockHeader {
        pub fn empty(input: &[u8]) -> Self {
            Self::new(input, 0)
        }

        pub fn new(input: &[u8], size: u64) -> Self {
            Self {
                size,
                next: None,
                in_use: AtomicBool::new(false),
                name: make_name(input),
            }
        }

        pub fn in_use(&self) -> &AtomicBool {
            &self.in_use
        }

        pub fn name_is(&self, name: &[u8; 64]) -> bool {
            &self.name == name
        }

        pub fn size(&self) -> u64 {
            self.size
        }

        pub fn set_used(&self) {
            self.in_use.store(true, Ordering::Release)
        }

        pub fn is_used(&self) -> bool {
            self.in_use.load(Ordering::Acquire)
        }

        pub fn set_next(&mut self, new_header_offset: u64) {
            self.next = Some(new_header_offset)
        }

        pub fn set_unused(&self) {
            self.in_use.store(false, Ordering::Release)
        }

        pub fn next_start(&mut self) -> *const u8 {
            &self.next as *const Option<u64> as *const u8
        }

        pub fn next(&self) -> Option<u64> {
            self.next
        }

        pub fn in_use_start(&self) -> *const u8 {
            &self.in_use as *const AtomicBool as *const u8
        }
    }
}

mod flush {
    //! Cache-line writeback + fence primitives for making writes durable
    //! on NVRAM. x86_64 only.
    //!
    //! Instruction choice, in preference order: `clwb` (writes back without
    //! evicting — cheapest for metadata we're about to re-read) > `clflushopt`
    //! (writes back + evicts, still weakly ordered) > `clflush` (universally
    //! available, but ordered only against itself/same-address stores — used
    //! only as a last-resort fallback). Which one compiles in is decided at
    //! build time via target-feature cfg flags (set with e.g.
    //! `-C target-feature=+clwb` in your kernel's build config, or by
    //! targeting a `-target-cpu` that implies it). There is no runtime
    //! CPUID check here — if you need to support hardware you don't know
    //! about at build time, that's the place to add one, dispatching to
    //! one of the three fns below based on a CPUID feature bit read once
    //! at boot and cached, rather than branching on every flush call.
    //!
    //! `clwb`/`clflushopt` are unordered with respect to other stores and
    //! with each other, which is exactly why every write path in this
    //! allocator calls `persist_range` (flush every touched line, then a
    //! single `sfence`) rather than trusting the flush alone.

    use core::arch::asm;

    const CACHE_LINE: u64 = 64;

    #[cfg(target_feature = "clwb")]
    #[inline(always)]
    fn flush(ptr: *const u8) {
        unsafe {
            asm!("clwb byte ptr [{0}]", in(reg) ptr, options(nostack, preserves_flags));
        }
    }

    #[cfg(all(not(target_feature = "clwb"), target_feature = "clflushopt"))]
    #[inline(always)]
    fn flush(ptr: *const u8) {
        unsafe {
            asm!("clflushopt byte ptr [{0}]", in(reg) ptr, options(nostack, preserves_flags));
        }
    }

    #[cfg(all(not(target_feature = "clwb"), not(target_feature = "clflushopt")))]
    #[inline(always)]
    fn flush(ptr: *const u8) {
        unsafe {
            asm!("clflush byte ptr [{0}]", in(reg) ptr, options(nostack, preserves_flags));
        }
    }

    #[inline(always)]
    fn fence() {
        // sfence orders stores (including the writebacks above) — a store
        // fence is sufficient here since we're only ordering writes, not
        // interleaving with loads that need to observe them in order.
        unsafe {
            asm!("sfence", options(nostack, preserves_flags));
        }
    }

    /// Persist an arbitrary byte range, not just a single word — flushes
    /// every cache line the range touches, then fences once at the end
    /// (one fence per range, not per line — sfence is not cheap, and
    /// since clwb/clflushopt are unordered w.r.t. each other anyway,
    /// batching them behind a single trailing fence is both correct and
    /// faster than fencing after every line).
    pub fn persist_range(start: *const u8, len: u64) {
        if len == 0 {
            return;
        }
        let mut addr = (start as u64) - (start as u64 % CACHE_LINE);
        let end = start as u64 + len;
        while addr < end {
            flush(addr as *const u8);
            addr += CACHE_LINE;
        }
        fence();
    }
}

pub mod alloc_name {
    const NAME_LEN: usize = 64;
    pub type Name = [u8; NAME_LEN];

    /// Builds a `[u8; NAME_LEN]` from a byte-string literal, NUL-padded.
    /// Panics (at compile time, if used in a const context) if the input
    /// is empty or longer than NAME_LEN.
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
    use alloc::alloc::AllocError;
    use core::{error::Error, fmt};

    pub type NvmemResult<T> = Result<T, NvmemError>;
    pub type NvmemAllocResult<T> = Result<T, NvmemError>;

    #[derive(Debug)]
    pub enum NvmemError {
        Alloc(AllocError),
        DuplicateName,
        NotFound,
        OutOfSpace,
        SizeMismatch,
    }

    impl fmt::Display for NvmemError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                NvmemError::Alloc(e) => write!(f, "allocation failed: {e:?}"),
                NvmemError::DuplicateName => write!(f, "name already in use"),
                NvmemError::NotFound => write!(f, "name not found"),
                NvmemError::OutOfSpace => write!(f, "pool exhausted"),
                NvmemError::SizeMismatch => write!(f, "name exists with a different size"),
            }
        }
    }

    impl Error for NvmemError {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            None
        }
    }

    impl From<AllocError> for NvmemError {
        fn from(e: AllocError) -> Self {
            NvmemError::Alloc(e)
        }
    }
}

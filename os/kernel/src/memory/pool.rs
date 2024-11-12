use core::array;
use core::any::{TypeId, type_name};
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::mem;
use log::info;

const POOL_MAGIC: u64 = 0x4433_4F53_504F4F4C; // "D3OS_POOL" in hex
const MAX_JOURNAL_ENTRIES: usize = 64;
const MAX_OBJECT_ENTRIES: usize = 63;//TODO: Gerade noch 63 weil passt in 4kb

#[repr(u8)]
#[derive(Debug, Copy, Clone)]
pub enum Operation {
    Allocation = 1,
    Deallocation = 2,
    Modification = 3,
    ObjectTableUpdate = 4,
}

#[repr(C)]
#[derive(Debug)]
pub struct JournalEntry {
    valid: AtomicBool,
    operation: Operation,
    offset: u64,
    size: usize,
    old_data: [u8; 64],
    type_hash: u64,
    checksum: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct Journal {
    valid: AtomicBool,
    generation: AtomicU32,
    entries: [JournalEntry; MAX_JOURNAL_ENTRIES],
    entry_count: AtomicUsize,
}

#[repr(C)]
pub struct ObjectTableEntry {
    valid: AtomicBool,
    id: [u8; 55],
    id_len: u8,
    type_hash: u64,
    type_size: usize,
    data: Option<NonNull<u8>>, // Direct pointer to the allocated data
}



#[repr(C)]
#[repr(C)]
pub struct PoolHeader {
    magic: u64,
    size: usize,
    used_space: AtomicUsize,

    // Size class management
    small_blocks: [AtomicU64; 31], // Bitmap for blocks of sizes 64,128,...,1984
    medium_blocks: AtomicU64,      // Bitmap for 2048 byte blocks
    large_blocks: AtomicU64,       // Bitmap for 4096 byte blocks

    // Object table management
    object_table_offset: u64,
    max_objects: usize,

    // Data area management
    data_area_offset: u64,
    data_area_size: usize,
}

#[derive(Debug)]
pub enum PoolError {
    AllocationFailed,
    TransactionFailed,
    TypeMismatch {
        expected: &'static str,
        actual: &'static str,
    },
    InvalidId,
    ObjectTableFull,
    JournalFull,
}

#[derive(Debug)]
struct PoolLayout {
    header_size: usize,
    object_table_offset: usize,
    max_objects: usize,
    journal_offset: usize,
    data_start: usize,
}

pub struct Pool {
    base: u64,
    header: *mut PoolHeader,
}

impl Pool {
    fn calculate_pool_layout(total_size: usize) -> PoolLayout {
        // Start with header size
        let mut current_offset = mem::size_of::<PoolHeader>();
        current_offset = align_up(current_offset, 64);

        // Calculate how many objects we could potentially store
        // Assuming minimum object size of 64 bytes
        let data_space = total_size - current_offset;
        let max_objects = data_space / 64;

        // Calculate object table size
        let object_table_size = max_objects * mem::size_of::<ObjectTableEntry>();
        let object_table_offset = current_offset;
        current_offset += object_table_size;
        current_offset = align_up(current_offset, 64);

        // Journal comes after object table
        let journal_offset = current_offset;

        PoolLayout {
            header_size: mem::size_of::<PoolHeader>(),
            object_table_offset,
            max_objects,
            journal_offset,
            data_start: align_up(journal_offset + mem::size_of::<Journal>(), 64),
        }
    }
    pub fn new(base: u64, size: usize) -> Self {
        let header = base as *mut PoolHeader;

        unsafe {
            // Calculate layout
            let header_size = mem::size_of::<PoolHeader>();
            let header_aligned = align_up(header_size, 64);

            // Calculate max possible objects
            let data_start = align_up(header_aligned + mem::size_of::<ObjectTableEntry>(), 64);
            let data_space = size - data_start;
            let max_objects = data_space / 64; // Minimum object size is 64 bytes

            let object_table_size = max_objects * mem::size_of::<ObjectTableEntry>();

            info!("Pool layout:");
            info!("  Header size: {}", header_aligned);
            info!("  Object table size: {}", object_table_size);
            info!("  Max objects: {}", max_objects);
            info!("  Data start: {}", data_start);
            info!("  Data space: {}", data_space);

            // Initialize header
            ptr::write(header, PoolHeader {
                magic: POOL_MAGIC,
                size,
                used_space: AtomicUsize::new(0),

                // Initialize block bitmaps
                small_blocks: array::from_fn(|_| AtomicU64::new(0)),
                medium_blocks: AtomicU64::new(0),
                large_blocks: AtomicU64::new(0),

                object_table_offset: header_aligned as u64,
                max_objects,

                data_area_offset: data_start as u64,
                data_area_size: data_space,
            });

            Self { base, header }
        }
    }

    fn get_object_table(&self) -> *mut [ObjectTableEntry] {
        unsafe {
            let table_ptr = (self.base + (*self.header).object_table_offset) as *mut ObjectTableEntry;
            core::slice::from_raw_parts_mut(table_ptr, (*self.header).max_objects)
        }
    }

    // Helper to get journal
    unsafe fn get_journal(&self) -> *mut Journal {
        (self.base + (*self.header).) as *mut Journal
    }

    // Transaction handling
    pub fn transaction<F, R>(&mut self, f: F) -> Result<R, PoolError>
    where
        F: FnOnce(&mut TransactionContext) -> Result<R, PoolError>,
    {
        unsafe {
            let header = &mut *self.header;
            let journal = &mut *self.get_journal();

            // Start new transaction
            journal.entry_count.store(0, Ordering::Release);
            journal.valid.store(true, Ordering::Release);
            journal.generation.fetch_add(1, Ordering::AcqRel);
            Self::flush_cache_line(journal as *const _ as *const u8);

            let mut context = TransactionContext { pool: self };

            match f(&mut context) {
                Ok(result) => {
                    // Mark transaction as complete
                    journal.valid.store(false, Ordering::Release);
                    Self::flush_cache_line(journal as *const _ as *const u8);
                    Ok(result)
                }
                Err(e) => {
                    // Rollback transaction
                    self.rollback_journal(journal)?;
                    Err(e)
                }
            }
        }
    }

    // Object allocation and management
    pub fn allocate_with_id<T: Copy + 'static>(&mut self, id: &str, data: T) -> Result<(), PoolError> {
        self.transaction(|ctx| {
            ctx.allocate_with_id(id, data)
        })
    }

    pub fn get_by_id<T: Copy + 'static>(&self, id: &str) -> Result<T, PoolError> {
        unsafe {
            let header = &*self.header;
            let table = &*self.get_object_table();

            for i in 0..table.count.load(Ordering::Acquire) {
                let entry = &table.entries[i];
                if !entry.valid.load(Ordering::Acquire) {
                    continue;
                }

                let entry_id = core::str::from_utf8_unchecked(
                    &entry.id[..entry.id_len as usize]
                );

                if entry_id == id {
                    // Verify type
                    let expected_hash = Self::compute_type_hash::<T>();
                    if entry.type_hash != expected_hash {
                        return Err(PoolError::TypeMismatch {
                            expected: type_name::<T>(),
                            actual: "unknown",
                        });
                    }

                    // Read data
                    let ptr = entry.offset as *const T;
                    return Ok(ptr.read());
                }
            }

            Err(PoolError::InvalidId)
        }
    }

    pub fn modify_data<T: Copy + 'static>(&mut self, id: &str, f: impl FnOnce(&mut T)) -> Result<(), PoolError> {
        self.transaction(|ctx| {
            let data_ptr = ctx.get_by_id::<T>(id)?;
            ctx.modify(data_ptr, f)
        })
    }

    // Recovery
    pub fn recover(&mut self) -> Result<(), PoolError> {
        unsafe {
            let header = &mut *self.header;
            let journal = &mut *self.get_journal();

            if journal.valid.load(Ordering::Acquire) {
                info!("Found unfinished transaction, rolling back...");
                self.rollback_journal(journal)?;
            }

            Ok(())
        }
    }

    // Internal helpers
    fn rollback_journal(&mut self, journal: &mut Journal) -> Result<(), PoolError> {
        unsafe {
            let count = journal.entry_count.load(Ordering::Acquire);

            for i in (0..count).rev() {
                let entry = &journal.entries[i];
                if !entry.valid.load(Ordering::Acquire) {
                    continue;
                }

                match entry.operation {
                    Operation::Modification => {
                        // Restore old data
                        ptr::copy_nonoverlapping(
                            entry.old_data.as_ptr(),
                            entry.offset as *mut u8,
                            entry.size
                        );
                        Self::flush_cache_line(entry.offset as *const u8);
                    }
                    Operation::Allocation => {
                        // Free allocated block
                        self.free_block(entry.offset, entry.size)?;
                    }
                    Operation::ObjectTableUpdate => {
                        let table = &mut *self.get_object_table();
                        let entry_ptr = entry.offset as *mut ObjectTableEntry;
                        ptr::copy_nonoverlapping(
                            entry.old_data.as_ptr(),
                            entry_ptr as *mut u8,
                            core::mem::size_of::<ObjectTableEntry>()
                        );
                        Self::flush_cache_line(entry_ptr as *const u8);
                    }
                    _ => {}
                }
            }

            journal.valid.store(false, Ordering::Release);
            Self::flush_cache_line(journal as *const _ as *const u8);
            Ok(())
        }
    }

    fn compute_type_hash<T: 'static>() -> u64 {
        // Simple hash computation
        let mut hash = 5381u64;
        for byte in type_name::<T>().as_bytes() {
            hash = ((hash << 5) + hash) + *byte as u64;
        }
        hash
    }

    #[inline]
    fn flush_cache_line(ptr: *const u8) {
        unsafe {
            core::arch::x86_64::_mm_clflush(ptr);
            core::arch::x86_64::_mm_sfence();
        }
    }

    fn free_block(&mut self, offset: u64, size: usize) -> Result<(), PoolError> {
        unsafe {
            let header = &mut *self.header;
            let relative_offset = (offset - self.base) as usize;

            if size <= 1984 {
                let class = (size - 1) / 64;
                let block_index = relative_offset / (64 * class + 64);
                header.small_blocks[class].fetch_and(!(1 << block_index), Ordering::Release);
            } else if size <= 2048 {
                let block_index = (relative_offset - (31 * 64 * 64)) / 2048;
                header.medium_blocks.fetch_and(!(1 << block_index), Ordering::Release);
            } else {
                let block_index = (relative_offset - (31 * 64 * 64) - (64 * 2048)) / 4096;
                header.large_blocks.fetch_and(!(1 << block_index), Ordering::Release);
            }

            Ok(())
        }
    }
}

pub struct TransactionContext<'a> {
    pool: &'a mut Pool,
}

impl<'a> TransactionContext<'a> {
    pub fn allocate_with_id<T: Copy + 'static>(&mut self, id: &str, data: T) -> Result<(), PoolError> {
        unsafe {
            let size = mem::size_of::<T>();
            let ptr = self.allocate_block(size)?;

            // Journal the allocation
            let journal = &mut *self.pool.get_journal();
            let idx = journal.entry_count.fetch_add(1, Ordering::AcqRel);

            if idx >= MAX_JOURNAL_ENTRIES {
                return Err(PoolError::JournalFull);
            }

            let entry = &mut journal.entries[idx];
            entry.valid.store(true, Ordering::Release);
            entry.operation = Operation::Allocation;
            entry.offset = ptr as u64;
            entry.size = size;
            entry.type_hash = Pool::compute_type_hash::<T>();

            Pool::flush_cache_line(entry as *const _ as *const u8);

            // Write data
            ptr::write(ptr as *mut T, data);
            Pool::flush_cache_line(ptr as *const u8);

            // Update object table
            let table = &mut *self.pool.get_object_table();
            let count = table.count.fetch_add(1, Ordering::AcqRel);

            if count >= MAX_OBJECT_ENTRIES {
                return Err(PoolError::ObjectTableFull);
            }

            let table_entry = &mut table.entries[count];

            // Journal object table update
            let idx = journal.entry_count.fetch_add(1, Ordering::AcqRel);
            let journal_entry = &mut journal.entries[idx];

            journal_entry.valid.store(true, Ordering::Release);
            journal_entry.operation = Operation::ObjectTableUpdate;
            journal_entry.offset = table_entry as *mut _ as u64;

            // Backup old entry
            ptr::copy_nonoverlapping(
                table_entry as *const _ as *const u8,
                journal_entry.old_data.as_mut_ptr(),
                mem::size_of::<ObjectTableEntry>()
            );

            // Update entry
            table_entry.valid.store(true, Ordering::Release);
            table_entry.offset = ptr as u64;
            table_entry.size = size;
            table_entry.type_hash = Pool::compute_type_hash::<T>();

            let id_bytes = id.as_bytes();
            if id_bytes.len() > 55 {
                return Err(PoolError::InvalidId);
            }

            table_entry.id[..id_bytes.len()].copy_from_slice(id_bytes);
            table_entry.id_len = id_bytes.len() as u8;

            Pool::flush_cache_line(table_entry as *const _ as *const u8);

            Ok(())
        }
    }

    pub fn get_by_id<T: Copy + 'static>(&self, id: &str) -> Result<NonNull<T>, PoolError> {
        unsafe {
            let header = &*self.pool.header;
            let table = &*self.pool.get_object_table();

            for i in 0..table.count.load(Ordering::Acquire) {
                let entry = &table.entries[i];
                if !entry.valid.load(Ordering::Acquire) {
                    continue;
                }

                let entry_id = core::str::from_utf8_unchecked(
                    &entry.id[..entry.id_len as usize]
                );

                if entry_id == id {
                    // Verify type
                    let expected_hash = Pool::compute_type_hash::<T>();
                    if entry.type_hash != expected_hash {
                        return Err(PoolError::TypeMismatch {
                            expected: type_name::<T>(),
                            actual: "unknown",
                        });
                    }

                    return Ok(NonNull::new_unchecked(entry.offset as *mut T));
                }
            }

            Err(PoolError::InvalidId)
        }
    }

    pub fn modify<T: Copy>(
        &mut self,
        mut ptr: NonNull<T>,
        f: impl FnOnce(&mut T),
    ) -> Result<(), PoolError> {
        unsafe {
            let journal = &mut *self.pool.get_journal();
            let idx = journal.entry_count.fetch_add(1, Ordering::AcqRel);

            if idx >= MAX_JOURNAL_ENTRIES {
                return Err(PoolError::JournalFull);
            }

            let entry = &mut journal.entries[idx];
            entry.valid.store(true, Ordering::Release);
            entry.operation = Operation::Modification;
            entry.offset = ptr.as_ptr() as u64;
            entry.size = mem::size_of::<T>();

            // Backup old data
            ptr::copy_nonoverlapping(
                ptr.as_ptr() as *const u8,
                entry.old_data.as_mut_ptr(),
                mem::size_of::<T>()
            );

            Pool::flush_cache_line(entry as *const _ as *const u8);

            // Modify data
            f(ptr.as_mut());
            Pool::flush_cache_line(ptr.as_ptr() as *const u8);

            Ok(())
        }
    }

    fn allocate_block(&mut self, size: usize) -> Result<*mut u8, PoolError> {
        unsafe {
            let header = &mut *self.pool.header;

            let (ptr, _) = if size <= 1984 {
                let class = (size - 1) / 64;
                let bitmap = &header.small_blocks[class];
                self.find_free_block(bitmap, class, 64)
            } else if size <= 2048 {
                self.find_free_block(&header.medium_blocks, 0, 2048)
            } else if size <= 4096 {
                self.find_free_block(&header.large_blocks, 0, 4096)
            } else {
                return Err(PoolError::AllocationFailed);
            }?;

            Ok(ptr)
        }
    }

    fn find_free_block(
        &self,
        bitmap: &AtomicU64,
        class: usize,
        block_size: usize,
    ) -> Result<(*mut u8, usize), PoolError> {
        let bits = bitmap.load(Ordering::Acquire);
        let pos = (!bits).trailing_zeros() as usize;

        if pos >= 64 {
            return Err(PoolError::AllocationFailed);
        }

        bitmap.fetch_or(1 << pos, Ordering::Release);

        let offset = if block_size <= 64 {
            self.pool.base as usize + (class * 64 * 64) + (pos * block_size)
        } else if block_size == 2048 {
            self.pool.base as usize + (31 * 64 * 64) + (pos * block_size)
        } else {
            self.pool.base as usize + (31 * 64 * 64) + (64 * 2048) + (pos * block_size)
        };

        Ok((offset as *mut u8, pos))
    }
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

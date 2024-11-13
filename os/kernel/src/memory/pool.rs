use core::array;
use core::any::{TypeId, type_name};
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::mem;
use linked_list_allocator::LockedHeap;
use log::info;


const POOL_MAGIC: u64 = 0x4433_4F53_504F4F4C; // "D3OS_POOL" in hex
const  MAX_OBJECT_ENTRIES: usize = 64;

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
pub struct UndoLog {
    valid: AtomicBool,
    operation: Operation,
    offset: u64,
    size: usize,
    type_hash: u64,
    checksum: u32,
    old_data: [u8; 4096],
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
    max_objects: usize,
    object_table_offset: u64,
    //Blocks
    heap_start: u64,
    heap_size: usize,

    journal_entry_offset: u64,
    //Statistics
    used_space: AtomicUsize, //Total Space used by objects in byte
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

pub struct Pool {
    base_address: u64,
    header: *mut PoolHeader,
    object_table_offset: u64,
    heap: LockedHeap,
}

impl Pool {
    pub fn new(base: u64, size: usize) -> Self {
        let header = base as *mut PoolHeader;
        let object_table_offset = align_up(mem::size_of::<PoolHeader>(), 64) as u64;
        let heap_offset = align_up(object_table_offset as usize + mem::size_of::<ObjectTableEntry>() * MAX_OBJECT_ENTRIES, 64) as u64;
        let heap_size = size - mem::size_of::<UndoLog>() - mem::size_of::<ObjectTableEntry>() * MAX_OBJECT_ENTRIES - mem::size_of::<PoolHeader>();
        let journal_offset = align_up(heap_offset as usize + size - mem::size_of::<UndoLog>(), 64) as u64;


        unsafe {
            ptr::write(header, PoolHeader {
                magic: POOL_MAGIC,
                size,
                max_objects: MAX_OBJECT_ENTRIES,
                object_table_offset,
                heap_start: heap_offset,
                heap_size,
                journal_entry_offset: journal_offset + base,
                used_space: AtomicUsize::new(0),
            });
        }

        let mut pool = Self {
            base_address: base,
            header,
            object_table_offset,
            heap: LockedHeap::empty(),
        };

        //init the heap
        unsafe {
            pool.heap.lock().init(
                (base + heap_offset as u64) as *mut u8,
                heap_size
            );
        }

        pool.print_metadata_debug_info();
        pool

    }

    pub fn write_to_log(&self) {
        unsafe {
            let ptr = &*self.header;
            let ptr = ptr.journal_entry_offset;

            let log = ptr as *mut UndoLog;

            info!("Writing to log at 0x{:x}", log as u64);

            ptr::write(log, UndoLog {
                valid: AtomicBool::new(true),
                operation: Operation::Allocation,
                offset: 0,
                size: 1234,
                type_hash: 0,
                checksum: 0,
                old_data: [0; 4096],
            });


        }
    }



    // Transaction handling
    // pub fn transaction<F, R>(&mut self, f: F) -> Result<R, PoolError>
    // where
    //     F: FnOnce(&mut TransactionContext) -> Result<R, PoolError>,
    // {
    //     unsafe {
    //         let header = &mut *self.header;
    //         let journal = &mut *self.get_journal();
    //
    //         // Start new transaction
    //         journal.entry_count.store(0, Ordering::Release);
    //         journal.valid.store(true, Ordering::Release);
    //         journal.generation.fetch_add(1, Ordering::AcqRel);
    //         Self::flush_cache_line(journal as *const _ as *const u8);
    //
    //         let mut context = TransactionContext { pool: self };
    //
    //         match f(&mut context) {
    //             Ok(result) => {
    //                 // Mark transaction as complete
    //                 journal.valid.store(false, Ordering::Release);
    //                 Self::flush_cache_line(journal as *const _ as *const u8);
    //                 Ok(result)
    //             }
    //             Err(e) => {
    //                 // Rollback transaction
    //                 self.rollback_journal(journal)?;
    //                 Err(e)
    //             }
    //         }
    //     }
    // }
    //
    // // Object allocation and management
    // pub fn allocate_with_id<T: Copy + 'static>(&mut self, id: &str, data: T) -> Result<(), PoolError> {
    //     self.transaction(|ctx| {
    //         ctx.allocate_with_id(id, data)
    //     })
    // }
    //
    // pub fn get_by_id<T: Copy + 'static>(&self, id: &str) -> Result<T, PoolError> {
    //     unsafe {
    //         let header = &*self.header;
    //         let table = &*self.get_object_table();
    //
    //         for i in 0..table.count.load(Ordering::Acquire) {
    //             let entry = &table.entries[i];
    //             if !entry.valid.load(Ordering::Acquire) {
    //                 continue;
    //             }
    //
    //             let entry_id = core::str::from_utf8_unchecked(
    //                 &entry.id[..entry.id_len as usize]
    //             );
    //
    //             if entry_id == id {
    //                 // Verify type
    //                 let expected_hash = Self::compute_type_hash::<T>();
    //                 if entry.type_hash != expected_hash {
    //                     return Err(PoolError::TypeMismatch {
    //                         expected: type_name::<T>(),
    //                         actual: "unknown",
    //                     });
    //                 }
    //
    //                 // Read data
    //                 let ptr = entry.offset as *const T;
    //                 return Ok(ptr.read());
    //             }
    //         }
    //
    //         Err(PoolError::InvalidId)
    //     }
    // }
    //
    // pub fn modify_data<T: Copy + 'static>(&mut self, id: &str, f: impl FnOnce(&mut T)) -> Result<(), PoolError> {
    //     self.transaction(|ctx| {
    //         let data_ptr = ctx.get_by_id::<T>(id)?;
    //         ctx.modify(data_ptr, f)
    //     })
    // }
    //
    // // Recovery
    // pub fn recover(&mut self) -> Result<(), PoolError> {
    //     unsafe {
    //         let header = &mut *self.header;
    //         let journal = &mut *self.get_journal();
    //
    //         if journal.valid.load(Ordering::Acquire) {
    //             info!("Found unfinished transaction, rolling back...");
    //             self.rollback_journal(journal)?;
    //         }
    //
    //         Ok(())
    //     }
    // }
    //
    // // Internal helpers
    // fn rollback_journal(&mut self, journal: &mut Journal) -> Result<(), PoolError> {
    //     unsafe {
    //         let count = journal.entry_count.load(Ordering::Acquire);
    //
    //         for i in (0..count).rev() {
    //             let entry = &journal.entries[i];
    //             if !entry.valid.load(Ordering::Acquire) {
    //                 continue;
    //             }
    //
    //             match entry.operation {
    //                 Operation::Modification => {
    //                     // Restore old data
    //                     ptr::copy_nonoverlapping(
    //                         entry.old_data.as_ptr(),
    //                         entry.offset as *mut u8,
    //                         entry.size
    //                     );
    //                     Self::flush_cache_line(entry.offset as *const u8);
    //                 }
    //                 Operation::Allocation => {
    //                     // Free allocated block
    //                     self.free_block(entry.offset, entry.size)?;
    //                 }
    //                 Operation::ObjectTableUpdate => {
    //                     let table = &mut *self.get_object_table();
    //                     let entry_ptr = entry.offset as *mut ObjectTableEntry;
    //                     ptr::copy_nonoverlapping(
    //                         entry.old_data.as_ptr(),
    //                         entry_ptr as *mut u8,
    //                         core::mem::size_of::<ObjectTableEntry>()
    //                     );
    //                     Self::flush_cache_line(entry_ptr as *const u8);
    //                 }
    //                 _ => {}
    //             }
    //         }
    //
    //         journal.valid.store(false, Ordering::Release);
    //         Self::flush_cache_line(journal as *const _ as *const u8);
    //         Ok(())
    //     }
    // }
    //
    //

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

    fn print_metadata_debug_info(&self) {
        unsafe {
            let header = &*self.header;
            info!("Pool Metadata:");
            info!("  - Base address: 0x{:x}", self.base_address);
            info!("  - Size: {} bytes", header.size);
            info!("  - Max objects: {}", header.max_objects);
            info!("  - Header size: {} bytes", mem::size_of::<PoolHeader>());
            info!("  - Object table offset: 0x{:x}", header.object_table_offset);
            info!("  - Object table start: 0x{:x}", self.base_address + header.object_table_offset);
            info!("  - Object table EntrySize: {} bytes", mem::size_of::<ObjectTableEntry>());
            info!("  - Heap offset: 0x{:x}", header.heap_start);
            info!("  - Heap start: 0x{:x}", self.base_address + header.heap_start);
            info!("  - Heap size: {} bytes", header.heap_size);
            info!("  - Journal entry offset: 0x{:x}", header.journal_entry_offset);
            info!("  - Used space: {} bytes", header.used_space.load(Ordering::Acquire));
        }
    }
}

pub struct TransactionContext<'a> {
    pool: &'a mut Pool,
}

// impl<'a> TransactionContext<'a> {
//     pub fn allocate_with_id<T: Copy + 'static>(&mut self, id: &str, data: T) -> Result<(), PoolError> {
//         unsafe {
//             let size = mem::size_of::<T>();
//             let ptr = self.allocate_block(size)?;
//
//             // Journal the allocation
//             let journal = &mut *self.pool.get_journal();
//             let idx = journal.entry_count.fetch_add(1, Ordering::AcqRel);
//
//             if idx >= MAX_JOURNAL_ENTRIES {
//                 return Err(PoolError::JournalFull);
//             }
//
//             let entry = &mut journal.entries[idx];
//             entry.valid.store(true, Ordering::Release);
//             entry.operation = Operation::Allocation;
//             entry.offset = ptr as u64;
//             entry.size = size;
//             entry.type_hash = Pool::compute_type_hash::<T>();
//
//             Pool::flush_cache_line(entry as *const _ as *const u8);
//
//             // Write data
//             ptr::write(ptr as *mut T, data);
//             Pool::flush_cache_line(ptr as *const u8);
//
//             // Update object table
//             let table = &mut *self.pool.get_object_table();
//             let count = table.count.fetch_add(1, Ordering::AcqRel);
//
//             if count >= MAX_OBJECT_ENTRIES {
//                 return Err(PoolError::ObjectTableFull);
//             }
//
//             let table_entry = &mut table.entries[count];
//
//             // Journal object table update
//             let idx = journal.entry_count.fetch_add(1, Ordering::AcqRel);
//             let journal_entry = &mut journal.entries[idx];
//
//             journal_entry.valid.store(true, Ordering::Release);
//             journal_entry.operation = Operation::ObjectTableUpdate;
//             journal_entry.offset = table_entry as *mut _ as u64;
//
//             // Backup old entry
//             ptr::copy_nonoverlapping(
//                 table_entry as *const _ as *const u8,
//                 journal_entry.old_data.as_mut_ptr(),
//                 mem::size_of::<ObjectTableEntry>()
//             );
//
//             // Update entry
//             table_entry.valid.store(true, Ordering::Release);
//             table_entry.offset = ptr as u64;
//             table_entry.size = size;
//             table_entry.type_hash = Pool::compute_type_hash::<T>();
//
//             let id_bytes = id.as_bytes();
//             if id_bytes.len() > 55 {
//                 return Err(PoolError::InvalidId);
//             }
//
//             table_entry.id[..id_bytes.len()].copy_from_slice(id_bytes);
//             table_entry.id_len = id_bytes.len() as u8;
//
//             Pool::flush_cache_line(table_entry as *const _ as *const u8);
//
//             Ok(())
//         }
//     }
//
//     pub fn get_by_id<T: Copy + 'static>(&self, id: &str) -> Result<NonNull<T>, PoolError> {
//         unsafe {
//             let header = &*self.pool.header;
//             let table = &*self.pool.get_object_table();
//
//             for i in 0..table.count.load(Ordering::Acquire) {
//                 let entry = &table.entries[i];
//                 if !entry.valid.load(Ordering::Acquire) {
//                     continue;
//                 }
//
//                 let entry_id = core::str::from_utf8_unchecked(
//                     &entry.id[..entry.id_len as usize]
//                 );
//
//                 if entry_id == id {
//                     // Verify type
//                     let expected_hash = Pool::compute_type_hash::<T>();
//                     if entry.type_hash != expected_hash {
//                         return Err(PoolError::TypeMismatch {
//                             expected: type_name::<T>(),
//                             actual: "unknown",
//                         });
//                     }
//
//                     return Ok(NonNull::new_unchecked(entry.offset as *mut T));
//                 }
//             }
//
//             Err(PoolError::InvalidId)
//         }
//     }
//
//     pub fn modify<T: Copy>(
//         &mut self,
//         mut ptr: NonNull<T>,
//         f: impl FnOnce(&mut T),
//     ) -> Result<(), PoolError> {
//         unsafe {
//             let journal = &mut *self.pool.get_journal();
//             let idx = journal.entry_count.fetch_add(1, Ordering::AcqRel);
//
//             if idx >= MAX_JOURNAL_ENTRIES {
//                 return Err(PoolError::JournalFull);
//             }
//
//             let entry = &mut journal.entries[idx];
//             entry.valid.store(true, Ordering::Release);
//             entry.operation = Operation::Modification;
//             entry.offset = ptr.as_ptr() as u64;
//             entry.size = mem::size_of::<T>();
//
//             // Backup old data
//             ptr::copy_nonoverlapping(
//                 ptr.as_ptr() as *const u8,
//                 entry.old_data.as_mut_ptr(),
//                 mem::size_of::<T>()
//             );
//
//             Pool::flush_cache_line(entry as *const _ as *const u8);
//
//             // Modify data
//             f(ptr.as_mut());
//             Pool::flush_cache_line(ptr.as_ptr() as *const u8);
//
//             Ok(())
//         }
//     }
//
//     fn allocate_block(&mut self, size: usize) -> Result<*mut u8, PoolError> {
//         unsafe {
//             let header = &mut *self.pool.header;
//
//             let (ptr, _) = if size <= 1984 {
//                 let class = (size - 1) / 64;
//                 let bitmap = &header.small_blocks[class];
//                 self.find_free_block(bitmap, class, 64)
//             } else if size <= 2048 {
//                 self.find_free_block(&header.medium_blocks, 0, 2048)
//             } else if size <= 4096 {
//                 self.find_free_block(&header.large_blocks, 0, 4096)
//             } else {
//                 return Err(PoolError::AllocationFailed);
//             }?;
//
//             Ok(ptr)
//         }
//     }
//
//     fn find_free_block(
//         &self,
//         bitmap: &AtomicU64,
//         class: usize,
//         block_size: usize,
//     ) -> Result<(*mut u8, usize), PoolError> {
//         let bits = bitmap.load(Ordering::Acquire);
//         let pos = (!bits).trailing_zeros() as usize;
//
//         if pos >= 64 {
//             return Err(PoolError::AllocationFailed);
//         }
//
//         bitmap.fetch_or(1 << pos, Ordering::Release);
//
//         let offset = if block_size <= 64 {
//             self.pool.base as usize + (class * 64 * 64) + (pos * block_size)
//         } else if block_size == 2048 {
//             self.pool.base as usize + (31 * 64 * 64) + (pos * block_size)
//         } else {
//             self.pool.base as usize + (31 * 64 * 64) + (64 * 2048) + (pos * block_size)
//         };
//
//         Ok((offset as *mut u8, pos))
//     }
// }

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

use core::alloc::Layout;
use core::any::{TypeId, type_name};
use core::array;
use core::mem;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use linked_list_allocator::LockedHeap;
use log::info;

const POOL_MAGIC: u64 = 0x4433_4F53_504F4F4C; // "D3OS_POOL" in hex
const MAX_OBJECT_ENTRIES: usize = 64;

#[repr(u8)]
#[derive(Debug, Copy, Clone)]
pub enum Operation {
    Allocation = 1,
    Deallocation = 2,
    Modification = 3,
    ObjectTableUpdate = 4,
}

//TODO: Only for modification
#[repr(C)]
#[derive(Debug)]
pub struct UndoLog {
    valid: AtomicBool,
    operation: Operation,
    offset: u64,// Ref to the objectTableEntry
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
                magic: POOL_MAGIC,//TODO: only for debug
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

    // Transaction handling
    pub fn transaction<F, R>(&mut self, f: F) -> Result<R, PoolError>
    where
        F: FnOnce(&mut TransactionContext) -> Result<R, PoolError>,
    {
        unsafe {
            // Start transaction by initializing the log
            let log = &mut *((*self.header).journal_entry_offset as *mut UndoLog);
            log.valid.store(true, Ordering::Release);
            Self::flush_cache_line(log as *const _ as *const u8);

            // Create transaction context
            let mut ctx = TransactionContext { pool: self };

            // Execute transaction
            match f(&mut ctx) {
                Ok(result) => {
                    // Mark transaction as complete
                    log.valid.store(false, Ordering::Release);
                    Self::flush_cache_line(log as *const _ as *const u8);
                    Ok(result)
                }
                Err(e) => {
                    // Roll back changes
                    self.rollback_log(log)?;
                    Err(e)
                }
            }
        }
    }

    fn rollback_log(&mut self, log: &mut UndoLog) -> Result<(), PoolError> {
        if !log.valid.load(Ordering::Acquire) {
            return Ok(());
        }

        unsafe {
            match log.operation {
                Operation::Modification => {
                    // Restore old data
                    ptr::copy_nonoverlapping(
                        log.old_data.as_ptr(),
                        log.offset as *mut u8,
                        log.size
                    );
                    Self::flush_cache_line(log.offset as *const u8);
                }
                Operation::Allocation => {
                    // Free allocated memory
                    if let Some(ptr) = NonNull::new(log.offset as *mut u8) {
                        self.heap.lock().deallocate(ptr, Layout::from_size_align(log.size, 1).unwrap());//TODO: check alignment
                    }
                }
                //TODO: Macht für mich noch keinen Sinn!
                Operation::ObjectTableUpdate => {
                    // Restore old object table entry
                    let entry = log.offset as *mut ObjectTableEntry;
                    ptr::copy_nonoverlapping(
                        log.old_data.as_ptr(),
                        entry as *mut u8,
                        mem::size_of::<ObjectTableEntry>()
                    );
                    Self::flush_cache_line(entry as *const u8);
                }
                _ => {}
            }

            log.valid.store(false, Ordering::Release);
            Self::flush_cache_line(log as *const _ as *const u8);
            Ok(())
        }
    }


    //
    // // Object allocation and management
    // pub fn allocate_with_id<T: Copy + 'static>(&mut self, id: &str, data: T) -> Result<(), PoolError> {
    //     self.transaction(|ctx| {
    //         ctx.allocate_with_id(id, data)
    //     })
    // }
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

    pub fn detailed_log_dump(&self) {
        unsafe {
            let log = &*((*self.header).journal_entry_offset as *mut UndoLog);
            let log_addr = (*self.header).journal_entry_offset;

            info!("=== Detailed Log Structure Dump ===");
            info!("Base Address: 0x{:x}", log_addr);
            info!("Offset  Content                 Interpretation");
            info!("-------------------------------------------");

            // Print each field with its offset and raw bytes
            info!("+0x00: {:016x}  Valid: {}",
                  *(log_addr as *const u64),
                  log.valid.load(Ordering::Acquire));

            info!("+0x08: {:02x}               Operation: {:?}",
                  log.operation as u8,
                  log.operation);

            info!("+0x10: {:016x}  Offset: 0x{:x}",
                  log.offset,
                  log.offset);

            info!("+0x18: {:016x}  Size: {} bytes",
                  log.size as u64,
                  log.size);

            info!("+0x20: {:016x}  Type Hash: 0x{:x}",
                  log.type_hash,
                  log.type_hash);

            info!("Old Data:");
            for i in 0..log.size.min(32) {
                if i % 16 == 0 {
                    print!("\n+0x{:02x}: ", 0x30 + i);
                }
                print!("{:02x} ", log.old_data[i]);
            }
            info!("");
        }
    }

    pub fn debug_print_object_table(&self) {
        unsafe {
            let table_base = (self.base_address + (*self.header).object_table_offset)
                as *const ObjectTableEntry;

            info!("=== Object Table Debug Information ===");
            info!("Object Table Location: 0x{:x}", table_base as u64);

            for i in 0..MAX_OBJECT_ENTRIES {
                let entry = &*table_base.add(i);
                if entry.valid.load(Ordering::Acquire) {
                    let id_str = core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]);
                    info!("Entry #{}", i);
                    info!("  ID: {}", id_str);
                    info!("  Type Hash: 0x{:x}", entry.type_hash);
                    info!("  Type Size: {} bytes", entry.type_size);
                    info!("  Data Pointer: {:?}", entry.data);
                }
            }
        }
    }
}

pub struct TransactionContext<'a> {
    pool: &'a mut Pool,
}

impl<'a> TransactionContext<'a> {

    pub fn allocate_with_id<T: Copy + 'static>(&mut self, id: &str, data: T) -> Result<(), PoolError> {
        if id.len() > 55 {
            return Err(PoolError::InvalidId);
        }

        if let Ok(ptr) = self.get_by_id::<T>(id) {
            // If it exists, modify it
            info!("Object with ID '{}' already exists, modifying...", id);
            self.modify(ptr, |existing| *existing = data)?;
            return Ok(());
        }

        unsafe {
            // Allocate memory for the object
            let layout = Layout::new::<T>();
            let ptr = self.pool.heap.lock()
                .allocate_first_fit(layout)
                .map_err(|_| PoolError::AllocationFailed)?;

            // Log the allocation
            let log = &mut *((*self.pool.header).journal_entry_offset as *mut UndoLog);
            log.operation = Operation::Allocation;
            log.offset = ptr.as_ptr() as u64;
            log.size = layout.size();
            log.type_hash = Pool::compute_type_hash::<T>();
            Pool::flush_cache_line(log as *const _ as *const u8);

            //TODO: hier debug
            info!("=== After Allocation info ===");
            self.pool.detailed_log_dump();

            // Write the data
            ptr::write(ptr.as_ptr() as *mut T, data);
            Pool::flush_cache_line(ptr.as_ptr() as *const u8);

            // Find free entry in object table
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset)
                as *mut ObjectTableEntry;
            let mut free_entry = None;

            for i in 0..MAX_OBJECT_ENTRIES {
                let entry = &mut *table_base.add(i);
                if !entry.valid.load(Ordering::Acquire) {
                    free_entry = Some(entry);
                    break;
                }
            }

            let entry = free_entry.ok_or(PoolError::ObjectTableFull)?;

            // Log object table update
            log.operation = Operation::ObjectTableUpdate;
            log.offset = entry as *mut _ as u64;
            ptr::copy_nonoverlapping(
                entry as *const ObjectTableEntry as *const u8,
                log.old_data.as_mut_ptr(),
                mem::size_of::<ObjectTableEntry>()
            );
            Pool::flush_cache_line(log as *const _ as *const u8);

            //TODO: hier debug
            info!("");
            info!("=== Object Table info logged ===");
            info!("");
            self.pool.detailed_log_dump();

            // Update object table entry
            let id_bytes = id.as_bytes();
            if id_bytes.len() > 55 {
                return Err(PoolError::InvalidId);
            }

            entry.valid.store(true, Ordering::Release);
            entry.id[..id_bytes.len()].copy_from_slice(id_bytes);
            entry.id_len = id_bytes.len() as u8;
            entry.type_hash = Pool::compute_type_hash::<T>();
            entry.type_size = mem::size_of::<T>();
            entry.data = Some(ptr);

            Pool::flush_cache_line(entry as *const _ as *const u8);

            //TODO: hier debug
            //info!("=== Final info ===");
            //self.pool.debug_print_object_table();

            Ok(())
        }
    }

    pub fn get_by_id<T: Copy + 'static>(&self, id: &str) -> Result<NonNull<T>, PoolError> {
        unsafe {
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset)
                as *const ObjectTableEntry;

            for i in 0..MAX_OBJECT_ENTRIES {
                let entry = &*table_base.add(i);
                if entry.valid.load(Ordering::Acquire) {
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

                        return Ok(NonNull::new_unchecked(entry.data.unwrap().as_ptr() as *mut T));
                    }
                }
            }

            Err(PoolError::InvalidId)
        }
    }

    pub fn read_by_id<T: Copy + 'static>(&self, id: &str) -> Result<T, PoolError> {
        let ptr = self.get_by_id::<T>(id)?;
        unsafe {
            Ok(*ptr.as_ref())
        }
    }

    pub fn modify<T: Copy>(
        &mut self,
        ptr: NonNull<T>,
        f: impl FnOnce(&mut T),
    ) -> Result<(), PoolError> {
        unsafe {
            // Log the modification
            let log = &mut *((*self.pool.header).journal_entry_offset as *mut UndoLog);
            log.operation = Operation::Modification;
            log.offset = ptr.as_ptr() as u64;
            log.size = mem::size_of::<T>();

            // Backup old data
            ptr::copy_nonoverlapping(
                ptr.as_ptr() as *const u8,
                log.old_data.as_mut_ptr(),
                mem::size_of::<T>()
            );
            Pool::flush_cache_line(log as *const _ as *const u8);

            //TODO: hier debug
            info!("=== LogModification info ===");
            self.pool.detailed_log_dump();

            // Modify data
            f(&mut *ptr.as_ptr());
            Pool::flush_cache_line(ptr.as_ptr() as *const u8);

            Ok(())
        }
    }
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

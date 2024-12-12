use alloc::vec::Vec;
use core::alloc::Layout;
use core::any::{type_name};
use core::arch::x86_64::_rdtsc;
use core::mem;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use linked_list_allocator::LockedHeap;
use log::info;
use crate::memory::global_persistent_allocator::qemu_exit;
use memory::global_persistent_allocator::{FIXED_POOL_SIZE, LOG_POOL_NAME};
use crate::memory;

static mut LOG_POOL: Option<Pool> = None;

const POOL_MAGIC: u64 = 0x4433_4F53_504F4F4C; // "D3OS_POOL" in hex
const MAX_OBJECT_ENTRIES: usize = 1024; //~88B*1024 for table

impl From<core::alloc::LayoutError> for PoolError {
    fn from(_: core::alloc::LayoutError) -> Self {
        PoolError::LayoutError
    }
}

//For log_pool!
#[repr(u8)]
#[derive(Copy, Clone)]
enum OperationType {
    Allocation,
    Modification,
    Deallocation,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct LoggedOperation {
    op_type: OperationType,
    pool_base_address: u64,
    absolute_address: u64,
    data_size: usize,
    type_hash: u64,
}



#[repr(C)]
pub struct ObjectTableEntry {
    valid: AtomicBool, // Data is valid (was written)
    active: AtomicBool, // Data is active (now usable) -> both has to be active
    operation_done: AtomicBool,
    padding_: AtomicBool,
    id: [u8; 32],
    id_len: u8,
    type_hash: u64,
    type_size: usize,
    data: Option<NonNull<u8>>, // Direct pointer to the allocated data
}

#[repr(C)]
pub(crate) struct PoolHeader {
    pub(crate) magic: u64,
    size: usize,
    max_objects: usize,
    object_table_offset: u64,
    //Blocks
    heap_start: u64,
    heap_size: usize,

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
    InconsistentState,
    LogPoolFull,
    LogPoolNotAvailable,
    LayoutError,
}


pub struct Pool {
    pub(crate) base_address: u64,
    pub(crate) header: *mut PoolHeader,
    object_table_offset: u64,
    heap: LockedHeap,
}
#[allow(static_mut_refs)]
impl Pool {
    //pub fn new(base: u64, size: usize, log_pool_address: Option<u64>) -> Self {
    pub fn new(base: u64, size: usize) -> Self {
        let header = base as *mut PoolHeader;
        let object_table_offset = align_up(mem::size_of::<PoolHeader>(), 64) as u64;
        let heap_offset = align_up(object_table_offset as usize + mem::size_of::<ObjectTableEntry>() * MAX_OBJECT_ENTRIES, 64) as u64;
        //let heap_size = size - heap_offset as usize - object_table_offset as usize;
        let heap_size = size - heap_offset as usize;

        let pool = Self {
            base_address: base,
            header,
            object_table_offset,
            heap: LockedHeap::empty(),
        };

        unsafe {
            if (*header).magic != POOL_MAGIC {
                ptr::write(header, PoolHeader {
                    magic: POOL_MAGIC,
                    size,
                    max_objects: MAX_OBJECT_ENTRIES,
                    object_table_offset,
                    heap_start: heap_offset,
                    heap_size,
                    used_space: AtomicUsize::new(0),
                });

                Self::flush_cache_line(header as *const _ as *const u8);

            } else {
                //info!("Reusing existing pool at 0x{:x}", base);
                //TODO: EVTL Noch sanity check?
            }

            pool.heap.lock().init(
                (base + heap_offset) as *mut u8,
                heap_size
            );

        }
        //pool.print_metadata_debug_info();
        pool
    }

    // Transaction handling
    pub fn transaction<F, R>(&mut self, f: F) -> Result<R, PoolError>
    where
        F: FnOnce(&mut TransactionContext) -> Result<R, PoolError>,
    {
        if let Some(log_pool) = self.get_log_pool() {
            let has_logs = unsafe {
                let table_base = (log_pool.base_address + log_pool.object_table_offset)
                    as *const ObjectTableEntry;

                (0..(*log_pool.header).max_objects).any(|i| {
                    let entry = &*table_base.add(i);
                    entry.active.load(Ordering::Acquire) &&
                        entry.valid.load(Ordering::Acquire)
                })
            };

            if has_logs {
                info!("Systemcrash happend inside a Rollback .. Continue with rollback");
                Self::perform_rollback(log_pool.base_address)?;
                Self::empty_log_pool(log_pool.base_address);
            }

            // Clear log pool for new transaction
            // log_pool.empty_log_pool();
         }

        // Always check for recovery at start of transaction
        //DOC: Hier wird  der Tatsächliche Speicher gelöscht
        self.recover()?;

        let mut ctx = TransactionContext {
            pool: self,
            pending_changes: Vec::new(),
        };

        match f(&mut ctx) {
            Ok(result) => {
                //info!("Transaction successful, committing changes...");
                // Activate all changes

                for change in ctx.pending_changes {
                    unsafe {
                        // mark entry valid at the end of transaction
                        (*change.entry).valid.store(true, Ordering::Release);
                        Self::flush_cache_line(change.entry as *const u8);
                    }
                }
                // Clear log pool after successful transaction
                if let Some(mut log_pool) = self.get_log_pool() {
                    Self::empty_log_pool(log_pool.base_address);
                }

                Ok(result)
            }
            Err(e) => {
               //info!("Transaction failed, rolling back changes");
                if let Some(log_pool) = self.get_log_pool() {
                    Self::perform_rollback(log_pool.base_address)?;
                    Self::empty_log_pool(log_pool.base_address);
                }
                Err(e)
            }
        }
    }

    fn recover(&mut self) -> Result<(), PoolError> {
        unsafe {
            let table_base = (self.base_address + (*self.header).object_table_offset)
                as *mut ObjectTableEntry;

            for i in 0..(*self.header).max_objects {
                let entry = &mut *table_base.add(i);
                //DOC: Hier kann ich einfach setzen, denn das letze was passiert ist war das active gesetzt wurde
                // Aktueller Stand:
                // !Active | !Valid |  OperationDone -> Normaler Dealloc
                // !Active |  Valid |  OperationDone -> Testen ob unnötig
                //  Active | !Valid |  OperationDone -> "Fastforward" Hier ist die Forschleife aus pendingchanges nicht fertig druchgelaufen!
                // !Active | !Valid | !OperationDone -> Rollback von Allocate
                //  Active |  Valid |  OperationDone -> Alles super -> nothing happens

                if !entry.active.load(Ordering::Acquire) && entry.valid.load(Ordering::Acquire) && entry.operation_done.load(Ordering::Acquire) {
                    //info!("Regular Deallocation");
                    //TODO: EVLT IST DIESE FN useless??

                    Self::deallocate(self, entry.data.unwrap(), Layout::from_size_align(entry.type_size, 64).unwrap());
                    entry.active.store(false, Ordering::Release);

                    // Clear all entry data
                    entry.data = None;
                    entry.type_hash = 0;
                    entry.type_size = 0;
                    ptr::write_bytes(
                        entry.id.as_mut_ptr(),
                        0,
                        32
                    );
                    entry.id_len = 0;

                    // Mark entry as inactive
                    entry.active.store(false, Ordering::Release);
                    entry.valid.store(false, Ordering::Release);
                    entry.operation_done.store(false, Ordering::Release);

                }

                if !entry.active.load(Ordering::Acquire) && !entry.valid.load(Ordering::Acquire) && entry.operation_done.load(Ordering::Acquire) {
                    //info!(">> LOG HAS FAILED << !DANGEROUS STATE!");

                    Self::deallocate(self, entry.data.unwrap(), Layout::from_size_align(entry.type_size, 64).unwrap());
                    entry.active.store(false, Ordering::Release);

                    // Clear all entry data
                    entry.data = None;
                    entry.type_hash = 0;
                    entry.type_size = 0;
                    ptr::write_bytes(
                        entry.id.as_mut_ptr(),
                        0,
                        32
                    );
                    entry.id_len = 0;

                    // Mark entry as inactive
                    entry.active.store(false, Ordering::Release);
                    entry.valid.store(false, Ordering::Release);
                    entry.operation_done.store(false, Ordering::Release);

                }

                if entry.active.load(Ordering::Acquire) && !entry.valid.load(Ordering::Acquire) {


                    if entry.operation_done.load(Ordering::Acquire) {
                        //info!("Recovering object with ID: {} fast-forward", core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]));
                        entry.valid.store(true, Ordering::Release);
                    }
                    else {

                        //wert deaktivieren... kann nur neu gemacht werden über allocate!
                        //info!(">> Data has been Rollbacked");

                        match entry.data {
                            Some(data) => {
                                // If data was allocated, deallocate it
                                Self::deallocate(self, data, Layout::from_size_align(entry.type_size, 64).unwrap());
                            },
                            None => {
                                // If no data was allocated, just continue with cleanup
                                // No need to deallocate
                            }
                        }

                        //Self::deallocate(self, entry.data.unwrap(), Layout::from_size_align(entry.type_size, 64).unwrap());
                        entry.active.store(false, Ordering::Release);

                        // Clear all entry data
                        entry.data = None;
                        entry.type_hash = 0;
                        entry.type_size = 0;
                        ptr::write_bytes(
                            entry.id.as_mut_ptr(),
                            0,
                            32
                        );
                        entry.id_len = 0;

                        // Mark entry as inactive
                        entry.active.store(false, Ordering::Release);
                        entry.valid.store(false, Ordering::Release);
                        entry.operation_done.store(false, Ordering::Release);

                        //Pool::flush_cache_line(entry as *const u8);
                    }

                }
            }
            Ok(())
        }
    }

    pub(crate) fn perform_rollback(pool_base: u64) -> Result<(), PoolError> {
        unsafe {

            let header = pool_base as *const PoolHeader;


            let table_base = (pool_base + (*header).object_table_offset)
                as *mut ObjectTableEntry;

            // Process logs in reverse order
            for i in (0..MAX_OBJECT_ENTRIES).rev() {
                let entry = &*table_base.add(i);
                if !entry.active.load(Ordering::Acquire) ||
                    !entry.valid.load(Ordering::Acquire) {
                    continue;
                }



                if let Some(data_ptr) = entry.data {
                    let logged_op = &*(data_ptr.as_ptr() as *const LoggedOperation);
                    let original_data = (data_ptr.as_ptr() as *const u8).add(mem::size_of::<LoggedOperation>());

                    //info!("Rolling back operation {}", logged_op.op_type as u8);

                    match logged_op.op_type {
                        OperationType::Allocation => {
                            // For allocation rollback, we need to:
                            // 1. Find the entry by offset
                            // 2. Mark it as inactive and invalid
                            // 3. Deallocate the memory
                            //info!("Trying to find entry that is affected by the allocation at 0x{:?}", logged_op.pool_base_address);

                            if let Ok(target_entry) = Self::find_entry_by_offset(logged_op.absolute_address,
                                                                                 logged_op.pool_base_address) {

                                (*target_entry).active.store(true, Ordering::Release);
                                (*target_entry).valid.store(false, Ordering::Release);
                                (*target_entry).operation_done.store(false, Ordering::Release);

                                Pool::flush_cache_line(target_entry as *const u8);
                             }
                        },
                        OperationType::Modification => {
                            // Restore original data using the offset
                            ptr::copy_nonoverlapping(
                                original_data,
                                (logged_op.absolute_address) as *mut u8,
                                logged_op.data_size
                            );

                            Pool::flush_cache_line((logged_op.absolute_address) as *const u8);

                            if let Ok(target_entry) = Self::find_entry_by_offset(logged_op.absolute_address,
                                                                                 logged_op.pool_base_address) {
                                (*target_entry).active.store(true, Ordering::Release);
                                (*target_entry).valid.store(true, Ordering::Release);
                                (*target_entry).operation_done.store(true, Ordering::Release);
                                Pool::flush_cache_line(target_entry as *const u8);
                            }


                        },
                        OperationType::Deallocation => {
                            // For deallocation rollback:
                            // 1. Find entry which is false | false | true
                            // 2. acitve entry back

                            let header = logged_op.pool_base_address as *const PoolHeader;

                            let table_base = (logged_op.pool_base_address + (*header).object_table_offset)
                                as *mut ObjectTableEntry;

                            for i in 0..MAX_OBJECT_ENTRIES {
                                let entry = &mut *table_base.add(i);
                                if !entry.active.load(Ordering::Acquire) && !entry.valid.load(Ordering::Acquire) && entry.operation_done.load(Ordering::Acquire) {
                                    entry.active.store(true, Ordering::Release);
                                    entry.valid.store(true, Ordering::Release);
                                    //Pool::flush_cache_line(entry as *const u8);
                                    break;
                                }
                            }

                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn get_pool(base_address: u64) -> &'static mut Pool {
        unsafe {
            &mut *(base_address as *mut Pool)
        }
    }

    unsafe fn find_entry_by_offset(absolute_addr: u64, pool_base: u64) -> Result<*mut ObjectTableEntry, PoolError> {
        // Use the pool_base to find the correct pool
        let header = pool_base as *const PoolHeader;


        let table_base = (pool_base + (*header).object_table_offset)
            as *mut ObjectTableEntry;

        // Search through entries
        for i in 0..MAX_OBJECT_ENTRIES{
            let entry = &mut *table_base.add(i);
            if let Some(data) = entry.data {
                //info!("Checking entry at index {} with data at 0x{:x}", i, data.as_ptr() as u64);
                if data.as_ptr() as u64 == absolute_addr {
                    return Ok(table_base.add(i));
                }
            }
        }

        Err(PoolError::InvalidId)
    }


    pub(crate) fn get_log_pool(&self) -> Option<&mut Pool> {
        if self.is_log_pool() {
            return None;
        }

        // Safety: We ensure single-threaded access in our early-stage OS
        unsafe { LOG_POOL.as_mut() }
    }

    pub(crate) fn init_log_pool(base_address: u64) {
        unsafe {
            LOG_POOL = Some(Pool::new(base_address, FIXED_POOL_SIZE));
        }
    }

    // Helper to check if log pool is initialized
    pub fn is_log_pool_initialized() -> bool {
        unsafe { LOG_POOL.is_some() }
    }

    fn is_log_pool(&self) -> bool {
        unsafe {
            let table_base = (self.base_address + (*self.header).object_table_offset)
                as *const ObjectTableEntry;

            // Check the first entry for LOG_POOL_NAME
            let entry = &*table_base;
            if entry.active.load(Ordering::Acquire) {
                let entry_id = core::str::from_utf8_unchecked(
                    &entry.id[..entry.id_len as usize]
                );
                return entry_id == core::str::from_utf8_unchecked(LOG_POOL_NAME);
            }
            false
        }
    }

    unsafe fn allocate_raw(&self, size: usize) -> Result<(*mut u8, *mut ObjectTableEntry), PoolError> {
        //info!("Allocating {} bytes in pool at 0x{:x}", size, self.base_address);

        let table_base = (self.base_address + self.object_table_offset) as *mut ObjectTableEntry;

        // Find free entry
        for i in 0..(*self.header).max_objects {
            let entry = &mut *table_base.add(i);
            if !entry.active.load(Ordering::Acquire) {
                // Allocate memory
                let layout = Layout::from_size_align(size, 8)?;

                match self.heap.lock().allocate_first_fit(layout) {
                    Ok(ptr) => {
                        // Setup entry
                        entry.active.store(true, Ordering::Release);
                        entry.valid.store(true, Ordering::Release);
                        entry.data = Some(ptr);
                        entry.type_size = size;
                        //info!("Allocation successful at 0x{:x}", ptr.as_ptr() as u64);
                        //info!("Safed in entry at 0x{:x}", entry as *const _ as u64);
                        return Ok((ptr.as_ptr(), entry));
                    },
                    Err(e) => {
                        //info!("Allocation failed in pool at 0x{:x}: {:?}", self.base_address, e);
                        return Err(PoolError::LogPoolFull);
                    }
                }
            }
        }

        //info!("No free entries found in pool at 0x{:x}", self.base_address);
        Err(PoolError::ObjectTableFull)
    }

    fn log_operation(&self, op_type: OperationType, address: u64, data: *const u8, size: usize, type_hash: u64) -> Result<(), PoolError> {
        let log_pool = self.get_log_pool()
            .ok_or(PoolError::LogPoolNotAvailable)?;

        unsafe {
            //info!("Logging operation at address 0x{:x}", address);
            let total_size = size_of::<LoggedOperation>() + size;
            let (ptr, _entry) = log_pool.allocate_raw(total_size)?;
            //info!("Raw Allocation done");

            // Initialize log entry
            let log_entry = &mut *(ptr as *mut LoggedOperation);
            *log_entry = LoggedOperation {
                op_type,
                pool_base_address: self.base_address,
                absolute_address: address,
                data_size: size,
                type_hash,
            };
            Pool::flush_cache_line(ptr as *const u8);
            Pool::flush_cache_line(ptr.add(64) as *const u8);  // In case entry spans cache lines


            // Copy original data after the LoggedOperation structure
            if !data.is_null() {
                ptr::copy_nonoverlapping(
                    data,
                    ptr.add(size_of::<LoggedOperation>()),
                    size
                );
            }

            Self::flush_cache_line(ptr as *const u8);
            if !data.is_null() {
                Self::flush_cache_line(ptr.add(size_of::<LoggedOperation>()));
            }
        }
        Ok(())
    }

    fn log_allocation(&self, address: u64, size: usize, type_hash: u64) -> Result<(), PoolError> {
        self.log_operation(OperationType::Allocation, address, ptr::null(), size, type_hash)
    }

    fn log_modification(&self, address: u64, data: *const u8, size: usize, type_hash: u64) -> Result<(), PoolError> {
        self.log_operation(OperationType::Modification, address, data, size, type_hash)
    }

    fn log_deallocation(&self, address: u64, size: usize, type_hash: u64) -> Result<(), PoolError> {
        self.log_operation(OperationType::Deallocation, address, ptr::null(), size, type_hash)
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
            core::arch::x86_64::_mm_sfence();
            core::arch::x86_64::_mm_clflush(ptr);
            core::arch::x86_64::_mm_sfence();
        }
    }

    pub fn empty_pool(pool_base: u64) {
        //info!("Emptying pool at 0x{:x}", self.base_address);
        unsafe {

            let header = pool_base as *const PoolHeader;
            let table_base = (pool_base + (*header).object_table_offset)
                as *mut ObjectTableEntry;

            for i in 0..MAX_OBJECT_ENTRIES {
                if (*table_base.add(i)).active.load(Ordering::Acquire) {
                    let entry = &mut *table_base.add(i);
                    entry.data = None;
                    entry.type_hash = 0;
                    entry.type_size = 0;
                    ptr::write_bytes(
                        entry.id.as_mut_ptr(),
                        0,
                        32
                    );
                    entry.id_len = 0;

                    // Zero out the entire entry to ensure complete clearing
                    ptr::write_bytes(
                        entry as *mut ObjectTableEntry as *mut u8,
                        0,
                        mem::size_of::<ObjectTableEntry>()
                    );

                    Pool::flush_cache_line(entry as *mut ObjectTableEntry as *const ObjectTableEntry as *const u8);
                }
            }

            //Fully Zero the logpoolHeap
            let heap_offset = (*header).heap_start;
            let heap_size = (*header).heap_size;

            ptr::write_bytes(
                (pool_base + heap_offset) as *mut u8,
                0,
                heap_size
            );
        }
    }

    //pub fn empty_log_pool(&mut self) {
    pub fn empty_log_pool(pool_base: u64) {
        //info!("Emptying log pool at 0x{:x}", pool_base);
        unsafe {

            let header = pool_base as *const PoolHeader;
            let table_base = (pool_base + (*header).object_table_offset)
                as *mut ObjectTableEntry;

            for i in 0..MAX_OBJECT_ENTRIES {
                if (*table_base.add(i)).active.load(Ordering::Acquire) {
                    let entry = &mut *table_base.add(i);
                    entry.data = None;
                    entry.type_hash = 0;
                    entry.type_size = 0;
                    ptr::write_bytes(
                        entry.id.as_mut_ptr(),
                        0,
                        32
                    );
                    entry.id_len = 0;

                    // Zero out the entire entry to ensure complete clearing
                    ptr::write_bytes(
                        entry as *mut ObjectTableEntry as *mut u8,
                        0,
                        mem::size_of::<ObjectTableEntry>()
                    );

                    Pool::flush_cache_line(entry as *mut ObjectTableEntry as *const ObjectTableEntry as *const u8);
                }
            }


            //Fully Zero the logpoolHeap
            let heap_offset = (*header).heap_start;
            let heap_size = (*header).heap_size;

            ptr::write_bytes(
                (pool_base + heap_offset) as *mut u8,
                0,
                heap_size
            );

            //qemu_exit(123);

            Self::init_log_pool(pool_base);

        }
    }

    unsafe fn deallocate(&mut self, ptr: NonNull<u8>, layout: Layout) {
        self.heap.lock().deallocate(ptr, layout);
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
            info!("  - Used space: {} bytes", header.used_space.load(Ordering::Acquire));
        }
    }

    pub fn debug_print_object_table(&self) {
        unsafe {
            let table_base = (self.base_address + (*self.header).object_table_offset)
                as *const ObjectTableEntry;

            info!("=== Object Table Debug Information ===");
            info!("Object Table Location: 0x{:x}", table_base as u64);


            for i in 0..MAX_OBJECT_ENTRIES {
            //for i in 0..10 {
                let entry = &*table_base.add(i);
                //if entry.active.load(Ordering::Acquire) {
                    let id_str = core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]);
                    info!("Entry #{}", i);
                    info!("  Active: {}", entry.active.load(Ordering::Acquire));
                    info!("  Valid: {}", entry.valid.load(Ordering::Acquire));
                    info!("  Operation Done: {}", entry.operation_done.load(Ordering::Acquire));
                    info!("  ID: {}", id_str);
                    info!("  Type Hash: 0x{:x}", entry.type_hash);
                    info!("  Type Size: {} bytes", entry.type_size);
                    info!("  Data Pointer: {:?}", entry.data);
                //}
            }
        }
    }

    pub fn debug_log_pool_state(&self) {
        if let Some(log_pool) = self.get_log_pool() {
            info!("=== Log Pool State ===");
            info!("Log Pool Base Address: 0x{:x}", log_pool.base_address);

            unsafe {
                let table_base = (log_pool.base_address + log_pool.object_table_offset)
                    as *const ObjectTableEntry;

                let mut active_entries = 0;
                for i in 0..(*log_pool.header).max_objects {
                    let entry = &*table_base.add(i);
                    if entry.active.load(Ordering::Acquire) {
                        active_entries += 1;
                        info!("Active Entry #{}: {:?}", i, entry.data);
                    }
                }
                info!("Total Active Entries: {}", active_entries);
            }
            info!("====================");
        } else {
            info!("No log pool available or this is the log pool");
        }
    }
}

//DOC: Wichtig für Thesis später
// Track changes within a transaction
// THIS IS ON THE DRAM !!!!!!!
struct PendingChange {
    entry: *mut ObjectTableEntry,
    data_ptr: NonNull<u8>,
}

pub struct TransactionContext<'a> {
    pool: &'a mut Pool,
    pending_changes: Vec<PendingChange>,
}

impl<'a> TransactionContext<'a> {

    pub fn allocate_with_id<T: Copy + 'static>(&mut self, id: &str, data: T) -> Result<NonNull<T>, PoolError> {

        if id.len() > 32 {
            return Err(PoolError::InvalidId);
        }

        unsafe {
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset)
                as *mut ObjectTableEntry;

            let mut free_entry = None;

            // Single pass through table
            for i in 0..(*self.pool.header).max_objects {
                let entry = &mut *table_base.add(i);

                // Check if this is the ID we're looking for
                if entry.active.load(Ordering::Acquire) && entry.valid.load(Ordering::Acquire) {
                    let entry_id = core::str::from_utf8_unchecked(
                        &entry.id[..entry.id_len as usize]
                    );

                    if entry_id == id {
                        // Found existing entry - modify it
                        self.modify(entry.data.unwrap().cast(), |existing| *existing = data).expect("Modify failed");
                        return Ok(entry.data.unwrap().cast());
                    }
                } else if free_entry.is_none() && !entry.active.load(Ordering::Acquire) {
                    // Found first free entry
                    free_entry = Some(entry);
                }
            }

            // Allocate new entry
            let entry = free_entry.ok_or(PoolError::ObjectTableFull)?;

            // Prepare entry metadata first (without flushing)
            entry.active.store(true, Ordering::Release);
            entry.valid.store(false, Ordering::Release);
            entry.operation_done.store(false, Ordering::Release);
            entry.id[..id.len()].copy_from_slice(id.as_bytes());
            entry.id_len = id.len() as u8;
            entry.type_hash = Pool::compute_type_hash::<T>();
            entry.type_size = mem::size_of::<T>();

            Pool::flush_cache_line(entry as *const _ as *const u8);

            let start1 = unsafe { _rdtsc() };
            let ptr = self.pool.heap.lock()
                .allocate_first_fit(Layout::new::<T>())
                .map_err(|_| PoolError::AllocationFailed)?;
            let end1 = unsafe { _rdtsc() };
            let alloc = end1 - start1;
            //info!("Allocation of LockedHeap took {} cycles", end - start);


            //DOC: Log the allocation BEFORE making it visible
            //info!("Starting to Allocate in Log");
            let start2 = unsafe { _rdtsc() };
            self.pool.log_allocation(
                ptr.as_ptr() as u64,
                mem::size_of::<T>(),
                Pool::compute_type_hash::<T>()
            )?;
            let end2 = unsafe { _rdtsc() };
            let log_alloc = end2 - start2;
            //info!("Offset: 0x{:x}", (ptr.as_ptr() as u64) - self.pool.base_address);
            //DOC: END

            //info!("Allocation in Log done");

            let start3 = unsafe { _rdtsc() };
            entry.data = Some(ptr);
            ptr::write(ptr.as_ptr() as *mut T, data);
            let end3 = unsafe { _rdtsc() };
            let write = end3 - start3;

            let start4 = unsafe { _rdtsc() };
            Pool::flush_cache_line(ptr.as_ptr() as *const u8);
            let end4 = unsafe { _rdtsc() };
            let flush = end4 - start4;

            //info!("Allocation: {} cycles, Log: {} cycles, Write: {} cycles, Flush: {} cycles", alloc, log_alloc, write, flush);


            // Mark as operation but not yet valid (valid happens at transaction commit)
            entry.operation_done.store(true, Ordering::Release);
            Pool::flush_cache_line(entry as *const _ as *const u8);

            self.pending_changes.push(PendingChange {
                entry,
                data_ptr: ptr,
            });
            Ok(ptr.cast())
        }
    }

    pub fn get_by_id<T: Copy + 'static>(&self, id: &str) -> Result<NonNull<T>, PoolError> {
        unsafe {
            //info!("Looking for ID: {}", id);
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset)
                as *const ObjectTableEntry;

            for i in 0..(*self.pool.header).max_objects {
                let entry = &*table_base.add(i);

                // Only consider entries that are both active AND valid
                if !entry.active.load(Ordering::Acquire) || !entry.valid.load(Ordering::Acquire) {
                    continue;
                }

                //info!("Checking entry with ID: {}", core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]));


                let entry_id = core::str::from_utf8_unchecked(
                    &entry.id[..entry.id_len as usize]
                );

                if entry_id == id {
                    // Verify type matches
                    let expected_hash = Pool::compute_type_hash::<T>();
                    if entry.type_hash != expected_hash {
                        return Err(PoolError::TypeMismatch {
                            expected: type_name::<T>(),
                            actual: "unknown",
                        });
                    }

                    return Ok(entry.data.unwrap().cast());
                }
            }
            //info!("ID not found");
            Err(PoolError::InvalidId)
        }
    }

    pub fn read_by_id<T: Copy + 'static>(&self, id: &str) -> Result<T, PoolError> {
        let ptr = self.get_by_id::<T>(id)?;
        unsafe {
            Ok(*ptr.as_ref())
        }
    }

    pub fn modify<T: Copy+ 'static>(
        &mut self,
        ptr: NonNull<T>,
        f: impl FnOnce(&mut T),
    ) -> Result<(), PoolError> {
        unsafe {
            // Find corresponding entry
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset)
                as *mut ObjectTableEntry;

            let mut entry_ptr = None;
            for i in 0..(*self.pool.header).max_objects {
                let entry = &mut *table_base.add(i);
                if entry.active.load(Ordering::Acquire) &&
                    entry.valid.load(Ordering::Acquire) &&
                    entry.data.map_or(false, |p| p.as_ptr() == ptr.as_ptr() as *mut u8)
                {
                    entry_ptr = Some(entry as *mut ObjectTableEntry);
                    break;
                }
            }

            //Also check if entry was found in pending since it could be created in the same commit! or was modified before!
            if entry_ptr.is_none() {
                for change in &self.pending_changes {
                    if change.data_ptr.as_ptr() == ptr.as_ptr() as *mut u8 {
                        entry_ptr = Some(change.entry);
                        break;
                    }
                }
            }

            let entry = entry_ptr.ok_or(PoolError::InvalidId)?;


            // Mark as active but not valid during modification
            // Also mark the operation done
            //(*entry).valid.store(false, Ordering::Release);
            (*entry).operation_done.store(false, Ordering::Release);
            Pool::flush_cache_line(entry as *const _ as *const u8);

            //DOC: Log the modification BEFORE making it visible
            //info!("Starting to Modify in Log");
            self.pool.log_modification(
                ptr.as_ptr() as u64, ptr.as_ptr() as *const u8,
                mem::size_of::<T>(),
                Pool::compute_type_hash::<T>()
            )?;
            //DOC: END
            //info!("Modification in Log done");


            // Modify data
            //DOC: Wird einfach überschrieben..
            f(&mut *ptr.as_ptr());
            //TODO: HIER NOCHMAL über cache flush nachdenken!


            (*entry).operation_done.store(true, Ordering::Release);
            Pool::flush_cache_line(ptr.as_ptr() as *const u8);


            // Add to pending changes if not already there
            if !self.pending_changes.iter().any(|c| c.entry == entry) {
                self.pending_changes.push(PendingChange {
                    entry,
                    data_ptr: ptr.cast(),
                });
            }

            Ok(())
        }
    }


    pub fn deallocate_by_id(&mut self, id: &str) -> Result<(), PoolError> {
        unsafe {
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset)
                as *mut ObjectTableEntry;

            for i in 0..(*self.pool.header).max_objects {
                let entry = &mut *table_base.add(i);

                let entry_id = core::str::from_utf8_unchecked(
                    &entry.id[..entry.id_len as usize]
                );

                if entry_id == id {
                    // First deallocate the memory
                    entry.operation_done.store(false, Ordering::Release);


                    if let Some(ptr) = entry.data {

                    //DOC: Log the deallocation BEFORE making it visible
                        self.pool.log_deallocation(
                            ptr.as_ptr() as u64,
                            entry.type_size,
                            entry.type_hash
                        )?;
                    //
                    // //DOC: END

                        //DOC: habe ich hier rausgenommen -> passiert erst in recovery methode
                        //Real deallocation
                        // self.pool.heap.lock().deallocate(
                        //     ptr,
                        //     Layout::from_size_align(entry.type_size, 8)
                        //         .map_err(|_| PoolError::AllocationFailed)?
                        // );


                    }

                    // // Clear all entry data
                    // entry.data = None;
                    // entry.type_hash = 0;
                    // entry.type_size = 0;
                    // ptr::write_bytes(
                    //     entry.id.as_mut_ptr(),
                    //     0,
                    //     32
                    // );
                    // entry.id_len = 0;
                    // Mark entry as inactive
                    entry.active.store(false, Ordering::Release);
                    entry.valid.store(false, Ordering::Release);
                    entry.operation_done.store(true, Ordering::Release);
                    //info!("Deallocation done");

                    //Pool::flush_cache_line(entry as *const u8);

                    //remove from pending if was in there:
                    self.pending_changes.retain(|c| c.entry != entry);



                    // self.pending_changes.push(PendingChange {
                    //     entry,
                    //     data_ptr: NonNull::dangling(),
                    // });

                    return Ok(());

                }
            }
            Err(PoolError::InvalidId)
        }
    }
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

use crate::memory;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::any::type_name;
use core::mem;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use linked_list_allocator::LockedHeap;
use log::info;
use memory::global_persistent_allocator::{FIXED_POOL_SIZE, LOG_POOL_NAME};

static mut LOG_POOL: Option<Pool> = None;

const POOL_MAGIC: u64 = 0x4433_4F53_504F4F4C; // "D3OS_POOL" in hex
const MAX_OBJECT_ENTRIES: usize = 256; //~88B*1024 for table

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
    valid: AtomicBool,          // Indicates if the data is valid (was written)
    active: AtomicBool,         // Indicates if the entry is currently in use
    operation_done: AtomicBool, // Indicates if the current operation is complete
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
    heap_start: u64,
    heap_size: usize,
}

#[derive(Debug)]
pub enum PoolError {
    AllocationFailed,
    TransactionFailed,
    TypeMismatch { expected: &'static str, actual: &'static str },
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
    pub fn heap_size(&self) -> usize {
        unsafe { (*self.header).heap_size }
    }

    /// Creates a new memory pool at the specified address with given size.
    /// Initializes pool metadata and heap structures.
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
                ptr::write(
                    header,
                    PoolHeader {
                        magic: POOL_MAGIC,
                        size,
                        max_objects: MAX_OBJECT_ENTRIES,
                        object_table_offset,
                        heap_start: heap_offset,
                        heap_size,
                    },
                );

                Self::flush_cache_line(header as *const _ as *const u8);
            }

            pool.heap.lock().init((base + heap_offset) as *mut u8, heap_size);
        }
        //pool.print_metadata_debug_info();
        pool
    }

    /// Executes operations within a transaction context, ensuring atomicity.
    /// All operations within the transaction are either completely applied or rolled back.
    ///
    /// # Arguments
    /// * `f` - Closure containing transaction operations
    ///
    /// # Returns
    /// * `Ok(R)` - Transaction result on success
    /// * `Err(PoolError)` - If transaction fails
    ///
    /// # Example
    /// ```rust
    /// struct MyData { value: u32 }
    ///
    /// // Execute multiple operations atomically
    /// pool.transaction(|ctx| {
    ///     // Allocate new data
    ///     let ptr1 = ctx.allocate_with_id("data1", MyData { value: 42 })?;
    ///
    ///     // Modify existing data
    ///     if let Ok(ptr2) = ctx.get_by_id::<MyData>("existing_data") {
    ///         ctx.modify(ptr2, |data| {
    ///             data.value += 1;
    ///         })?;
    ///     }
    ///
    ///     // Delete old data
    ///     ctx.deallocate_by_id("old_data")?;
    ///
    ///     Ok(()) // Commit transaction
    /// })?;
    /// ```
    ///
    /// If any operation fails, all changes are automatically rolled back.
    /// The log pool is used to ensure consistency in case of system crashes.
    pub fn transaction<F, R>(&mut self, f: F) -> Result<R, PoolError>
    where
        F: FnOnce(&mut TransactionContext) -> Result<R, PoolError>,
    {
        if let Some(log_pool) = self.get_log_pool() {
            let has_logs = unsafe {
                let table_base = (log_pool.base_address + log_pool.object_table_offset) as *const ObjectTableEntry;

                (0..(*log_pool.header).max_objects).any(|i| {
                    let entry = &*table_base.add(i);
                    entry.active.load(Ordering::Acquire) && entry.valid.load(Ordering::Acquire)
                })
            };

            if has_logs {
                info!("Systemcrash happened inside a Rollback .. Continue with rollback");
                Self::perform_rollback(log_pool.base_address)?;
                Self::empty_log_pool(log_pool.base_address);
            }
        }

        // Always check for recovery at start of transaction
        // The de allocation happens here
        self.recover()?;

        //Consumes the closure
        let mut ctx = TransactionContext {
            pool: self,
            pending_changes: Vec::new(),
        };

        match f(&mut ctx) {
            Ok(result) => {
                // Activate all changes
                for change in ctx.pending_changes {
                    unsafe {
                        // mark entry valid at the end of transaction
                        (*change.entry).valid.store(true, Ordering::Release);
                        Self::flush_cache_line(change.entry as *const u8);
                    }
                }
                // Clear log pool after successful transaction
                if let Some(log_pool) = self.get_log_pool() {
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

    // !Active | !Valid |  OperationDone -> Normaler Dealloc
    // !Active |  Valid |  OperationDone -> Unmöglich
    //  Active | !Valid |  OperationDone -> "Fastforward" Hier ist die Forschleife aus pendingchanges nicht fertig druchgelaufen!
    // !Active | !Valid | !OperationDone -> Rollback von Allocate
    //  Active |  Valid |  OperationDone -> Alles super -> nothing happens
    fn recover(&mut self) -> Result<(), PoolError> {
        unsafe {
            let table_base = (self.base_address + (*self.header).object_table_offset) as *mut ObjectTableEntry;

            for i in 0..(*self.header).max_objects {
                let entry = &mut *table_base.add(i);

                //impossible state!!! -> catching it because to dangerous
                if !entry.active.load(Ordering::Acquire) && entry.valid.load(Ordering::Acquire) && entry.operation_done.load(Ordering::Acquire) {
                    Self::deallocate(self, entry.data.unwrap(), Layout::from_size_align(entry.type_size, 64).unwrap());
                    entry.active.store(false, Ordering::Release);

                    // Clear all entry data
                    entry.data = None;
                    entry.type_hash = 0;
                    entry.type_size = 0;
                    ptr::write_bytes(entry.id.as_mut_ptr(), 0, 32);
                    entry.id_len = 0;

                    // Mark entry as inactive
                    entry.active.store(false, Ordering::Release);
                    entry.valid.store(false, Ordering::Release);
                    entry.operation_done.store(false, Ordering::Release);
                }

                if !entry.active.load(Ordering::Acquire) && !entry.valid.load(Ordering::Acquire) && entry.operation_done.load(Ordering::Acquire) {
                    Self::deallocate(self, entry.data.unwrap(), Layout::from_size_align(entry.type_size, 64).unwrap());

                    // Clear all entry data
                    entry.data = None;
                    entry.type_hash = 0;
                    entry.type_size = 0;
                    ptr::write_bytes(entry.id.as_mut_ptr(), 0, 32);
                    entry.id_len = 0;

                    Self::flush_cache_line(entry as *mut ObjectTableEntry as *const ObjectTableEntry as *const u8);

                    entry.operation_done.store(false, Ordering::Release);
                }

                if entry.active.load(Ordering::Acquire) && !entry.valid.load(Ordering::Acquire) {
                    if entry.operation_done.load(Ordering::Acquire) {
                        //info!("Recovering object with ID: {} fast-forward", core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]));
                        entry.valid.store(true, Ordering::Release);
                    } else {
                        match entry.data {
                            Some(data) => {
                                // If data was allocated, deallocate it
                                Self::deallocate(self, data, Layout::from_size_align(entry.type_size, 64).unwrap());
                            }
                            None => {
                                // If no data was allocated, just continue with cleanup
                                // No need to deallocate
                            }
                        }

                        entry.active.store(false, Ordering::Release);

                        // Clear all entry data
                        entry.data = None;
                        entry.type_hash = 0;
                        entry.type_size = 0;
                        ptr::write_bytes(entry.id.as_mut_ptr(), 0, 32);
                        entry.id_len = 0;
                        Self::flush_cache_line(entry as *mut ObjectTableEntry as *const ObjectTableEntry as *const u8);

                        // Mark entry as inactive
                        entry.operation_done.store(false, Ordering::Release);
                    }
                }
            }
            Ok(())
        }
    }

    pub(crate) fn perform_rollback(pool_base: u64) -> Result<(), PoolError> {
        unsafe {
            let header = pool_base as *const PoolHeader;

            let table_base = (pool_base + (*header).object_table_offset) as *mut ObjectTableEntry;

            // Process logs in reverse order
            for i in (0..MAX_OBJECT_ENTRIES).rev() {
                let entry = &*table_base.add(i);
                if !entry.active.load(Ordering::Acquire) || !entry.valid.load(Ordering::Acquire) {
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

                            if let Ok(target_entry) = Self::find_entry_by_offset(logged_op.absolute_address, logged_op.pool_base_address) {
                                (*target_entry).active.store(true, Ordering::Release);
                                (*target_entry).valid.store(false, Ordering::Release);
                                (*target_entry).operation_done.store(false, Ordering::Release);

                                Pool::flush_cache_line(target_entry as *const u8);
                            }
                        }
                        OperationType::Modification => {
                            // Restore original data using the offset
                            ptr::copy_nonoverlapping(original_data, (logged_op.absolute_address) as *mut u8, logged_op.data_size);

                            Pool::flush_cache_line((logged_op.absolute_address) as *const u8);

                            if let Ok(target_entry) = Self::find_entry_by_offset(logged_op.absolute_address, logged_op.pool_base_address) {
                                (*target_entry).active.store(true, Ordering::Release);
                                (*target_entry).valid.store(true, Ordering::Release);
                                (*target_entry).operation_done.store(true, Ordering::Release);
                                Pool::flush_cache_line(target_entry as *const u8);
                            }
                        }
                        OperationType::Deallocation => {
                            // For deallocation rollback:
                            // 1. Find entry which is false | false | true
                            // 2. acitve entry back

                            let header = logged_op.pool_base_address as *const PoolHeader;

                            let table_base = (logged_op.pool_base_address + (*header).object_table_offset) as *mut ObjectTableEntry;

                            for i in 0..MAX_OBJECT_ENTRIES {
                                let entry = &mut *table_base.add(i);
                                if !entry.active.load(Ordering::Acquire) && !entry.valid.load(Ordering::Acquire) && entry.operation_done.load(Ordering::Acquire)
                                {
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

    unsafe fn find_entry_by_offset(absolute_addr: u64, pool_base: u64) -> Result<*mut ObjectTableEntry, PoolError> {
        // Use the pool_base to find the correct pool
        let header = pool_base as *const PoolHeader;
        unsafe {
            let table_base = (pool_base + (*header).object_table_offset) as *mut ObjectTableEntry;

            // Search through entries
            for i in 0..MAX_OBJECT_ENTRIES {
                let entry = &mut *table_base.add(i);
                if let Some(data) = entry.data {
                    //info!("Checking entry at index {} with data at 0x{:x}", i, data.as_ptr() as u64);
                    if data.as_ptr() as u64 == absolute_addr {
                        return Ok(table_base.add(i));
                    }
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
            let table_base = (self.base_address + (*self.header).object_table_offset) as *const ObjectTableEntry;

            // Check the first entry for LOG_POOL_NAME
            let entry = &*table_base;
            if entry.active.load(Ordering::Acquire) {
                let entry_id = core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]);
                return entry_id == core::str::from_utf8_unchecked(LOG_POOL_NAME);
            }
            false
        }
    }

    //this is only for the log_pool
    unsafe fn allocate_raw(&self, size: usize) -> Result<(*mut u8, *mut ObjectTableEntry), PoolError> {
        //info!("Allocating {} bytes in pool at 0x{:x}", size, self.base_address);

        let table_base = (self.base_address + self.object_table_offset) as *mut ObjectTableEntry;

        unsafe {
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
                            return Ok((ptr.as_ptr(), entry));
                        }
                        Err(_) => {
                            //info!("Allocation failed in pool at 0x{:x}: {:?}", self.base_address, e);
                            return Err(PoolError::LogPoolFull);
                        }
                    }
                }
            }
        }

        //info!("No free entries found in pool at 0x{:x}", self.base_address);
        Err(PoolError::ObjectTableFull)
    }

    fn log_operation(&self, op_type: OperationType, address: u64, data: *const u8, size: usize, type_hash: u64) -> Result<(), PoolError> {
        let log_pool = self.get_log_pool().ok_or(PoolError::LogPoolNotAvailable)?;

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
            Pool::flush_cache_line(ptr.add(64) as *const u8); // In case entry spans cache lines

            // Copy original data after the LoggedOperation structure
            if !data.is_null() {
                ptr::copy_nonoverlapping(data, ptr.add(size_of::<LoggedOperation>()), size);
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
            hash = ((hash << 5).wrapping_add(hash)).wrapping_add(*byte as u64);
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

    /// Empties a pool by clearing all its entries and heap memory.
    /// Used during pool release and recovery operations.
    ///
    /// # Arguments
    /// * `pool_base` - Base address of the pool to empty
    pub fn empty_pool(pool_base: u64) {
        //info!("Emptying pool at 0x{:x}", self.base_address);
        unsafe {
            let header = pool_base as *const PoolHeader;
            let table_base = (pool_base + (*header).object_table_offset) as *mut ObjectTableEntry;

            for i in 0..MAX_OBJECT_ENTRIES {
                if (*table_base.add(i)).active.load(Ordering::Acquire) {
                    let entry = &mut *table_base.add(i);
                    entry.data = None;
                    entry.type_hash = 0;
                    entry.type_size = 0;
                    ptr::write_bytes(entry.id.as_mut_ptr(), 0, 32);
                    entry.id_len = 0;

                    // Zero out the entire entry to ensure complete clearing
                    ptr::write_bytes(entry as *mut ObjectTableEntry as *mut u8, 0, mem::size_of::<ObjectTableEntry>());

                    Pool::flush_cache_line(entry as *mut ObjectTableEntry as *const ObjectTableEntry as *const u8);
                }
            }

            //Fully Zero the logpoolHeap
            let heap_offset = (*header).heap_start;
            let heap_size = (*header).heap_size;

            ptr::write_bytes((pool_base + heap_offset) as *mut u8, 0, heap_size);
        }
    }

    /// *USE THIS CAREFULLY*
    /// Specifically empties the log pool and reinitializes it.
    /// Used after successful transactions or during recovery.
    ///
    /// # Arguments
    /// * `pool_base` - Base address of the log pool
    pub fn empty_log_pool(pool_base: u64) {
        //info!("Emptying log pool at 0x{:x}", pool_base);
        unsafe {
            let header = pool_base as *const PoolHeader;
            let table_base = (pool_base + (*header).object_table_offset) as *mut ObjectTableEntry;

            for i in 0..MAX_OBJECT_ENTRIES {
                if (*table_base.add(i)).active.load(Ordering::Acquire) {
                    let entry = &mut *table_base.add(i);
                    entry.data = None;
                    entry.type_hash = 0;
                    entry.type_size = 0;
                    ptr::write_bytes(entry.id.as_mut_ptr(), 0, 32);
                    entry.id_len = 0;

                    // Zero out the entire entry to ensure complete clearing
                    ptr::write_bytes(entry as *mut ObjectTableEntry as *mut u8, 0, mem::size_of::<ObjectTableEntry>());

                    Pool::flush_cache_line(entry as *mut ObjectTableEntry as *const ObjectTableEntry as *const u8);
                }
            }

            //Fully Zero the logpoolHeap
            let heap_offset = (*header).heap_start;
            let heap_size = (*header).heap_size;

            ptr::write_bytes((pool_base + heap_offset) as *mut u8, 0, heap_size);

            //qemu_exit(123);

            Self::init_log_pool(pool_base);
        }
    }

    unsafe fn deallocate(&mut self, ptr: NonNull<u8>, layout: Layout) {
        unsafe { self.heap.lock().deallocate(ptr, layout) };
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
        }
    }

    pub fn debug_print_object_table(&self, count: usize) {
        unsafe {
            let table_base = (self.base_address + (*self.header).object_table_offset) as *const ObjectTableEntry;

            info!("=== Object Table Debug Information ===");
            info!("Object Table Location: 0x{:x}", table_base as u64);

            for i in 0..count {
                let entry = &*table_base.add(i);
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
                let table_base = (log_pool.base_address + log_pool.object_table_offset) as *const ObjectTableEntry;

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

//IMPORTANT: This lies on the RAM not NVRAM
struct PendingChange {
    entry: *mut ObjectTableEntry,
    data_ptr: NonNull<u8>,
}

pub struct TransactionContext<'a> {
    pool: &'a mut Pool,
    pending_changes: Vec<PendingChange>,
}

//IMPORTANT: This lies on the RAM not NVRAM
impl<'a> TransactionContext<'a> {
    /// Allocates memory for an object and associates it with an ID.
    /// If an object with the same ID exists, it will be updated if the types match.
    ///
    /// # Arguments
    /// * `id` - Unique identifier for the object (max 32 bytes)
    /// * `data` - Object to be stored
    ///
    /// # Returns
    /// * `Ok(NonNull<T>)` - Pointer to allocated memory
    /// * `Err(PoolError)` - If allocation fails
    ///
    /// # Example
    /// ```rust
    /// #[derive(Copy, Clone)]
    /// struct User { age: u32, active: bool }
    ///
    /// pool.transaction(|ctx| {
    ///     // Create new user
    ///     let user = User { age: 25, active: true };
    ///     let ptr = ctx.allocate_with_id("user1", user)?;
    ///
    ///     // Update existing user (automatically handles existing IDs)
    ///     let updated_user = User { age: 26, active: true };
    ///     ctx.allocate_with_id("user1", updated_user)?;
    ///
    ///     Ok(())
    /// })?;
    /// ```
    pub fn allocate_with_id<T: Copy + 'static>(&mut self, id: &str, data: T) -> Result<NonNull<T>, PoolError> {
        if id.len() > 32 {
            return Err(PoolError::InvalidId);
        }

        unsafe {
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset) as *mut ObjectTableEntry;

            let mut free_entry = None;

            // Single pass through table
            for i in 0..(*self.pool.header).max_objects {
                let entry = &mut *table_base.add(i);

                // Check if this is the ID we're looking for
                if entry.active.load(Ordering::Acquire) && entry.valid.load(Ordering::Acquire) {
                    let entry_id = core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]);

                    if entry_id == id {
                        // Found existing entry - modify it
                        // But before check type hash
                        let expected_hash = Pool::compute_type_hash::<T>();
                        if entry.type_hash != expected_hash {
                            info!("Type mismatch for ID: {}", id);
                            info!(">>>>> use deallocate_by_id(\"{}\") to remove the entry and call the transaction again!", id);

                            return Err(PoolError::TypeMismatch {
                                expected: type_name::<T>(),
                                actual: "unknown",
                            });
                        }
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

            let ptr = self
                .pool
                .heap
                .lock()
                .allocate_first_fit(Layout::new::<T>())
                .map_err(|_| PoolError::AllocationFailed)?;

            // Log the allocation BEFORE making it visible
            self.pool
                .log_allocation(ptr.as_ptr() as u64, mem::size_of::<T>(), Pool::compute_type_hash::<T>())?;

            //info!("Offset: 0x{:x}", (ptr.as_ptr() as u64) - self.pool.base_address);

            entry.data = Some(ptr);
            //No flush < less than 8bytes
            ptr::write(ptr.as_ptr() as *mut T, data);

            Pool::flush_cache_line(ptr.as_ptr() as *const u8);

            // Mark as operation but not yet valid (valid happens at transaction commit)
            entry.operation_done.store(true, Ordering::Release);

            self.pending_changes.push(PendingChange { entry, data_ptr: ptr });
            Ok(ptr.cast())
        }
    }

    /// Retrieves a pointer to an object stored in the pool by its ID.
    /// This function only returns a pointer and does not copy the data.
    ///
    /// # Type Parameters
    /// * `T` - Type of the stored object. Must match the original type used during allocation.
    ///
    /// # Arguments
    /// * `id` - ID of the object to retrieve
    ///
    /// # Returns
    /// * `Ok(NonNull<T>)` - Pointer to the object if found
    /// * `Err(PoolError)` - If object not found or type mismatch
    ///
    /// # Example
    /// ```rust
    /// pool.transaction(|ctx| {
    ///     match ctx.get_by_id::<User>("user1") {
    ///         Ok(ptr) => {
    ///             // Use pointer to modify data
    ///             ctx.modify(ptr, |user| {
    ///                 user.age += 1;
    ///             })?;
    ///         },
    ///         Err(PoolError::InvalidId) => {
    ///             println!("User not found");
    ///         },
    ///         Err(PoolError::TypeMismatch { .. }) => {
    ///             println!("Stored object is not a User");
    ///         },
    ///         Err(e) => return Err(e)
    ///     }
    ///     Ok(())
    /// })?;
    /// ```
    pub fn get_by_id<T: Copy + 'static>(&self, id: &str) -> Result<NonNull<T>, PoolError> {
        unsafe {
            //info!("Looking for ID: {}", id);
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset) as *const ObjectTableEntry;

            for i in 0..(*self.pool.header).max_objects {
                let entry = &*table_base.add(i);

                // Only consider entries that are both active AND valid
                if !entry.active.load(Ordering::Acquire) || !entry.valid.load(Ordering::Acquire) {
                    continue;
                }

                let entry_id = core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]);

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
        unsafe { Ok(*ptr.as_ref()) }
    }

    /// Modifies an existing object in the pool.
    /// The modification is logged and only becomes permanent after transaction commit.
    ///
    /// # Arguments
    /// * `ptr` - Pointer to the object to modify
    /// * `f` - Closure containing modification logic
    ///
    /// # Returns
    /// * `Ok(())` - If modification succeeds
    /// * `Err(PoolError)` - If modification fails
    ///
    /// # Example
    /// ```rust
    /// pool.transaction(|ctx| {
    ///     // Get existing object
    ///     if let Ok(ptr) = ctx.get_by_id::<User>("user1") {
    ///         // Modify the object
    ///         ctx.modify(ptr, |user| {
    ///             user.age += 1;
    ///             user.active = false;
    ///         })?;
    ///     }
    ///     Ok(())
    /// })?;
    /// ```
    pub fn modify<T: Copy + 'static>(&mut self, ptr: NonNull<T>, f: impl FnOnce(&mut T)) -> Result<(), PoolError> {
        unsafe {
            // Find corresponding entry
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset) as *mut ObjectTableEntry;

            let mut entry_ptr = None;
            for i in 0..(*self.pool.header).max_objects {
                let entry = &mut *table_base.add(i);
                if entry.active.load(Ordering::Acquire)
                    && entry.valid.load(Ordering::Acquire)
                    && entry.data.map_or(false, |p| p.as_ptr() == ptr.as_ptr() as *mut u8)
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

            (*entry).operation_done.store(false, Ordering::Release);

            //Log the modification BEFORE making it visible

            self.pool.log_modification(
                ptr.as_ptr() as u64,
                ptr.as_ptr() as *const u8,
                mem::size_of::<T>(),
                Pool::compute_type_hash::<T>(),
            )?;

            // Modify data
            f(&mut *ptr.as_ptr());

            Pool::flush_cache_line(ptr.as_ptr() as *const u8);

            (*entry).operation_done.store(true, Ordering::Release);

            // Add to pending changes if not already there
            if !self.pending_changes.iter().any(|c| c.entry == entry) {
                self.pending_changes.push(PendingChange { entry, data_ptr: ptr.cast() });
            }

            Ok(())
        }
    }

    /// Deallocates an object from the pool by its ID.
    /// The deallocation is logged and becomes permanent after transaction commit.
    ///
    /// # Important Note
    /// The actual memory deallocation is delayed until the next access to the pool.
    /// Frequent deallocate/allocate cycles with the same ID should be avoided as they
    /// can lead to unnecessary overhead and fragmentation. The memory cleanup happens
    /// during the recovery phase of the next pool operation.
    ///
    /// # Arguments
    /// * `id` - ID of the object to deallocate
    ///
    /// # Returns
    /// * `Ok(())` - If deallocation succeeds
    /// * `Err(PoolError)` - If object not found or deallocation fails
    ///
    /// # Example
    /// ```rust
    /// pool.transaction(|ctx| {
    ///     // Delete single object
    ///     ctx.deallocate_by_id("user1")?;
    ///
    ///     // Example of multiple operations in one transaction
    ///     ctx.allocate_with_id("new_user", User { age: 30, active: true })?;
    ///     ctx.deallocate_by_id("old_user")?;
    ///
    ///     Ok(())
    /// })?;
    /// ```
    pub fn deallocate_by_id(&mut self, id: &str) -> Result<(), PoolError> {
        unsafe {
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset) as *mut ObjectTableEntry;

            for i in 0..(*self.pool.header).max_objects {
                let entry = &mut *table_base.add(i);

                let entry_id = core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]);

                if entry_id == id {
                    // First deallocate the memory
                    entry.operation_done.store(false, Ordering::Release);

                    if let Some(ptr) = entry.data {
                        //Log the deallocation BEFORE making it visible
                        self.pool.log_deallocation(ptr.as_ptr() as u64, entry.type_size, entry.type_hash)?;
                    }

                    entry.active.store(false, Ordering::Release);
                    entry.valid.store(false, Ordering::Release);
                    entry.operation_done.store(true, Ordering::Release);

                    self.pending_changes.retain(|c| c.entry != entry);

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

use crate::memory::pool::{Pool,};
use core::mem;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use log::{info,};
use x86_64::instructions::port::Port;

const ALLOCATOR_MAGIC: u64 = 0x4433_4F53_4E56_4D4D; // "D3OS_NVMM"

// Fixed pool size for each pool
// DO NOT SET THIS SMALLER THAN 8Kb ! Space for metadata needed
pub const FIXED_POOL_SIZE: usize = 1024*1024;// 1MB

const BITS_PER_WORD: usize = 64;
const METADATA_SIZE: usize = core::mem::size_of::<GlobalMetadata>();
const DIRECTORY_ALIGNMENT: usize = 8;

pub const LOG_POOL_NAME: &'static [u8] = b"__LOG__"; // Reserved name for log pool

#[repr(C)]
pub(crate) struct GlobalMetadata {
    magic_number: u64,
    nvdimm_size: usize,
    pool_size: usize,
    total_pools: AtomicU32,
    used_pools: AtomicU32,
    initialized_pools: AtomicU32,
    bitmap_offset: u64, // Offset to bitmap array
    pool_directory_offset: u64,
    bitmap_words: usize,
    log_pool_offset: u64,
    // Statistics
    initialization_failures: AtomicU32,
    total_allocations: AtomicUsize,
    total_deallocations: AtomicUsize,
}

#[repr(C)]
pub(crate) struct PoolDirectoryEntry {
    name: [u8; 64],
    pool: Option<Pool>,  //Direct pointer to the pool
    _padding: [u8; 8], // Adjusted padding to maintain alignment
}

// Bitmap to track pool states
#[repr(C)]
struct PoolBitmap {
    initialized_bits: [AtomicU64; 0], // Zero-sized array, actual size determined at runtime
    used_bits: [AtomicU64; 0],        // Zero-sized array, actual size determined at runtime
}

pub(crate) struct GlobalPersistentAllocator {
    base_address: u64,
    metadata: *mut GlobalMetadata,
    bitmap: *mut PoolBitmap,
    pool_directory: *mut PoolDirectoryEntry,
    log_pool_address: Option<u64>, // Direct pointer to the log pool instead of searching always
}

#[derive(Debug)]
pub struct RecoveryStatus {
    metadata_valid: bool,
    bitmap_consistent: bool,
    used_pools: u32,
    total_pools: u32,
    initialization_failures: u32,
}

#[derive(Debug)]
pub enum AllocError {
    NameTooLongOrShort,
    NoPoolsAvailable,
    InconsistentState,
    NameNotAllowed,
}

unsafe impl Send for GlobalPersistentAllocator {}
unsafe impl Sync for GlobalPersistentAllocator {}

fn calculate_max_pools(nvdimm_size: usize) -> usize {
    let entry_size = size_of::<PoolDirectoryEntry>();

    // Solve for x:
    // nvdimm_size = METADATA_SIZE + (x * FIXED_POOL_SIZE) + (ceil(x/64) * 16) + (x * entry_size)

    // Simplified approximation (slightly conservative):
    let available_space = nvdimm_size - METADATA_SIZE;
    let space_per_pool = FIXED_POOL_SIZE + entry_size + (16.0 / 64.0) as usize;

    let max_pools = available_space / space_per_pool;

    // Round down to multiple of 64 for bitmap alignment
    max_pools & !(63)
}

fn calculate_max_pools_precise(nvdimm_size: usize) -> Option<usize> {
    // Constants for clarity
    const MIN_REQUIRED_SIZE: usize =
        size_of::<GlobalMetadata>() + // Minimum for metadata
            FIXED_POOL_SIZE +             // At least one pool
            size_of::<PoolDirectoryEntry>() + // One directory entry
            16;                           // One bitmap word (for up to 64 pools)

    // Early validation
    if nvdimm_size < MIN_REQUIRED_SIZE {
        return None; // NVDIMM too small for even one pool
    }

    let metadata_size = size_of::<GlobalMetadata>();
    let directory_entry_size = size_of::<PoolDirectoryEntry>();

    // Start with maximum theoretical pools
    let mut pools = (nvdimm_size - metadata_size) /
        (FIXED_POOL_SIZE + directory_entry_size + 1);

    // Round down to multiple of 64 for bitmap alignment
    pools = pools & !(63);

    // Verify with exact calculations
    while pools > 0 {
        let bitmap_words = (pools + 63) / 64;
        let bitmap_size = bitmap_words * 16; // 2 bitmaps * 8 bytes per word

        let total_needed =
            metadata_size +                    // Global metadata
                (pools * FIXED_POOL_SIZE) +       // Actual pools
                (pools * directory_entry_size) +   // Directory entries
                bitmap_size;                      // Bitmap space

        // Add alignment padding to be extra safe
        let total_with_padding = (total_needed + 4095) & !4095; // 4KB alignment

        if total_with_padding <= nvdimm_size {
            // Double-check our numbers
            debug_assert!(pools % 64 == 0, "Pools must be multiple of 64");
            debug_assert!(total_with_padding > total_needed, "Padding calculation error");

            return Some(pools);
        }

        pools -= 64;
    }

    None // Could not find valid configuration
}


impl GlobalPersistentAllocator {
    fn check_recovery_status(&self) -> RecoveryStatus {
        unsafe {
            RecoveryStatus {
                metadata_valid: self.verify_metadata(),
                bitmap_consistent: self.verify_bitmap_consistency(),
                used_pools: (*self.metadata).used_pools.load(Ordering::Acquire),
                total_pools: (*self.metadata).total_pools.load(Ordering::Acquire),
                initialization_failures: (*self.metadata).initialization_failures.load(Ordering::Acquire),
            }
        }
    }

    pub fn new(base_address: u64, nvdimm_size: usize) -> Self {
        if FIXED_POOL_SIZE < 8 * 1024 {
            panic!("Pool size too small, must be at least 8KB");
        }

        info!("Trying to create a GlobalPersistentAllocator at address: 0x{:x}", base_address);
        let metadata = base_address as *mut GlobalMetadata;

        //let max_pools = calculate_max_pools_precise(nvdimm_size).unwrap();

        let max_pools = calculate_max_pools(nvdimm_size);
        let bitmap_words = (max_pools + 63) / 64; // ceiling division by 64


        // Calculate offsets...
        let bitmap_offset = (METADATA_SIZE + DIRECTORY_ALIGNMENT - 1) & !(DIRECTORY_ALIGNMENT - 1);
        let directory_offset = bitmap_offset + (bitmap_words * 2 * size_of::<AtomicU64>());
        let directory_offset = (directory_offset + DIRECTORY_ALIGNMENT - 1) & !(DIRECTORY_ALIGNMENT - 1);

        let mut allocator = Self {
            base_address,
            metadata,
            bitmap: (base_address + bitmap_offset as u64) as *mut PoolBitmap,
            pool_directory: (base_address + directory_offset as u64) as *mut PoolDirectoryEntry,
            log_pool_address: None
        };

        unsafe {
            if (*metadata).magic_number != ALLOCATOR_MAGIC {
                info!("Initializing new NVDIMM metadata");
                allocator.initialize(nvdimm_size, max_pools, bitmap_offset, directory_offset, bitmap_words);
                // match allocator.create_log_pool() {
                //     Ok(_) => info!("LOG pool initialized"),
                //     Err(e) => panic!("Failed to create LOG pool: {:?}", e),
                // }
                if let Err(e) = allocator.create_log_pool() {
                    panic!("Failed to create LOG pool: {:?}", e);
                }
            } else {
                info!("Found existing NVDIMM metadata, checking status");
                let status = allocator.check_recovery_status();

                if !status.metadata_valid || !status.bitmap_consistent {
                    info!("Recovery check failed: {:?}, reinitializing", status);
                    //Offsets might be wrong, so we need to reinitialize because its to risky to use
                    allocator.initialize(nvdimm_size, max_pools, bitmap_offset, directory_offset, bitmap_words);
                    match allocator.create_log_pool() {
                        Ok(_) => info!("LOG pool initialized"),
                        Err(e) => panic!("Failed to create LOG pool: {:?}", e),
                    }
                } else {
                    if (*metadata).log_pool_offset != 0 {
                        // Restore log pool pointer from metadata
                        // System Could be crashed..
                        let log_pool_address = base_address + (*metadata).log_pool_offset;
                        info!("Found LogPool: with address: 0x{:x}", log_pool_address);


                        Pool::perform_rollback(log_pool_address).expect("Failed to perform rollback");
                        Pool::empty_log_pool(log_pool_address);
                        Pool::init_log_pool(log_pool_address);

                    } else {
                        panic!("Invalid metadata: log pool offset is 0");
                    }


                    info!("Recovery check successful: {:?}", status);
                    // Verify configuration
                    assert_eq!((*metadata).nvdimm_size, nvdimm_size, "NVDIMM size mismatch");
                    assert_eq!((*metadata).pool_size, FIXED_POOL_SIZE, "Pool size mismatch");
                }

            }
        }
        //allocator.print_metadata_debug_info();
        allocator
    }

    fn initialize(&self, nvdimm_size: usize, max_pools: usize, bitmap_offset: usize, directory_offset: usize, bitmap_words: usize) {
        unsafe {
            // Initialize metadata
            ptr::write(self.metadata, GlobalMetadata {
                magic_number: ALLOCATOR_MAGIC,
                nvdimm_size,
                pool_size: FIXED_POOL_SIZE,
                total_pools: AtomicU32::new(max_pools as u32),
                used_pools: AtomicU32::new(0),
                initialized_pools: AtomicU32::new(0),
                bitmap_offset: bitmap_offset as u64,
                pool_directory_offset: directory_offset as u64,
                bitmap_words,
                log_pool_offset: 0,
                initialization_failures: AtomicU32::new(0),
                total_allocations: AtomicUsize::new(0),
                total_deallocations: AtomicUsize::new(0),
            });

            // Ensure it's persisted
            core::arch::x86_64::_mm_sfence();
            core::arch::x86_64::_mm_clflush(self.metadata as *const u8);
            core::arch::x86_64::_mm_sfence();

            // Initialize bitmap
            let bitmap = (self.base_address + bitmap_offset as u64) as *mut AtomicU64;
            for i in 0..bitmap_words * 2 {  // *2 for both bitmaps
                ptr::write(bitmap.add(i), AtomicU64::new(0));
            }

            // Ensure bitmap is persisted
            core::arch::x86_64::_mm_sfence();
            for i in 0..bitmap_words * 2 {
                core::arch::x86_64::_mm_clflush(bitmap.add(i) as *const u8);
            }
            core::arch::x86_64::_mm_sfence();
        }
    }

    fn get_bitmap_word(&self, is_used: bool, index: usize) -> &AtomicU64 {
        unsafe {
            let bitmap_words = (*self.metadata).bitmap_words;
            let word_ptr = self.bitmap as *const AtomicU64;

            // If accessing used_bits, offset by bitmap_words
            let offset = if is_used { bitmap_words } else { 0 };
            &*word_ptr.add(offset + index)
        }
    }

    fn set_bit(&self, pool_index: usize, is_used: bool, value: bool) {
        let word_index = pool_index / BITS_PER_WORD;
        let bit_index = pool_index % BITS_PER_WORD;
        let mask = 1u64 << bit_index;

        let word = self.get_bitmap_word(is_used, word_index);

        if value {
            word.fetch_or(mask, Ordering::SeqCst)
        } else {
            word.fetch_and(!mask, Ordering::SeqCst)
        };
    }

    fn is_bit_set(&self, pool_index: usize, is_used: bool) -> bool {
        let word_index = pool_index / BITS_PER_WORD;
        let bit_index = pool_index % BITS_PER_WORD;
        let mask = 1u64 << bit_index;

        let word = self.get_bitmap_word(is_used, word_index);
        (word.load(Ordering::SeqCst) & mask) != 0
    }

    /// Creates a new pool or recovers an existing one
    pub fn get_or_create_pool(&mut self, name: &[u8]) -> Result<&mut Pool, AllocError> {
        if name.len() >= 64 || name.len() <= 0 {
            return Err(AllocError::NameTooLongOrShort);
        }

        if name == LOG_POOL_NAME {
            return Err(AllocError::NameNotAllowed);
        }

        unsafe {
            let total_pools = (*self.metadata).total_pools.load(Ordering::Acquire);
            let used_pools = (*self.metadata).used_pools.load(Ordering::Acquire);

            let mut first_free_slot = None;

            // Single pass through the pools
            for i in 0..total_pools as usize {
                let entry = &mut *self.pool_directory.add(i);

                if self.is_bit_set(i, true) {
                    // Check if this is our pool
                    if self.compare_name(name, &entry.name) {
                        if let Some(pool) = &mut entry.pool {
                            info!("Pool already exists");
                            return Ok(pool);
                        }
                    }
                } else if first_free_slot.is_none() && !self.is_bit_set(i, false) {
                    first_free_slot = Some((i, entry));
                }
            }

            //Could be that the bitmap insnt full but no more store!
            if used_pools >= total_pools {
                info!("Cannot create new pool: all {} pools are in use", total_pools);
                return Err(AllocError::NoPoolsAvailable);
            }

            if first_free_slot.is_none() {
                info!("Inconsistency detected: used_pools reported {} free slots but none found",
                  total_pools - used_pools);
                (*self.metadata).initialization_failures.fetch_add(1, Ordering::Release);
                return Err(AllocError::InconsistentState);
            }

            // Create new pool in free slot...
            let (index, entry) = first_free_slot.unwrap();

            // Create new pool if slot found

            let pool_offset = self.get_pool_data_offset();
            let pool_address = self.base_address + pool_offset + (index as u64 * FIXED_POOL_SIZE as u64);

            // Prepare new entry
            let mut new_entry = PoolDirectoryEntry {
                name: [0; 64],
                //pool: Some(Pool::new(pool_address, FIXED_POOL_SIZE, self.log_pool_address)),
                pool: Some(Pool::new(pool_address, FIXED_POOL_SIZE)),//test
                _padding: [0; 8],
            };
            ptr::copy_nonoverlapping(name.as_ptr(), new_entry.name.as_mut_ptr(), name.len());
            new_entry.name[name.len()] = 0;
            info!("Created new Pool with ID {}", core::str::from_utf8(name).unwrap());

            // Batch update metadata counters
            let metadata = &*self.metadata;
            metadata.used_pools.fetch_add(1, Ordering::Release);
            metadata.initialized_pools.fetch_add(1, Ordering::Release);
            metadata.total_allocations.fetch_add(1, Ordering::Release);

            // Set both bits in bitmap
            let word_index = index / BITS_PER_WORD;
            let bit_index = index % BITS_PER_WORD;
            let mask = 1u64 << bit_index;

            // Update bitmap words
            let init_word = self.get_bitmap_word(false, word_index);
            let used_word = self.get_bitmap_word(true, word_index);

            init_word.fetch_or(mask, Ordering::SeqCst);
            used_word.fetch_or(mask, Ordering::SeqCst);

            // Single fence before flushes
            core::arch::x86_64::_mm_sfence();

            // Flush bitmap updates
            core::arch::x86_64::_mm_clflush(init_word as *const AtomicU64 as *const u8);
            core::arch::x86_64::_mm_clflush(used_word as *const AtomicU64 as *const u8);

            // Write and persist entry
            ptr::write_volatile(entry, new_entry);
            core::arch::x86_64::_mm_clflush(entry as *const PoolDirectoryEntry as *const u8);

            // Final fence
            core::arch::x86_64::_mm_sfence();



            entry.pool.as_mut().ok_or(AllocError::InconsistentState)

        }
    }

    fn create_log_pool(&mut self) -> Result<(), AllocError> {
        unsafe {
            info!("Creating log pool");
            let total_pools = (*self.metadata).total_pools.load(Ordering::Acquire);

            for i in 0..total_pools as usize {
                let entry = &mut *self.pool_directory.add(i);

                if !self.is_bit_set(i, true) && !self.is_bit_set(i, false) {
                    let pool_offset = self.get_pool_data_offset();
                    let pool_address = self.base_address + pool_offset +
                        (i as u64 * FIXED_POOL_SIZE as u64);

                    info!("Creating log pool at address: 0x{:x}", pool_address);

                    // Initialize the static log pool first
                    Pool::init_log_pool(pool_address);

                    // Create directory entry
                    let mut new_entry = PoolDirectoryEntry {
                        name: [0; 64],
                        pool: None,
                        _padding: [0; 8],
                    };

                    ptr::copy_nonoverlapping(
                        LOG_POOL_NAME.as_ptr(),
                        new_entry.name.as_mut_ptr(),
                        LOG_POOL_NAME.len()
                    );

                    // Update bitmap and entry
                    self.set_bit(i, true, true);
                    self.set_bit(i, false, true);

                    ptr::write_volatile(entry, new_entry);
                    core::arch::x86_64::_mm_sfence();
                    core::arch::x86_64::_mm_clflush(entry as *const PoolDirectoryEntry as *const u8);
                    core::arch::x86_64::_mm_sfence();

                    // Update metadata
                    (*self.metadata).log_pool_offset = pool_address - self.base_address;
                    (*self.metadata).used_pools.fetch_add(1, Ordering::Release);
                    (*self.metadata).initialized_pools.fetch_add(1, Ordering::Release);

                    return Ok(());
                }
            }
            Err(AllocError::NoPoolsAvailable)
        }
    }


    pub fn release_pool(&mut self, name: &[u8]) -> bool {
        if name.len() >= 64 || name.len() <= 0 {
            return false;
        }

        if name == LOG_POOL_NAME {
            return false;
        }

        unsafe {
            let total_pools = (*self.metadata).total_pools.load(Ordering::Acquire);
            for i in 0..total_pools {
                if self.is_bit_set(i as usize, true) {
                    let entry = &mut *self.pool_directory.add(i as usize);
                    if self.compare_name(name, &entry.name) {
                        info!(
                            "Pool found! Releasing pool: {}",
                            core::str::from_utf8(name).unwrap()
                        );
                        // Directly invalidate the pool's magic number
                        if let Some(pool) = &mut entry.pool {
                            // Use ptr::write_volatile to write to the header's magic field
                            ptr::write_volatile(&mut (*pool.header).magic as *mut u64, 0);
                            pool.empty_pool();

                            // Ensure write is flushed to persistence
                            core::arch::x86_64::_mm_sfence();
                            core::arch::x86_64::_mm_clflush(&(*pool.header).magic as *const u64 as *const u8);
                            core::arch::x86_64::_mm_sfence();
                        }

                        // Clear bits
                        self.set_bit(i as usize, true, false); // Clear used
                        self.set_bit(i as usize, false, false); // Clear initialized

                        // Update metadata
                        (*self.metadata).used_pools.fetch_sub(1, Ordering::Release);
                        (*self.metadata).total_deallocations.fetch_add(1, Ordering::Release);

                        // Clear entry
                        self.ensure_persistent_write(entry, PoolDirectoryEntry {
                            name: [0; 64],
                            pool: None,
                            _padding: [0; 8],
                        });

                        return true;
                    }
                }
            }
        }
        false
    }

    fn get_pool_data_offset(&self) -> u64 {
        //info!("Calculating pool data offset:");
        //info!("  pool_directory_offset: 0x{:x}", unsafe { (*self.metadata).pool_directory_offset });
        //info!("  total_pools: {}", unsafe { (*self.metadata).total_pools.load(Ordering::Relaxed)  });
        //info!("  sizeof PoolDirectoryEntry: {}",mem::size_of::<PoolDirectoryEntry>());
        let offset = unsafe {
            // Remove self.base_address from here
            (*self.metadata).pool_directory_offset
                + ((*self.metadata).total_pools.load(Ordering::Relaxed) as usize
                * mem::size_of::<PoolDirectoryEntry>()) as u64
        };

        //info!("  Final offset: 0x{:x}", offset);
        offset
    }



    fn compare_name(&self, name: &[u8], entry_name: &[u8; 64]) -> bool {
        let mut i = 0;
        while i < name.len() && i < 64 {
            if name[i] != entry_name[i] {
                return false;
            }
            i += 1;
        }
        i < 64 && entry_name[i] == 0
    }

    fn ensure_persistent_write<T>(&self, addr: *mut T, value: T) {
        unsafe {
            // Write value
            ptr::write_volatile(addr, value);

            // Ensure persistence
            core::arch::x86_64::_mm_sfence();
            core::arch::x86_64::_mm_clflush(addr as *const u8);
            core::arch::x86_64::_mm_sfence();
        }
    }

    fn persist_bitmap_word(&self, pool_index: usize, is_used: bool) {
        let word_index = pool_index / BITS_PER_WORD;
        let word = self.get_bitmap_word(is_used, word_index);

        unsafe {
            core::arch::x86_64::_mm_sfence();
            core::arch::x86_64::_mm_clflush(word as *const _ as *const u8);
            core::arch::x86_64::_mm_sfence();
        }
    }

    fn persist_metadata_counters(&self) {
        unsafe {
            let metadata = &*self.metadata;
            core::arch::x86_64::_mm_sfence();

            // Flush the atomic counters
            core::arch::x86_64::_mm_clflush(&metadata.used_pools as *const _ as *const u8);
            core::arch::x86_64::_mm_clflush(&metadata.initialized_pools as *const _ as *const u8);
            core::arch::x86_64::_mm_clflush(&metadata.total_allocations as *const _ as *const u8);

            core::arch::x86_64::_mm_sfence();
        }
    }

    fn verify_metadata(&self) -> bool {
        unsafe {
            // Check magic number
            if (*self.metadata).magic_number != ALLOCATOR_MAGIC {
                return false;
            }

            // Verify size constraints
            if (*self.metadata).nvdimm_size < METADATA_SIZE {
                return false;
            }

            if (*self.metadata).pool_size != FIXED_POOL_SIZE {
                return false;
            }

            if (*self.metadata).total_pools.load(Ordering::Relaxed) == 0 {
                return false;
            }
            
            true
        }
    }

    fn verify_bitmap_consistency(&self) -> bool {
        // Verify that used_pools count matches bitmap
        let mut count = 0;
        let total_pools = unsafe { (*self.metadata).total_pools.load(Ordering::Relaxed) };

        for i in 0..total_pools {
            if self.is_bit_set(i as usize, true) {
                count += 1;
            }
        }

        unsafe {
            count == (*self.metadata).used_pools.load(Ordering::Relaxed)
        }
    }

    fn print_metadata_debug_info(&self) {
        unsafe {
            info!("=== NVDIMM Metadata Debug Info ===");
            info!("Base address: 0x{:x}", self.base_address);
            info!("Magic number: 0x{:x}", (*self.metadata).magic_number);
            info!("NVDIMM size: {} bytes ({} MB)",
                (*self.metadata).nvdimm_size,
                (*self.metadata).nvdimm_size / (1024 * 1024));
            info!("Pool size: {} bytes ({} KB)",
                (*self.metadata).pool_size,
                (*self.metadata).pool_size / 1024);

            // Pool information
            info!("Total pools: {}", (*self.metadata).total_pools.load(Ordering::Acquire));
            info!("Used pools: {}", (*self.metadata).used_pools.load(Ordering::Acquire));
            info!("Initialized pools: {}", (*self.metadata).initialized_pools.load(Ordering::Acquire));

            // Layout information
            info!("Bitmap offset: 0x{:x}", (*self.metadata).bitmap_offset);
            info!("Pool directory offset: 0x{:x}", (*self.metadata).pool_directory_offset);
            info!("Pool directory size: {} bytes",
                (*self.metadata).total_pools.load(Ordering::Acquire) as usize * size_of::<PoolDirectoryEntry>());
            info!("Bitmap words: {}", (*self.metadata).bitmap_words);

            info!("=== Log Pool Information ===");
            if let Some(log_pool_address) = self.log_pool_address {
                info!("Log pool address: 0x{:x}", log_pool_address);
            } else {
                info!("Log pool not initialized");
            }

            // Statistics
            info!("=== Statistics ===");
            info!("Initialization failures: {}",
                (*self.metadata).initialization_failures.load(Ordering::Acquire));
            info!("Total allocations: {}",
                (*self.metadata).total_allocations.load(Ordering::Acquire));
            info!("Total deallocations: {}",
                (*self.metadata).total_deallocations.load(Ordering::Acquire));
            info!("==============================");
        }
    }

    pub fn print_bitmap(&self) {
        unsafe {
            let total_pools = (*self.metadata).total_pools.load(Ordering::Acquire);
            for i in 0..total_pools {
                let used = self.is_bit_set(i as usize, true);
                let initialized = self.is_bit_set(i as usize, false);
                info!("Pool {}: initialized: {}, used: {}", i, initialized, used);
            }
        }
    }
}

pub(crate) fn qemu_exit(exit_code: u32) -> ! {
    unsafe {
        let mut port = Port::new(0xf4);
        port.write(exit_code as u32);
    }
    loop {}
}

use crate::memory::pool::Pool;
use bitflags::bitflags;
use core::mem;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use log::info;
use x86_64::instructions::port::Port;

const ALLOCATOR_MAGIC: u64 = 0x4433_4F53_4E56_4D4D; // "D3OS_NVMM"
const FIXED_POOL_SIZE: usize = 1048576;// 1MB

const BITS_PER_WORD: usize = 64;
const METADATA_SIZE: usize = core::mem::size_of::<GlobalMetadata>();
const DIRECTORY_ALIGNMENT: usize = 8;

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
    // Statistics
    initialization_failures: AtomicU32,
    total_allocations: AtomicUsize,
    total_deallocations: AtomicUsize,
}

#[repr(C)]
pub(crate) struct PoolDirectoryEntry {
    name: [u8; 64],
    pool: Option<Pool>,
    _padding: [u8; 8], // Adjusted padding to maintain alignment
}

// Bitmap to track pool states
#[repr(C)]
struct PoolBitmap {
    initialized_bits: [AtomicU64; 0], // Zero-sized array, actual size determined at runtime
    used_bits: [AtomicU64; 0],        // Zero-sized array, actual size determined at runtime
}

bitflags! {
    #[derive(Debug)]
    pub struct PoolStatus: u8 {
        const UNINITIALIZED = 0b00;
        const INITIALIZED = 0b01;
        const IN_USE = 0b10;
        const ERROR = 0b11;
    }
}

pub(crate) struct GlobalPersistentAllocator {
    base_address: u64,
    metadata: *mut GlobalMetadata,
    bitmap: *mut PoolBitmap,
    pool_directory: *mut PoolDirectoryEntry,
}

#[derive(Debug)]
pub struct RecoveryStatus {
    metadata_valid: bool,
    bitmap_consistent: bool,
    used_pools: u32,
    total_pools: u32,
    initialization_failures: u32,
}

unsafe impl Send for GlobalPersistentAllocator {}
unsafe impl Sync for GlobalPersistentAllocator {}

//helper fn
fn calculate_pool_layout(nvdimm_size: usize) -> (usize, usize) {
    let aligned_metadata_size =
        (METADATA_SIZE + DIRECTORY_ALIGNMENT - 1) & !(DIRECTORY_ALIGNMENT - 1);

    // Calculate maximum possible pools
    let max_pools = (nvdimm_size - aligned_metadata_size) / FIXED_POOL_SIZE;

    // Calculate how many u64 words we need for the bitmap
    let bitmap_words = (max_pools + BITS_PER_WORD - 1) / BITS_PER_WORD;

    (max_pools, bitmap_words)
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
        info!("Trying to create a GlobalPersistentAllocator at address: 0x{:x}", base_address);
        let metadata = base_address as *mut GlobalMetadata;
        let (max_pools, bitmap_words) = calculate_pool_layout(nvdimm_size);

        // Calculate offsets...
        let bitmap_offset = (METADATA_SIZE + DIRECTORY_ALIGNMENT - 1) & !(DIRECTORY_ALIGNMENT - 1);
        let directory_offset = bitmap_offset + (bitmap_words * 2 * size_of::<AtomicU64>());
        let directory_offset = (directory_offset + DIRECTORY_ALIGNMENT - 1) & !(DIRECTORY_ALIGNMENT - 1);

        let allocator = Self {
            base_address,
            metadata,
            bitmap: (base_address + bitmap_offset as u64) as *mut PoolBitmap,
            pool_directory: (base_address + directory_offset as u64) as *mut PoolDirectoryEntry,
        };

        unsafe {
            if (*metadata).magic_number != ALLOCATOR_MAGIC {
                info!("Initializing new NVDIMM metadata");

                // Initialize metadata
                ptr::write(metadata, GlobalMetadata {
                    magic_number: ALLOCATOR_MAGIC,
                    nvdimm_size,
                    pool_size: FIXED_POOL_SIZE,
                    total_pools: AtomicU32::new(max_pools as u32),
                    used_pools: AtomicU32::new(0),
                    initialized_pools: AtomicU32::new(0),
                    bitmap_offset: bitmap_offset as u64,
                    pool_directory_offset: directory_offset as u64,
                    bitmap_words,
                    initialization_failures: AtomicU32::new(0),
                    total_allocations: AtomicUsize::new(0),
                    total_deallocations: AtomicUsize::new(0),
                });

                // Ensure it's persisted
                core::arch::x86_64::_mm_sfence();
                core::arch::x86_64::_mm_clflush(metadata as *const u8);
                core::arch::x86_64::_mm_sfence();


                // Initialize bitmap (also can be done in one write per bitmap)
                let bitmap = (base_address + bitmap_offset as u64) as *mut AtomicU64;
                for i in 0..bitmap_words * 2 {  // *2 for both bitmaps
                    ptr::write(bitmap.add(i), AtomicU64::new(0));
                }

                // Ensure bitmap is persisted
                core::arch::x86_64::_mm_sfence();
                for i in 0..bitmap_words * 2 {
                    core::arch::x86_64::_mm_clflush(bitmap.add(i) as *const u8);
                }
                core::arch::x86_64::_mm_sfence();
                allocator.print_metadata_debug_info();

            } else {
                info!("Found existing NVDIMM metadata, checking status");
                let status = allocator.check_recovery_status();

                if !status.metadata_valid || !status.bitmap_consistent {
                    info!("Recovery check failed: {:?}, reinitializing", status);
                    //TODO: Init code wieder callen .. vielleicht geht das noch besser ?
                } else {
                    info!("Recovery check successful: {:?}", status);
                    // Verify configuration
                    assert_eq!((*metadata).nvdimm_size, nvdimm_size, "NVDIMM size mismatch");
                    assert_eq!((*metadata).pool_size, FIXED_POOL_SIZE, "Pool size mismatch");
                }

            }
        }
        allocator
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
    pub fn get_or_create_pool(&mut self, name: &[u8]) -> Option<&mut Pool> {
        if name.len() >= 64 {
            return None;
        }

        let total_pools = unsafe { (*self.metadata).total_pools.load(Ordering::Acquire) };

        // First try to find existing pool
        for i in 0..total_pools {
            if self.is_bit_set(i as usize, true) {
                unsafe {
                    let entry = &mut *self.pool_directory.add(i as usize);
                    if self.compare_name(name, &entry.name) {
                        if let Some(pool) = &mut entry.pool {
                            info!("Pool with name found: {}", core::str::from_utf8(name).unwrap());
                            self.print_metadata_debug_info();
                            return Some(pool);
                        }
                    }
                }
            }
        }

        info!("no pool with name found, creating new pool");

        // Create new pool if not found
        for i in 0..total_pools {
            unsafe {
                if !self.is_bit_set(i as usize, true) {
                    let entry = &mut *self.pool_directory.add(i as usize);
                    info!("found empty slot in bitmap");

                    let pool_offset = self.get_pool_data_offset();
                    let pool_address = self.base_address + pool_offset + (i as u64 * FIXED_POOL_SIZE as u64);

                    // Create new pool
                    let pool = Pool::new(pool_address, FIXED_POOL_SIZE);

                    // Update directory entry
                    let mut new_entry = PoolDirectoryEntry {
                        name: [0; 64],
                        pool: Some(pool),
                        _padding: [0; 8],
                    };
                    ptr::copy_nonoverlapping(name.as_ptr(), new_entry.name.as_mut_ptr(), name.len());
                    new_entry.name[name.len()] = 0;

                    // Important: First persist the directory entry
                    self.ensure_persistent_write(entry, new_entry);

                    // Now set the bitmap bits - order matters!
                    self.set_bit(i as usize, false, true); // Set initialized
                    self.persist_bitmap_word(i as usize, false);

                    self.set_bit(i as usize, true, true); // Set used
                    self.persist_bitmap_word(i as usize, true);

                    // Update and persist metadata counters
                    (*self.metadata).used_pools.fetch_add(1, Ordering::Release);
                    (*self.metadata).initialized_pools.fetch_add(1, Ordering::Release);
                    (*self.metadata).total_allocations.fetch_add(1, Ordering::Release);

                    // Ensure metadata counters are persisted
                    self.persist_metadata_counters();

                    return entry.pool.as_mut();
                }
            }
        }
        None
    }


    // You might also want to add a method to release/delete pools:
    pub fn release_pool(&mut self, name: &[u8]) -> bool {
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
        info!("Calculating pool data offset:");
        info!("  pool_directory_offset: 0x{:x}", unsafe {
            (*self.metadata).pool_directory_offset
        });
        info!("  total_pools: {}", unsafe {
            (*self.metadata).total_pools.load(Ordering::Relaxed)
        });
        info!(
            "  sizeof PoolDirectoryEntry: {}",
            mem::size_of::<PoolDirectoryEntry>()
        );

        let offset = unsafe {
            // Remove self.base_address from here
            (*self.metadata).pool_directory_offset
                + ((*self.metadata).total_pools.load(Ordering::Relaxed) as usize
                * mem::size_of::<PoolDirectoryEntry>()) as u64
        };

        info!("  Final offset: 0x{:x}", offset);
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
            //TODO: Noch drüber nachdenken ob sich mehr lohnt ?
            
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
            info!("Bitmap words: {}", (*self.metadata).bitmap_words);

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
}

pub(crate) fn qemu_exit(exit_code: u32) -> ! {
    unsafe {
        let mut port = Port::new(0xf4);
        port.write(exit_code as u32);
    }
    loop {}
}

use crate::memory::nvram_allocator::qemu_exit;
use crate::memory::pool::Pool;
use bitflags::bitflags;
use core::mem;
use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use log::info;

const ALLOCATOR_MAGIC: u64 = 0x4433_4F53_4E56_4D4D; // "D3OS_NVMM"
const FIXED_POOL_SIZE: usize = 1024; // 1KB

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
    pub fn new(base_address: u64, nvdimm_size: usize) -> Self {
        //TODO: in fn auslagern, falls eine der sachen != dann neu erstellen..
        info!(
            "Creating GlobalPersistentAllocator at address: 0x{:x}",
            base_address
        );
        let metadata = base_address as *mut GlobalMetadata;
        let (max_pools, bitmap_words) = calculate_pool_layout(nvdimm_size);

        // Calculate offsets for different sections
        let bitmap_offset = (METADATA_SIZE + DIRECTORY_ALIGNMENT - 1) & !(DIRECTORY_ALIGNMENT - 1);
        let mut directory_offset = bitmap_offset + (bitmap_words * 2 * size_of::<AtomicU64>()); //2* because we have two bitmaps
        directory_offset =
            (directory_offset + DIRECTORY_ALIGNMENT - 1) & !(DIRECTORY_ALIGNMENT - 1);

        let bitmap = (base_address + bitmap_offset as u64) as *mut PoolBitmap;
        let pool_directory = (base_address + directory_offset as u64) as *mut PoolDirectoryEntry;

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

                // Initialize bitmap words to 0
                for i in 0..bitmap_words {
                    // Initialize initialized_bits
                    ptr::write((bitmap as *mut AtomicU64).add(i), AtomicU64::new(0));
                    // Initialize used_bits
                    ptr::write(
                        (bitmap as *mut AtomicU64).add(bitmap_words + i),
                        AtomicU64::new(0),
                    );
                }

                //debug info of all metadata:
                info!("magicnumber: {}", (*metadata).magic_number);
                info!("nvdimm_size: {}", (*metadata).nvdimm_size);
                info!("pool_size: {}", (*metadata).pool_size);
                info!(
                    "total_pools: {}",
                    (*metadata).total_pools.load(Ordering::Acquire)
                );
                info!(
                    "used_pools: {}",
                    (*metadata).used_pools.load(Ordering::Acquire)
                );
                info!(
                    "initialized_pools: {}",
                    (*metadata).initialized_pools.load(Ordering::Acquire)
                );
                info!("bitmap_offset: {}", (*metadata).bitmap_offset);
                info!(
                    "pool_directory_offset: {}",
                    (*metadata).pool_directory_offset
                );
                info!("bitmap_words: {}", (*metadata).bitmap_words);
                info!(
                    "initialization_failures: {}",
                    (*metadata).initialization_failures.load(Ordering::Acquire)
                );
                info!(
                    "total_allocations: {}",
                    (*metadata).total_allocations.load(Ordering::Acquire)
                );
                info!(
                    "total_deallocations: {}",
                    (*metadata).total_deallocations.load(Ordering::Acquire)
                );
            } else {
                // Verify the configuration matches
                assert_eq!((*metadata).nvdimm_size, nvdimm_size, "NVDIMM size mismatch");
                assert_eq!((*metadata).pool_size, FIXED_POOL_SIZE, "Pool size mismatch");
                info!("Recovered existing NVDIMM metadata");
                info!(
                    "{} used pools were found",
                    (*metadata).used_pools.load(Ordering::Acquire)
                );
            }
        }

        Self {
            base_address,
            metadata,
            bitmap,
            pool_directory,
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

        //Thread safety:
        //Changed to Acquire and Release to ensure that the bit is set before the pool is used
        if value {
            word.fetch_or(mask, Ordering::Release);
        } else {
            word.fetch_and(!mask, Ordering::Acquire);
        }
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

        // Add pool-level locking
        let total_pools = unsafe { (*self.metadata).total_pools.load(Ordering::Acquire) };

        // First try to find existing pool
        for i in 0..total_pools {
            if self.is_bit_set(i as usize, true) {
                unsafe {
                    let entry = &mut *self.pool_directory.add(i as usize);
                    // Add entry-level synchronization
                    if self.compare_name(name, &entry.name) {
                        if let Some(pool) = &mut entry.pool {
                            return Some(pool);
                        }
                    }
                }
            }
        }

        info!("no pool with name found, creating new pool");

        // Create new pool if not found
        for i in 0..total_pools {
            // Find first unused slot in bitmap
            unsafe {
                if !self.is_bit_set(i as usize, true) {
                    let entry = &mut *self.pool_directory.add(i as usize);
                    info!("found empty slot in bitmap");

                    // Calculate pool address
                    let pool_offset = self.get_pool_data_offset();
                    info!("Pool offset: 0x{:x}", pool_offset);
                    let pool_address =
                        self.base_address + pool_offset + (i as u64 * FIXED_POOL_SIZE as u64);
                    info!("Calculated pool address: 0x{:x}", pool_address);

                    // Create new pool
                    let pool = Pool::new(pool_address, FIXED_POOL_SIZE);
                    info!("new pool created");

                    // Update directory entry
                    ptr::copy_nonoverlapping(name.as_ptr(), entry.name.as_mut_ptr(), name.len());
                    entry.name[name.len()] = 0;
                    entry.pool = Some(pool);
                    info!("directory entry updated");

                    // Set both initialized and used bits
                    self.set_bit(i as usize, false, true); // Set initialized
                    self.set_bit(i as usize, true, true); // Set used

                    // Update metadata counters
                    (*self.metadata).used_pools.fetch_add(1, Ordering::Release);
                    (*self.metadata)
                        .initialized_pools
                        .fetch_add(1, Ordering::Release);
                    (*self.metadata)
                        .total_allocations
                        .fetch_add(1, Ordering::Release);

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
                        (*self.metadata)
                            .total_deallocations
                            .fetch_add(1, Ordering::Release);

                        // Clear entry
                        entry.pool = None;
                        entry.name = [0; 64];

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

    pub fn recover(&mut self) -> bool {
        info!(
            "Attempting to recover at address: 0x{:x}",
            self.base_address
        );
        unsafe { (*self.metadata).magic_number == ALLOCATOR_MAGIC }
    }
}

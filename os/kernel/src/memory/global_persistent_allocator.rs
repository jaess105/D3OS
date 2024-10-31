use crate::memory::pool::Pool;
use core::mem;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use log::info;

const ALLOCATOR_MAGIC: u64 = 0x4433_4F53_4E56_4D4D; // "D3OS_NVMM"
const FIXED_POOL_SIZE: usize = 1024 * 1024; // 1KB

#[repr(C)]
pub(crate) struct GlobalMetadata {
    magic_number: u64,
    total_pools: AtomicU32,
    used_pools: AtomicU32,
    pool_directory_offset: u64,
}

#[repr(C)]
pub(crate) struct PoolDirectoryEntry {
    name: [u8; 64],
    pool: Option<Pool>,
    is_used: bool,
    _padding: [u8; 7],
}

pub(crate) struct GlobalPersistentAllocator {
    base_address: u64,
    metadata: *mut GlobalMetadata,
    pool_directory: *mut PoolDirectoryEntry,
    total_size: usize,
}

unsafe impl Send for GlobalPersistentAllocator {}
unsafe impl Sync for GlobalPersistentAllocator {}

impl GlobalPersistentAllocator {
    pub fn new(base_address: u64, total_size: usize) -> Self {
        let metadata = base_address as *mut GlobalMetadata;
        let directory_offset = (mem::size_of::<GlobalMetadata>() + 7) & !7; // align to 8 bytes
        let pool_directory = (base_address + directory_offset as u64) as *mut PoolDirectoryEntry;

        unsafe {
            if (*metadata).magic_number != ALLOCATOR_MAGIC {
                // Initialize new metadata
                info!("No valid metadata found, initializing new metadata");
                let max_pools = (total_size - directory_offset) / FIXED_POOL_SIZE;

                ptr::write(metadata, GlobalMetadata {
                    magic_number: ALLOCATOR_MAGIC,
                    total_pools: AtomicU32::new(max_pools as u32),
                    used_pools: AtomicU32::new(0),
                    pool_directory_offset: directory_offset as u64,
                });

                // Initialize directory entries
                for i in 0..max_pools {
                    ptr::write(pool_directory.add(i), PoolDirectoryEntry {
                        name: [0; 64],
                        pool: None,
                        is_used: false,
                        _padding: [0; 7],
                    });
                }
            }
        }

        GlobalPersistentAllocator {
            base_address,
            metadata,
            pool_directory,
            total_size,
        }
    }

    /// Creates a new pool or recovers an existing one
    pub fn get_or_create_pool(&mut self, name: &[u8]) -> Option<&mut Pool> {
        if name.len() >= 64 {
            return None;
        }

        unsafe {
            // First try to find existing pool
            let total_pools = (*self.metadata).total_pools.load(Ordering::Acquire);
            for i in 0..total_pools {
                let entry = &mut *self.pool_directory.add(i as usize);
                if entry.is_used && self.compare_name(name, &entry.name) {
                    // Found existing pool
                    if let Some(pool) = &mut entry.pool {
                        // Recover pool if needed
                        pool.recover().expect("TODO: panic message"); //TODO: panic message
                        return Some(pool);
                    }
                }
            }

            // Create new pool if not found
            for i in 0..total_pools {
                let entry = &mut *self.pool_directory.add(i as usize);
                if !entry.is_used {
                    // Calculate pool address
                    let pool_address = self.base_address
                        + (self.get_pool_data_offset() + i as u64 * FIXED_POOL_SIZE as u64);

                    // Create new pool
                    let pool = Pool::new(pool_address, FIXED_POOL_SIZE);

                    // Update directory entry
                    ptr::copy_nonoverlapping(name.as_ptr(), entry.name.as_mut_ptr(), name.len());
                    entry.name[name.len()] = 0;
                    entry.pool = Some(pool);
                    entry.is_used = true;

                    (*self.metadata).used_pools.fetch_add(1, Ordering::Release);
                    return entry.pool.as_mut();
                }
            }
        }
        None
    }

    fn get_pool_data_offset(&self) -> u64 {
        unsafe {
            self.base_address
                + (*self.metadata).pool_directory_offset
                + ((*self.metadata).total_pools.load(Ordering::Relaxed) as usize
                    * mem::size_of::<PoolDirectoryEntry>()) as u64
        }
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
        unsafe { (*self.metadata).magic_number == ALLOCATOR_MAGIC }
    }
}

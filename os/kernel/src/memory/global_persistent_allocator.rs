use crate::memory::nvmem::align_up;
use core::mem;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use log::info;
use uefi::table::boot::PAGE_SIZE;

#[repr(C)]
pub(crate) struct GlobalMetadata {
    magic_number: u64,
    version: u32,
    pool_size: usize,
    total_pools: AtomicU32, // Make atomic
    used_pools: AtomicU32,  // Make atomic
    pool_directory_offset: u64,
}

#[repr(C)]
pub(crate) struct PoolDirectoryEntry {
    name: [u8; 64],
    address: u64,
    is_used: bool,
    size: usize,
    _padding: [u8; 7], // Ensure proper alignment
}

pub(crate) struct GlobalPersistentAllocator {
    base_address: u64,
    metadata: *mut GlobalMetadata,
    pool_directory: *mut PoolDirectoryEntry,
    total_size: usize,
}

pub(crate) const ALLOCATOR_MAGIC: u64 = 0x4433_4F53_4E56_4D4D; // "D3OS_NVMM"

unsafe impl Send for GlobalPersistentAllocator {}
unsafe impl Sync for GlobalPersistentAllocator {}

impl GlobalPersistentAllocator {
    pub fn new(base_address: u64, total_size: usize) -> Self {
        let metadata = base_address as *mut GlobalMetadata;
        let directory_offset = align_up(mem::size_of::<GlobalMetadata>(), 8);
        let pool_directory = (base_address + directory_offset as u64) as *mut PoolDirectoryEntry;

        unsafe {
            if (*metadata).magic_number != ALLOCATOR_MAGIC {
                // Initialize new metadata
                info!("No valid metadata found, initializing new metadata");
                let total_pools = ((total_size - directory_offset) / PAGE_SIZE as usize) as u32;
                ptr::write(metadata, GlobalMetadata {
                    magic_number: ALLOCATOR_MAGIC,
                    version: 1,
                    pool_size: PAGE_SIZE as usize,
                    total_pools: AtomicU32::new(total_pools),
                    used_pools: AtomicU32::new(0),
                    pool_directory_offset: directory_offset as u64,
                });

                info!("magic_number: {:x}", (*metadata).magic_number);
                info!("version: {}", (*metadata).version);
                info!("pool_size: {}", (*metadata).pool_size);
                info!("total_pools: {}", (*metadata).total_pools.load(Ordering::Relaxed));
                info!("used_pools: {}", (*metadata).used_pools.load(Ordering::Relaxed));
                info!("pool_directory_offset: {}", (*metadata).pool_directory_offset);

                // Clear directory entries
                for i in 0..total_pools {
                    ptr::write(pool_directory.add(i as usize), PoolDirectoryEntry {
                        name: [0; 64],
                        address: 0,
                        is_used: false,
                        size: 0,
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

    pub fn allocate_pool(&mut self, name: &[u8], requested_size: usize) -> Option<u64> {
        if name.len() >= 63 {
            return None;
        }

        unsafe {
            let total_pools = (*self.metadata).total_pools.load(Ordering::Acquire);
            for i in 0..total_pools {
                let entry = &mut *self.pool_directory.add(i as usize);
                if !entry.is_used {
                    let pool_address = self.base_address
                        + (self.get_pool_data_offset()
                            + i as u64 * align_up(requested_size, PAGE_SIZE as usize) as u64);

                    entry.is_used = true;
                    ptr::copy_nonoverlapping(name.as_ptr(), entry.name.as_mut_ptr(), name.len());
                    entry.name[name.len()] = 0;
                    entry.address = pool_address;
                    entry.size = requested_size;

                    (*self.metadata).used_pools.fetch_add(1, Ordering::Release);
                    return Some(pool_address);
                } else if self.compare_name(name, &entry.name) {
                    info!("Pool with name {:?} already exists", name);
                    return None;
                }
            }
        }
        None
    }

    pub fn find_pool(&self, name: &[u8]) -> Option<(u64, usize)> {
        unsafe {
            let total_pools = (*self.metadata).total_pools.load(Ordering::Acquire);
            for i in 0..total_pools {
                let entry = &*self.pool_directory.add(i as usize);
                if entry.is_used && self.compare_name(name, &entry.name) {
                    return Some((entry.address, entry.size));
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

    fn get_total_pools(&self) -> u32 {
        unsafe { (*self.metadata).total_pools.load(Ordering::Relaxed) }
    }

    pub fn recover(&mut self) -> bool {
        unsafe { (*self.metadata).magic_number == ALLOCATOR_MAGIC }
    }
}

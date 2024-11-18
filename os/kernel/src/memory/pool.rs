use alloc::vec::Vec;
use core::alloc::Layout;
use core::any::{TypeId, type_name};
use core::array;
use core::mem;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use linked_list_allocator::LockedHeap;
use log::info;
use crate::memory::global_persistent_allocator::qemu_exit;

const POOL_MAGIC: u64 = 0x4433_4F53_504F4F4C; // "D3OS_POOL" in hex
const MAX_OBJECT_ENTRIES: usize = 1024; //~88KB for table

// #[repr(u8)]
// #[derive(Debug, Copy, Clone)]
// pub enum Operation {
//     Allocation = 1,
//     Deallocation = 2,
//     Modification = 3,
//     ObjectTableUpdate = 4,
// }

//Dogshit
// #[repr(C)]
// #[derive(Debug)]
// pub struct UndoLog {
//     valid: AtomicBool,
//     operation: Operation,
//     offset: u64,// Ref to the objectTableEntry
//     size: usize,
//     type_hash: u64,
//     checksum: u32,
//     old_data: [u8; 4096],
// }

#[repr(C)]
pub struct ObjectTableEntry {
    valid: AtomicBool, // Data is valid (was written)
    active: AtomicBool, // Data is active (now usable) -> both has to be active
    operation_done: AtomicBool,
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
        let heap_size = size - mem::size_of::<ObjectTableEntry>() * MAX_OBJECT_ENTRIES - mem::size_of::<PoolHeader>();

        let mut pool = Self {
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

                core::arch::x86_64::_mm_sfence();
                core::arch::x86_64::_mm_clflush(header as *const u8);
                core::arch::x86_64::_mm_sfence();

                pool.heap.lock().init(
                    (base + heap_offset as u64) as *mut u8,
                    heap_size
                );
            } else {
                info!("Reusing existing pool at 0x{:x}", base);
                //TODO: Recover
            }
        }

        pool.print_metadata_debug_info();
        pool
    }

    // Transaction handling
    pub fn transaction<F, R>(&mut self, f: F) -> Result<R, PoolError>
    where
        F: FnOnce(&mut TransactionContext) -> Result<R, PoolError>,
    {
        // Always check for recovery at start of transaction
        self.recover()?;

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
                Ok(result)
            }
            Err(e) => Err(e)
        }
    }

    fn recover(&mut self) -> Result<(), PoolError> {
        unsafe {
            let table_base = (self.base_address + (*self.header).object_table_offset)
                as *mut ObjectTableEntry;

            for i in 0..(*self.header).max_objects {
                let entry = &*table_base.add(i);
                // If we find an entry that's active but not valid,
                // it means we crashed during transaction commit
                //TODO: Hier kann ich einfach setzen, denn das letze was passiert ist war das active gesetzt wurde
                // Idee zustand gucken und ob vollständig
                // Cases durchgehen:
                // 1. Active und Valid und opertion_done -> alles gut
                // 2. Active und Valid und nicht operation_done -> nicht möglich
                // 3. Active und nicht Valid und operation_done -> Entweder Alloc oder modify
                if entry.active.load(Ordering::Acquire) && !entry.valid.load(Ordering::Acquire) {

                    info!("Recovering object with ID: {}", core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]));
                    if entry.operation_done.load(Ordering::Acquire) {
                        entry.valid.store(true, Ordering::Release);//TODO: Absprechen
                    }
                    else {
                        //wert deaktivieren... kann nur neu gemacht werden über allocate!
                        entry.active.store(false, Ordering::Release);//TODO: Absprechen
                    }


                }
            }
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
            core::arch::x86_64::_mm_sfence();
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
                let entry = &*table_base.add(i);
                if entry.active.load(Ordering::Acquire) {
                    let id_str = core::str::from_utf8_unchecked(&entry.id[..entry.id_len as usize]);
                    info!("Entry #{}", i);
                    info!("  Active: {}", entry.active.load(Ordering::Acquire));
                    info!("  Valid: {}", entry.valid.load(Ordering::Acquire));
                    info!("  Operation Done: {}", entry.operation_done.load(Ordering::Acquire));
                    info!("  ID: {}", id_str);
                    info!("  Type Hash: 0x{:x}", entry.type_hash);
                    info!("  Type Size: {} bytes", entry.type_size);
                    info!("  Data Pointer: {:?}", entry.data);
                }
            }
        }
    }
}

//TODO: Wichtig für Thesis später
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
        if id.len() > 55 {
            return Err(PoolError::InvalidId);
        }

        // First check if object exists and is valid
        match self.get_by_id::<T>(id) {
            Ok(ptr) => {
                // Object exists and is valid, modify it
                self.modify(ptr, |existing| *existing = data)?;
                Ok(ptr)
            }
            Err(PoolError::InvalidId) => {
                // Create new object
                unsafe {

                    // Find free or inactive entry
                    let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset)
                        as *mut ObjectTableEntry;

                    let mut free_entry = None;
                    for i in 0..(*self.pool.header).max_objects {
                        let entry = &mut *table_base.add(i);
                        if !entry.active.load(Ordering::Acquire) {
                            free_entry = Some(entry);
                            break;
                        }
                    }

                    let entry = free_entry.ok_or(PoolError::ObjectTableFull)?;

                    // Prepare entry
                    entry.active.store(false, Ordering::Release);
                    entry.valid.store(false, Ordering::Release);
                    entry.operation_done.store(false, Ordering::Release);
                    entry.id[..id.len()].copy_from_slice(id.as_bytes());
                    entry.id_len = id.len() as u8;
                    Pool::flush_cache_line(entry as *const _ as *const u8);

                    let ptr = self.pool.heap.lock()
                        .allocate_first_fit(Layout::new::<T>())
                        .map_err(|_| PoolError::AllocationFailed)?;
                    //TODO: Wenn hier ein Fehler passiert, dann ist ptr weg!
                    // deshalb:
                    entry.data = Some(ptr);
                    Pool::flush_cache_line(entry as *const _ as *const u8);

                    //qemu_exit(123);

                    // Falls hier abgebrochen, kann ich später realsen

                    // Write data first
                    ptr::write(ptr.as_ptr() as *mut T, data);
                    Pool::flush_cache_line(ptr.as_ptr() as *const u8);


                    // Prepare entry
                    entry.active.store(true, Ordering::Release);
                    entry.valid.store(false, Ordering::Release);
                    entry.operation_done.store(true, Ordering::Release);
                    //entry.id[..id.len()].copy_from_slice(id.as_bytes());
                    //entry.id_len = id.len() as u8;
                    entry.type_hash = Pool::compute_type_hash::<T>();
                    entry.type_size = mem::size_of::<T>();
                    //entry.data = Some(ptr);
                    Pool::flush_cache_line(entry as *const _ as *const u8);

                    // Add to pending changes
                    self.pending_changes.push(PendingChange {
                        entry,
                        data_ptr: ptr,
                    });

                    Ok(ptr.cast())
                }
            }
            Err(e) => Err(e),
        }
    }

    pub fn get_by_id<T: Copy + 'static>(&self, id: &str) -> Result<NonNull<T>, PoolError> {
        unsafe {
            let table_base = (self.pool.base_address + (*self.pool.header).object_table_offset)
                as *const ObjectTableEntry;

            for i in 0..(*self.pool.header).max_objects {
                let entry = &*table_base.add(i);

                // Only consider entries that are both active AND valid
                if !entry.active.load(Ordering::Acquire) || !entry.valid.load(Ordering::Acquire) {
                    continue;
                }

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
            (*entry).valid.store(false, Ordering::Release);
            (*entry).operation_done.store(false, Ordering::Release);
            Pool::flush_cache_line(entry as *const _ as *const u8);

            //  TODO: HIER NOCHMAL über cacheflush nachdenken!
            // Modify data
            f(&mut *ptr.as_ptr());
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

    //TODO: Ungeprüft
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
                        self.pool.heap.lock().deallocate(
                            ptr,
                            Layout::from_size_align(entry.type_size, 8)
                                .map_err(|_| PoolError::AllocationFailed)?
                        );
                    }

                    // Mark entry as inactive
                    entry.active.store(false, Ordering::Release);
                    entry.valid.store(false, Ordering::Release);
                    entry.operation_done.store(true, Ordering::Release);
                    Pool::flush_cache_line(entry as *const _ as *const u8);

                    //put to pending changes
                    self.pending_changes.push(PendingChange {
                        entry,
                        data_ptr: NonNull::dangling(),
                    });

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

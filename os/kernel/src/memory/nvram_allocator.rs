use super::global_persistent_allocator::{ALLOCATOR_MAGIC, GlobalPersistentAllocator};
use crate::memory::nvmem::Locked;
use x86_64::instructions::port::Port;
use x86_64::structures::paging::frame::PhysFrameRange;

pub struct NvramAllocator {
    global_allocator: Locked<Option<GlobalPersistentAllocator>>,
}

impl NvramAllocator {
    pub const fn new() -> Self {
        NvramAllocator {
            global_allocator: Locked::new(None),
        }
    }

    pub fn init(&self, range: &PhysFrameRange) {
        let mut allocator = self.global_allocator.lock();
        if allocator.is_none() {
            let base_address = range.start.start_address().as_u64();
            let size = (range.end.start_address().as_u64() - base_address) as usize;

            *allocator = Some(GlobalPersistentAllocator::new(base_address, size));
        }
    }

    pub fn create_pool(&self, name: impl AsRef<[u8]>, size: usize) -> Option<u64> {
        if let Some(allocator) = &mut *self.global_allocator.lock() {
            allocator.allocate_pool(name.as_ref(), size)
        } else {
            None
        }
    }

    pub fn find_pool(&self, name: impl AsRef<[u8]>) -> Option<(u64, usize)> {
        if let Some(allocator) = &*self.global_allocator.lock() {
            allocator.find_pool(name.as_ref())
        } else {
            None
        }
    }
}

// unsafe impl Allocator for NvramAllocator {
//     fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
//         info!("Allocating memory with layout: {:?}", layout);
//         if layout.size() == 0 {
//             info!("Allocating zero size memory");
//             return Ok(NonNull::slice_from_raw_parts(layout.dangling(), 0));
//         }
//
//         match self.heap.lock().allocate_first_fit(layout) {
//             Ok(ptr) => {
//                 info!("Allocated memory at: {:?}, size: {}", ptr, layout.size());
//                 Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
//             }
//             Err(_) => Err(AllocError),
//         }
//     }
//
//     unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
//         info!("Deallocating memory at: {:?}, size: {}", ptr, layout.size());
//         if layout.size() != 0 {
//             let mut heap = self.heap.lock();
//             heap.deallocate(ptr, layout);
//         }
//     }
// }

//testing atomic transactions with qemu exit

pub(crate) fn qemu_exit(exit_code: u32) -> ! {
    unsafe {
        let mut port = Port::new(0xf4);
        port.write(exit_code as u32);
    }
    loop {}
}

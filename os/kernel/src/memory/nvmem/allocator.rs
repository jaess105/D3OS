use core::{
    alloc::Layout,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize},
};

use alloc::alloc::{AllocError, Allocator};
use goblin::elf64::reloc::R_OR1K_TLS_DTPMOD;
use linked_list_allocator::LockedHeap;
use log::{info, warn};
use x86_64::structures::paging::frame::PhysFrameRange;

use crate::{
    acpi_tables, efi_services_available,
    memory::{PAGE_SIZE, nvmem::Nfit},
};
use uefi::runtime::Time;

/// As a demo for NVRAM support, we read the last boot time from NVRAM and write the current boot time to it
pub fn original_demo() {
    match acpi_tables().lock().find_table::<Nfit>() {
        Ok(nfit) => {
            if let Some(range) = nfit.get_phys_addr_ranges().first() {
                let date_ptr = range.as_phys_frame_range().start.start_address().as_u64() as *mut Time;

                // Read last boot time from NVRAM
                let date = unsafe { date_ptr.read() };
                if date.is_valid().is_ok() {
                    info!(
                        "Last boot time: [{:0>4}-{:0>2}-{:0>2} {:0>2}:{:0>2}:{:0>2}]",
                        date.year(),
                        date.month(),
                        date.day(),
                        date.hour(),
                        date.minute(),
                        date.second()
                    );
                }

                // Write current boot time to NVRAM
                if efi_services_available() {
                    if let Ok(time) = uefi::runtime::get_time() {
                        unsafe { date_ptr.write(time) }
                    }
                }
            }
        }
        Err(e) => {
            warn!("Error when trying to find nfit acpi table for nvmem demo. Error was {e:?}");
        }
    }
}

static FREE_BYTES: AtomicUsize = AtomicUsize::new(0); // number of bytes currently in the pipe

pub struct NmemAllocator {
    heap: LockedHeap,
    initialized: AtomicBool,
}

impl NmemAllocator {
    pub const fn new() -> Self {
        Self {
            heap: LockedHeap::empty(),
            initialized: AtomicBool::new(false),
        }
    }

    pub unsafe fn init(&self, frames: &PhysFrameRange) {
        todo!()
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(core::sync::atomic::Ordering::SeqCst)
    }

    pub fn is_locked(&self) -> bool {
        self.heap.is_locked()
    }

    pub fn fetch(&self) {
        todo!()
    }
}

unsafe impl Allocator for NmemAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        todo!()
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        todo!()
    }
}

struct NamedBumpAllocator {}

struct Entry;

/// As a demo for NVRAM support, we read the last boot time from NVRAM and write the current boot time to it
pub fn allocator_demo() {
    match acpi_tables().lock().find_table::<Nfit>() {
        Ok(nfit) => {
            if let Some(range) = nfit.get_phys_addr_ranges().first() {
                let frames = range.as_phys_frame_range();

                let allocator = NmemAllocator::new();
                if allocator.is_initialized() {
                    warn!("Allocator was already initialized. This is unexpected!");
                    panic!();
                }

                unsafe {
                    allocator.init(&frames);
                }

                //     // Read last boot time from NVRAM
                //     let date = unsafe { start_addr.read() };
                //     if date.is_valid().is_ok() {
                //         info!(
                //             "Last boot time: [{:0>4}-{:0>2}-{:0>2} {:0>2}:{:0>2}:{:0>2}]",
                //             date.year(),
                //             date.month(),
                //             date.day(),
                //             date.hour(),
                //             date.minute(),
                //             date.second()
                //         );
                //     }

                //     // Write current boot time to NVRAM
                //     if efi_services_available() {
                //         if let Ok(time) = uefi::runtime::get_time() {
                //             unsafe { start_addr.write(time) }
                //         }
                //     }
            }
        }
        Err(e) => {
            warn!("Error when trying to find nfit acpi table for nvmem demo. Error was {e:?}");
        }
    }
}

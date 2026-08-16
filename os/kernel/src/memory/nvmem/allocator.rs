use core::{
    alloc::Layout,
    fmt,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize},
};

use alloc::{
    alloc::{AllocError, Allocator},
    vec::Vec,
};
use linked_list_allocator::LockedHeap;
use log::{info, warn};
use x86_64::structures::paging::frame::PhysFrameRange;

use crate::{
    acpi_tables, efi_services_available,
    memory::nvmem::{
        Nfit,
        named_bump_allocator::{
            self, NamedBumpAllocator, NvmemError, NvmemResult, SuperblockBehavior,
            alloc_name::{Name, make_name},
        },
    },
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

/// As a demo for NVRAM support, we read the last boot time from NVRAM and write the current boot time to it
pub fn allocator_demo() {
    match acpi_tables().lock().find_table::<Nfit>() {
        Ok(nfit) => {
            if let Some(range) = nfit.get_phys_addr_ranges().first() {
                let start = range.as_phys_frame_range().start.start_address().as_u64();
                let end = {
                    let end_frame = range.as_phys_frame_range().end;
                    let end_start = end_frame.start_address();
                    let end_size = end_frame.size();
                    (end_start + end_size).as_u64()
                };
                let len = end - start;

                info!("NVMEM allocator start: {}; len: {}", start, len);
                let allocator = unsafe { named_bump_allocator::init(start as *mut u8, len, SuperblockBehavior::Throw) };

                const BOOT_TIME: Name = make_name(b"boot_time");

                let dates_opt = allocator.get::<[Option<Time>; 5]>(BOOT_TIME);
                if let Some(dates) = dates_opt {
                    for (idx, date) in dates.iter().enumerate() {
                        if let Some(date) = date {
                            if date.is_valid().is_ok() {
                                info!(
                                    "Last {} boot time: [{:0>4}-{:0>2}-{:0>2} {:0>2}:{:0>2}:{:0>2}]",
                                    idx,
                                    date.year(),
                                    date.month(),
                                    date.day(),
                                    date.hour(),
                                    date.minute(),
                                    date.second()
                                );
                            }
                        }
                    }
                } else {
                    info!("Last boot time not found");
                }

                if efi_services_available() {
                    if let Ok(time) = uefi::runtime::get_time() {
                        let mut dates = dates_opt.unwrap_or([None; 5]);

                        for idx in (1..5).rev() {
                            dates[idx] = dates[idx - 1];
                        }
                        dates[0] = Some(time);

                        match on_size_mismatch_retry(allocator, BOOT_TIME, dates) {
                            Err(err) => {
                                warn!("There was an error allocating the boot time! {}", err);
                            }
                            Ok(_) => {
                                info!("Boot time stored!");
                            }
                        }
                    } else {
                        warn!("No boot time stored, could not get time!");
                    }
                } else {
                    warn!("No boot time stored, efi services unavailable!");
                }
            }
        }
        Err(e) => {
            warn!("Error when trying to find nfit acpi table for nvmem demo. Error was {e:?}");
        }
    }
}

fn on_size_mismatch_retry<T: Copy + fmt::Debug>(allocator: NamedBumpAllocator, name: Name, element: T) -> NvmemResult<T> {
    let error = allocator.alloc(name, element);
    match error {
        Err(NvmemError::SizeMismatch) => {
            info!("Size mismatch, retrying");
            allocator.dealloc(name)?;
            allocator.alloc(name, element)
        }
        other => other,
    }
}

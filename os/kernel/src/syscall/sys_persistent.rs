use core::ptr::slice_from_raw_parts;
use log::info;
use crate::persistent_allocator;

pub fn sys_create_persistent_pool(name_ptr: *const u8, name_len: usize) -> isize {
    // Validate pointer before using it
    if name_ptr.is_null() {
        info!("Null pointer received");
        return -1;
    }

    if name_len < 1 {
        return -1;
    }

    info!("Creating persistent pool, ptr: {:?}, len: {}", name_ptr, name_len);

    //TODO: Evtl noch bauen
    // Validate address range
    // if !is_valid_user_range(name_ptr as usize, name_len) {
    //     info!("Invalid memory range");
    //     return -1;
    // }

    let name = unsafe { slice_from_raw_parts(name_ptr, name_len).as_ref().unwrap() };
    info!("Pool name: {:?}", core::str::from_utf8(name));

    let mut allocator = persistent_allocator().write();
    match allocator.get_or_create_pool(name) {
        Some(_) => 0,
        None => -1,
    }
}

pub fn sys_release_persistent_pool(name_ptr: *const u8, name_len: usize) -> isize {
    let name = unsafe { slice_from_raw_parts(name_ptr, name_len).as_ref().unwrap() };

    let mut allocator = persistent_allocator().write();
    if allocator.release_pool(name) {
        0
    } else {
        -1
    }
}
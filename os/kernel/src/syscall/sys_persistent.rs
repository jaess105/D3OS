use core::alloc::Layout;
use core::ptr::slice_from_raw_parts;
use log::info;
use crate::memory::pool::Pool;
use crate::persistent_allocator;

//This just creates/finds the pool and returns success
pub fn sys_create_persistent_pool(name_ptr: *const u8, name_len: usize) -> isize {
    if name_ptr.is_null() || name_len < 1 {
        return -1;
    }

    //info!("Creating persistent pool, ptr: {:?}, len: {}", name_ptr, name_len);
    let name = unsafe { slice_from_raw_parts(name_ptr, name_len).as_ref().unwrap() };
    //info!("Pool name: {:?}", core::str::from_utf8(name));

    let mut allocator = persistent_allocator().write();
    match allocator.get_or_create_pool(name) {
        Ok(_) => 0,  // Just verify pool exists/created
        Err(_) => -1,
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

fn usize_to_str(mut num: usize, buffer: &mut [u8]) -> &str {
    let mut i = buffer.len();

    // Fill the buffer with digits in reverse order
    while num > 0 {
        i -= 1;
        buffer[i] = b'0' + (num % 10) as u8;
        num /= 10;
    }

    // Handle the case where the number is 0
    if i == buffer.len() {
        i -= 1;
        buffer[i] = b'0';
    }

    // Create a &str from the buffer
    unsafe { core::str::from_utf8_unchecked(&buffer[i..]) }
}

pub fn sys_perform_transaction(
    pool_name_ptr: *const u8,
    pool_name_len: usize,
    data_ptr: *const u8,
    data_len: usize,
    id: usize,
) -> isize {
    if pool_name_ptr.is_null() || data_ptr.is_null() {
        return -1;
    }

    let pool_name = unsafe { slice_from_raw_parts(pool_name_ptr, pool_name_len).as_ref().unwrap() };
    let data = unsafe { slice_from_raw_parts(data_ptr, data_len).as_ref().unwrap() };
    info!("Data is {:?}", data);

    let mut buffer = [0u8; 20]; // Buffer to hold up to 20 digits
    let id_name = usize_to_str(id, &mut buffer);

    let mut allocator = persistent_allocator().write();

    match allocator.get_or_create_pool(pool_name) {
        Ok(pool) => {
            match pool.transaction(|tx| {
                tx.allocate_with_id(id_name, data).expect("TODO: panic message");
                Ok(())
            }) {
                Ok(_) => {
                    pool.debug_print_object_table();
                    0
                }
                Err(_) => -1,
            }
        }
        Err(_) => -2,
    }
}
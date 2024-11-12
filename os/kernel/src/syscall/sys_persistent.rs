use core::alloc::Layout;
use core::ptr::slice_from_raw_parts;
use log::info;
use crate::memory::pool::Pool;
use crate::persistent_allocator;

// This just creates/finds the pool and returns success
pub fn sys_create_persistent_pool(name_ptr: *const u8, name_len: usize) -> isize {
    if name_ptr.is_null() || name_len < 1 {
        return -1;
    }

    info!("Creating persistent pool, ptr: {:?}, len: {}", name_ptr, name_len);
    let name = unsafe { slice_from_raw_parts(name_ptr, name_len).as_ref().unwrap() };
    info!("Pool name: {:?}", core::str::from_utf8(name));

    let mut allocator = persistent_allocator().write();
    match allocator.get_or_create_pool(name) {
        Some(_) => 0,  // Just verify pool exists/created
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

// pub fn sys_perform_transaction(
//     pool_name_ptr: *const u8,
//     pool_name_len: usize,
//     data_ptr: *const u8,
//     data_len: usize
// ) -> isize {
//     if pool_name_ptr.is_null() || data_ptr.is_null() {
//         return -1;
//     }
//
//     let pool_name = unsafe { slice_from_raw_parts(pool_name_ptr, pool_name_len).as_ref().unwrap() };
//     let data = unsafe { slice_from_raw_parts(data_ptr, data_len).as_ref().unwrap() };
//
//     let mut allocator = persistent_allocator().write();
//
//     #[repr(C)]
//     #[derive(Copy, Clone)]
//     struct PoolData {
//         size: usize,
//         data: [u8; 32],//TODO:32 zeichen erlauben
//     }
//
//     match allocator.get_or_create_pool(pool_name) {
//         Some(pool) => {
//             match pool.transaction(|tx| {
//
//                 let layout = Layout::new::<PoolData>();
//                 // TODO: nur array bisher, je nach Zeit erweitern
//                 let ptr = tx.allocate::<PoolData>(layout)?;
//
//                 tx.modify(ptr, |pool_data| {
//                     pool_data.size = data_len;
//                     pool_data.data[..data_len].copy_from_slice(data);
//                 })?;
//
//                 Ok(())
//             }) {
//                 Ok(_) => 0,
//                 Err(_) => -1,
//             }
//         }
//         None => -2,
//     }
// }
#![no_std]

use syscall::{SystemCall, syscall};

// pub fn create_persistent_pool(name: &str) -> Result<(), &'static str> {
//     match syscall(SystemCall::CreatePersistentPool, &[
//         name.as_ptr() as usize,
//         name.len(),
//         0,
//         0,
//         0,
//     ]) {
//         Ok(_) => Ok(()),
//         Err(_) => Err("Failed to create pool"),
//     }
// }
//
// pub fn release_persistent_pool(name: &str) -> Result<(), &'static str> {
//     match syscall(SystemCall::ReleasePersistentPool, &[
//         name.as_ptr() as usize,
//         name.len(),
//         0,
//         0,
//         0,
//     ]) {
//         Ok(_) => Ok(()),
//         Err(_) => Err("Failed to release pool"),
//     }
// }

// pub fn perform_transaction(pool_name: &str, data: &[u8]) -> Result<(), &'static str> {
//     match syscall(
//         SystemCall::PerformTransaction,
//         &[
//             pool_name.as_ptr() as usize,
//             pool_name.len(),
//             data.as_ptr() as usize,
//             data.len(),
//             0,
//         ],
//     ) {
//         Ok(_) => Ok(()),
//         Err(_) => Err("Transaction failed"),
//     }
// }

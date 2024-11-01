#![no_std]

extern crate alloc;

#[allow(unused_imports)]
use runtime::*;
use terminal::{print, println};
use syscall::{syscall, SystemCall};

#[unsafe(no_mangle)]
pub fn main() {

    let pool_name = b"simon";

    match syscall(
        SystemCall::CreatePersistentPool,
        &[
            pool_name.as_ptr() as usize,
            pool_name.len(),
            0, 0, 0
        ]
    ) {
        Ok(_) => println!("Successfully created/accessed pool 'test_pool'"),
        Err(e) => println!("Failed to create pool: {:?}", e),
    }
}
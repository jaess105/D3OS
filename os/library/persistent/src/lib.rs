#![no_std]

use syscall::{SystemCall, syscall};

pub fn create_persistent_pool(name: &str) -> Result<(), &'static str> {
    match syscall(SystemCall::CreatePersistentPool, &[
        name.as_ptr() as usize,
        name.len(),
        0,
        0,
        0,
    ]) {
        Ok(_) => Ok(()),
        Err(_) => Err("Failed to create pool"),
    }
}

pub fn release_persistent_pool(name: &str) -> Result<(), &'static str> {
    match syscall(SystemCall::ReleasePersistentPool, &[
        name.as_ptr() as usize,
        name.len(),
        0,
        0,
        0,
    ]) {
        Ok(_) => Ok(()),
        Err(_) => Err("Failed to release pool"),
    }
}

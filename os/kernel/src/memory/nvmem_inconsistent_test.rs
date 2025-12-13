use core::ptr;

use alloc::vec::Vec;
use alloc::{slice, vec};
use log::info;

use crate::syscall::sys_concurrent::sys_thread_sleep;

// layout:
// [0..8)    magic
// [8..16)   payload_len (u64 little endian)
// [16..20)  crc32 (u32 little endian)
// [4096..]  payload ( payload aligned to start of page boundary)
const MAGIC: &[u8; 8] = b"PMEMOBJ1";
const PAYLOAD_LEN: usize = 16 * 1024 * 1024; // 16 MiB object (adjust)

const PAYLOAD_OFFSET: usize = 4096;
const HEADER_OFFSET: usize = 0;

const CHUNK_SIZE: usize = 4096 * 16; // 64 KiB per chunk
const DELAY_MS: u64 = 200; // 200 ms between chunks
const FLUSH_EACH_CHUNK: bool = false; // toggle flush (msync) per chunk

#[derive(Debug, Clone)]
pub enum NvmiMode<'a> {
    /// Will run a simple program writing into the nvme mem region `{mem_path}`.
    Simple(*mut u8, &'a [u8]),
    /// Will write an "object" of size `{payload_len}` into the nvme mem region `{mem_path}` and delay each time for
    /// `{delay_ms}` milliseconds between `{chunk_size}` many bytes written into memory.
    /// This allows for simulating a crash during writing into memory.
    PMemWrite(*mut u8, usize, usize, usize, bool),
    /// Will check if the memory in the nvme mem region `{mem_path}` is consistent.
    /// This will try to detect, weather a write was interrupted or inconsistent many bytes were written into memory.
    /// This should be run after pmem-write was run and the system was lead to a crash.
    PMemCheck(*mut u8),
    /// Does nothing
    Nothing,
}

pub fn run(mode: NvmiMode) {
    match mode {
        NvmiMode::Simple(addr, msg) => {
            info!("NVMI: Running simple writing!");
            simple_write_to_nvme(addr, msg);
        }
        NvmiMode::PMemWrite(addr, payload_len, chunk_size, delay_ms, flush_each_chunk) => {
            info!("NVMI: Running pmem writer!");
            pmem_write(addr, payload_len, chunk_size, delay_ms, flush_each_chunk);
        }
        NvmiMode::PMemCheck(addr) => {
            info!("NVMI: Running pmem checker!");
            pmem_check(addr);
        }
        NvmiMode::Nothing => {
            info!("NVMI: Running nothing!");
        }
    }
}

fn pmem_check(addr: *mut u8) {
    unsafe {
        let header_ptr = addr as *const u8;
        let magic_present = slice::from_raw_parts(header_ptr, 8);
        if magic_present != MAGIC {
            panic!("No object header found (magic mismatch). Device likely zero or unrelated data.");
        }

        let len_ptr = header_ptr.add(8) as *const u64;
        let payload_len = u64::from_le(*len_ptr) as usize;
        let crc_ptr = header_ptr.add(16) as *const u32;
        let expected_crc = u32::from_le(*crc_ptr);
        info!(
            "Found object header. declared payload bytes: {}, expected CRC32: {:08x}",
            payload_len, expected_crc
        );

        let payload_ptr = header_ptr.add(PAYLOAD_OFFSET);
        let present = check_chunks(payload_ptr, payload_len, CHUNK_SIZE);

        info!("Contiguous non-zero bytes from payload start: {}", present);

        // compute crc over the contiguous region present
        let expected_slice = slice::from_raw_parts(payload_ptr, payload_len);
        let present_slice = slice::from_raw_parts(addr, payload_len);

        if expected_slice == present_slice {
            info!("Object is fully present and CRC matches: consistent!");
        } else if present == 0 {
            info!("No payload data present (all zeros).");
        } else {
            info!("Partial or corrupted write detected: {} / {} bytes present", present, payload_len);
            if present == payload_len {
                info!("All bytes present but CRC mismatch -> corruption.");
            }
        }
    }
}

fn check_chunks(addr: *const u8, payload_len: usize, chunk_size: usize) -> usize {
    let payload_ptr = unsafe { addr.add(PAYLOAD_OFFSET) };
    let mut offset = 0;

    while offset < payload_len {
        let this_chunk = chunk_size.min(payload_len - offset);
        let actual_slice = unsafe { slice::from_raw_parts(payload_ptr.add(offset), this_chunk) };

        // generate expected chunk
        let expected_slice: Vec<u8> = (offset..offset + this_chunk).map(|i| (i & 0xFF) as u8).collect();

        if actual_slice != expected_slice.as_slice() {
            info!("Chunk at offset {} is inconsistent! {} bytes differ", offset, this_chunk);
            break;
        }

        offset += this_chunk;
    }

    offset
}

fn pmem_write(addr: *mut u8, payload_len: usize, chunk_size: usize, delay_ms: usize, flush_each_chunk: bool) {
    // prepare payload (deterministic pattern)
    let mut payload = vec![0u8; payload_len];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }

    // compute CRC for the full payload (we do this before writing)
    unsafe {
        // Zero out the entire payload to keep the results consistent inconsistent even after executing this multiple times.
        zero_out_unsafe(slice::from_raw_parts_mut(addr as *mut u8, payload_len), payload_len, addr);

        // Write header (in-memory copy)
        let header_ptr = (addr as *mut u8).add(HEADER_OFFSET);
        ptr::copy_nonoverlapping(MAGIC.as_ptr(), header_ptr, MAGIC.len());
        // write payload length
        let len_ptr = header_ptr.add(8) as *mut u64;
        *len_ptr = payload_len as u64;
        // write crc32
        let crc_ptr = header_ptr.add(16) as *mut u32;
        // the crc pointer was supposed to hold the hash of our object.
        // But there is no hasher in the kernel, so this is skipped.
        *crc_ptr = 0;

        // flush header immediately
        msync(header_ptr as *mut _);
        info!("Header flushed");

        // Write payload in chunks
        let mut written = 0usize;
        while written < payload_len {
            let to_write = chunk_size.min(payload_len - written);
            let dst = (addr as *mut u8).add(PAYLOAD_OFFSET + written);
            let src = payload[written..written + to_write].as_ptr();
            ptr::copy_nonoverlapping(src, dst, to_write);

            written += to_write;
            info!("Wrote chunk, total written: {} / {}", written, payload_len);

            if flush_each_chunk {
                // flush the region we just wrote
                msync(dst as *mut _);
                info!("msync chunk done");
            }

            // sleep so you have time to kill QEMU from the host
            sys_thread_sleep(delay_ms);
        }

        info!("Finished writing payload. Final msync of payload.");
        msync((addr as *mut u8).add(PAYLOAD_OFFSET) as *mut _);
    }

    info!("Done. If you want to simulate a crash, re-run with a shorter delay and kill QEMU during the loop.");
}

fn simple_write_to_nvme(addr: *mut u8, msg: &[u8]) {
    let size = 4096; // just map 4 KiB

    unsafe {
        let slice = slice::from_raw_parts_mut(addr as *mut u8, size);
        zero_out_unsafe(slice, size, addr);

        slice[..msg.len()].copy_from_slice(msg);
        info!("Wrote message.");

        // Flush CPU cache lines to pmem (best-effort)
        msync(addr);
    }
}

unsafe fn zero_out_unsafe(slice: &mut [u8], _: usize, addr: *mut u8) {
    slice.fill(0);
    msync(addr);
}

// pub(crate) fn simple_write(address: u64, msg: &[u8; 54]) {
//     unsafe {
//         let pmem_addr = address as *mut u8;
//         let pmem_slice = slice::from_raw_parts_mut(pmem_addr, msg.len() + 10);

//         pmem_slice[..msg.len()].copy_from_slice(msg);
//     }
// }

fn msync(addr: *mut u8) {
    unsafe {
        core::arch::x86_64::_mm_sfence();
        core::arch::x86_64::_mm_clflush(addr);
        core::arch::x86_64::_mm_sfence();
    }
}

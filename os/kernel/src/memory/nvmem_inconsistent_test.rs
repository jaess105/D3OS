use core::{ptr, u8};

use alloc::vec::Vec;
use alloc::{slice, vec};
use log::{error, info};

use crate::syscall::sys_concurrent::sys_thread_sleep;

// layout:
// [0..8)    magic
// [8..16)   payload_len (u64 little endian)
// [16..20)  crc32 (u32 little endian)
// [16..]  payload
const MAGIC: &[u8; 8] = b"PMEMOBJ1";
const PAYLOAD_LEN: usize = 16 * 1024 * 1024; // 16 MiB object 

const PAYLOAD_OFFSET: usize = size_of::<Meta>();
const HEADER_OFFSET: usize = 0;

pub const CHUNK_SIZE: usize = 4096 * 16; // 64 KiB per chunk
pub const DELAY_MS: usize = 200; // 200 ms between chunks

#[repr(C)]
#[derive(Debug)]
struct Meta {
    magic: [u8; 8],
    payload_len: usize,
    payload_offset: usize,
}

impl Meta {
    fn read_meta_at_address(address: usize) -> &'static Self {
        unsafe { &*(address as *const Meta) }
    }

    fn write_at_address(address: usize, magic: &[u8; 8], payload_len: usize, payload_offset: usize) {
        let meta = address as *mut Meta;
        unsafe {
            (*meta).magic.copy_from_slice(magic);
            (*meta).payload_len = payload_len;
            (*meta).payload_offset = payload_offset;
        }
    }

    fn write_meta_at_address(address: usize, data: Meta) {
        unsafe { ptr::write(address as *mut Meta, data) };
    }

    fn payload_ptr(&self) -> usize {
        let addr = (self as *const Meta) as usize;
        addr + self.payload_offset
    }
}

#[derive(Debug, Clone)]
pub enum NvmiMode<'a> {
    /// Will run a simple program writing into the nvme mem region `{mem_path}`.
    Simple(*mut u8, &'a [u8]),
    /// Will write an "object" of size `{payload_len}` into the nvme mem region at `{addr}` and delay each time for
    /// `{delay_ms}` milliseconds between `{chunk_size}` many bytes written into memory.
    /// This allows for simulating a crash during writing into memory.
    PMemWrite(usize, usize, usize, usize, bool),
    /// Will check if the memory in the nvme mem region `{mem_path}` is consistent.
    /// This will try to detect, weather a write was interrupted or inconsistent many bytes were written into memory.
    /// This should be run after pmem-write was run and the system was lead to a crash.
    PMemCheck(usize, usize),
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
            pmem_write(addr as *mut u8, payload_len, chunk_size, delay_ms, flush_each_chunk);
        }
        NvmiMode::PMemCheck(addr, chunk_size) => {
            info!("NVMI: Running pmem checker!");
            pmem_check(addr, chunk_size);
        }
        NvmiMode::Nothing => {
            info!("NVMI: Running nothing!");
        }
    }
}

fn pmem_check(addr: usize, chunk_size: usize) {
    unsafe {
        let meta = Meta::read_meta_at_address(addr + HEADER_OFFSET as usize);
        if &meta.magic != MAGIC {
            error!("No object header found (magic mismatch). Device likely zero or unrelated data.");
            return;
        }

        info!("Found object header. declared payload bytes: {}", meta.payload_len,);

        let payload_ptr = meta.payload_ptr();
        let consistent_chunks_size = check_chunks(payload_ptr, meta.payload_len, chunk_size);

        info!("Contiguous non-zero bytes from payload start: {}", consistent_chunks_size);

        if consistent_chunks_size == meta.payload_len {
            info!("Object is fully present!");
        } else {
            error!("Object is not fully present!");
        }
    }
}

fn check_chunks(addr: usize, payload_len: usize, chunk_size: usize) -> usize {
    let payload_ptr = addr as *mut u8;
    let mut offset = 0;

    while offset < payload_len {
        let this_chunk = chunk_size.min(payload_len - offset);
        let actual_slice = unsafe { slice::from_raw_parts(payload_ptr.add(offset), this_chunk) };

        // generate expected chunk
        let expected_slice: Vec<u8> = (offset..offset + this_chunk).map(|i| message_at(i)).collect();

        if actual_slice != expected_slice.as_slice() {
            info!("Chunk at offset {} is inconsistent! {} bytes differ", offset, this_chunk);
            break;
        }

        offset += this_chunk;
    }

    offset
}

fn message_at(pos_in_payload: usize) -> u8 {
    const MESSAGE: &[u8; 13] = b"Hello There; ";
    MESSAGE[pos_in_payload % MESSAGE.len()]
}

fn pmem_write(addr: *mut u8, payload_len: usize, chunk_size: usize, delay_ms: usize, flush_each_chunk: bool) {
    // prepare payload (deterministic pattern)
    let mut payload = vec![0u8; payload_len];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = message_at(i);
    }

    // compute CRC for the full payload (we do this before writing)
    unsafe {
        // Zero out the entire payload to keep the results consistent inconsistent even after executing this multiple times.
        zero_out_unsafe(slice::from_raw_parts_mut(addr as *mut u8, payload_len), payload_len, addr);

        // Write header (in-memory copy)
        let meta_addr = (addr as *mut u8).add(HEADER_OFFSET) as usize;
        Meta::write_at_address(meta_addr, MAGIC, payload_len, PAYLOAD_OFFSET);

        // flush header immediately
        msync(meta_addr as *mut _);
        info!("Header flushed");

        let meta = Meta::read_meta_at_address(meta_addr);
        assert_eq!(&meta.magic, MAGIC);
        assert_eq!(meta.payload_len, payload_len);
        assert_eq!(meta.payload_offset, PAYLOAD_OFFSET);

        // Write payload in chunks
        let mut written = 0usize;
        let mut i = 0;
        while written < payload_len {
            info!("Writing chunk {}", i);

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

            info!("Wrote chunk {i} and sleeping for {delay_ms}!");
            // sleep so you have time to kill QEMU from the host
            sys_thread_sleep(delay_ms);
            i += 1;
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

use alloc::vec::Vec;
use core::alloc::Layout;
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::{mem, slice};
use linked_list_allocator::LockedHeap;
use log::info;

const POOL_MAGIC: u64 = 0x4433_4F53_504F4F4C; // "D3OS_POOL" in hex
const PAGE_SIZE: usize = 64;

#[repr(C)]
pub struct LogEntry {
    typ: LogType,
    offset: u64,
    size: usize,
    old_data: Option<*mut u8>, // For modifications
    generation: u32,
    checksum: u32,
}

#[repr(u8)]
#[derive(Debug)]
pub enum LogType {
    Allocation = 1,
    Deallocation = 2,
    Modification = 3,
}

#[repr(C)]
pub struct Transaction {
    valid: AtomicBool,
    generation: AtomicU32,
    logs: Vec<LogEntry>,
}

#[repr(C)]
pub struct PoolHeader {
    magic: u64,
    generation: AtomicU32,
    size: usize,
    used_space: AtomicUsize,

    // Memory management
    heap_start: u64,
    heap_size: usize,

    // Transaction management
    current_transaction: AtomicU64,
    transaction_area: u64,
}

pub struct Pool {
    base: u64,
    header: *mut PoolHeader,
    heap: LockedHeap,
    current_transaction: Option<*mut Transaction>,
}

impl Pool {
    pub fn new(base: u64, size: usize) -> Self {
        let header = base as *mut PoolHeader;
        let heap_offset = align_up(mem::size_of::<PoolHeader>(), PAGE_SIZE);
        let transaction_offset = size - PAGE_SIZE;

        unsafe {
            // Initialize header
            ptr::write(header, PoolHeader {
                magic: POOL_MAGIC,
                generation: AtomicU32::new(0),
                size,
                used_space: AtomicUsize::new(0),
                heap_start: base + heap_offset as u64,
                heap_size: size - heap_offset - PAGE_SIZE,
                current_transaction: AtomicU64::new(0),
                transaction_area: base + transaction_offset as u64,
            });

            // Create pool with empty heap
            let mut pool = Self {
                base,
                header,
                heap: LockedHeap::empty(),
                current_transaction: None,
            };

            // Initialize the heap
            pool.heap.lock().init(
                (base + heap_offset as u64) as *mut u8,
                size - heap_offset - PAGE_SIZE,
            );

            pool
        }
    }

    pub fn transaction<F, R>(&mut self, f: F) -> Result<R, TransactionError>
    where
        F: FnOnce(&mut TransactionContext) -> Result<R, TransactionError>,
    {
        info!("Starting new transaction");
        // Start transaction
        let transaction = self.begin_transaction()?;
        info!("Transaction initialized at address {:p}", transaction);

        let mut context = TransactionContext {
            pool: self,
            transaction,
        };

        // Execute transaction
        match f(&mut context) {
            Ok(result) => {
                info!("Transaction operations completed successfully");
                // Print logs before commit
                self.print_transaction_logs(transaction);

                // Commit transaction
                self.commit_transaction(transaction)?;
                info!("Transaction committed successfully");
                Ok(result)
            }
            Err(e) => {
                info!("Transaction failed, performing rollback");
                self.print_transaction_logs(transaction);
                // Rollback transaction
                self.rollback_transaction(transaction)?;
                info!("Transaction rollback completed");
                Err(e)
            }
        }
    }

    // For Transaction creation, use a reference to LockedHeap
    fn begin_transaction(&mut self) -> Result<*mut Transaction, TransactionError> {
        unsafe {
            let transaction_area = (*self.header).transaction_area as *mut Transaction;

            // Initialize new transaction with empty Vec
            let transaction = Transaction {
                valid: AtomicBool::new(true),
                generation: AtomicU32::new(
                    (*self.header).generation.fetch_add(1, Ordering::AcqRel),
                ),
                logs: Vec::new(), // Start with empty Vec
            };

            ptr::write(transaction_area, transaction);

            // Mark as current transaction
            (*self.header)
                .current_transaction
                .store(transaction_area as u64, Ordering::Release);

            Self::flush_cache_line(transaction_area as *const u8);
            self.current_transaction = Some(transaction_area);

            Ok(transaction_area)
        }
    }

    fn commit_transaction(
        &mut self,
        transaction: *mut Transaction,
    ) -> Result<(), TransactionError> {
        unsafe {
            let transaction = &mut *transaction;

            // Apply all logs
            for log in &transaction.logs {
                match log.typ {
                    LogType::Allocation => {
                        // Allocation already done, just update metadata
                        (*self.header)
                            .used_space
                            .fetch_add(log.size, Ordering::Release);
                    }
                    LogType::Deallocation => {
                        // Actually perform deallocation
                        self.heap.lock().deallocate(
                            NonNull::new_unchecked(log.offset as *mut u8),
                            Layout::from_size_align(log.size, 8).unwrap(),
                        );
                    }
                    LogType::Modification => {
                        // Data already modified, just flush changes
                        Self::flush_cache_line(log.offset as *const u8);
                    }
                }
            }

            // Mark transaction as complete
            transaction.valid.store(false, Ordering::Release);
            Self::flush_cache_line(&transaction.valid as *const _ as *const u8);

            self.current_transaction = None;
            Ok(())
        }
    }

    fn rollback_transaction(
        &mut self,
        transaction: *mut Transaction,
    ) -> Result<(), TransactionError> {
        unsafe {
            let transaction = &mut *transaction;

            // Rollback all logs in reverse order
            for log in transaction.logs.iter().rev() {
                match log.typ {
                    LogType::Allocation => {
                        // Free allocated memory
                        self.heap.lock().deallocate(
                            NonNull::new_unchecked(log.offset as *mut u8),
                            Layout::from_size_align(log.size, 8).unwrap(),
                        );
                    }
                    LogType::Modification => {
                        if let Some(old_data) = log.old_data {
                            // Restore old data
                            ptr::copy_nonoverlapping(old_data, log.offset as *mut u8, log.size);
                            Self::flush_cache_line(log.offset as *const u8);
                        }
                    }
                    _ => {}
                }
            }

            // Mark transaction as invalid
            transaction.valid.store(false, Ordering::Release);
            Self::flush_cache_line(&transaction.valid as *const _ as *const u8);

            self.current_transaction = None;
            Ok(())
        }
    }

    pub fn recover(&mut self) -> Result<(), TransactionError> {
        unsafe {
            let transaction_addr = (*self.header).current_transaction.load(Ordering::Acquire);
            if transaction_addr == 0 {
                return Ok(());
            }

            let transaction = &mut *(transaction_addr as *mut Transaction);
            if transaction.valid.load(Ordering::Acquire) {
                // Unfinished transaction found, roll it back
                self.rollback_transaction(transaction)?;
            }

            Ok(())
        }
    }

    #[inline]
    fn flush_cache_line(ptr: *const u8) {
        unsafe {
            core::arch::x86_64::_mm_clflush(ptr);
            core::arch::x86_64::_mm_sfence();
        }
    }

    fn print_transaction_logs(&self, transaction: *mut Transaction) {
        unsafe {
            let transaction = &*transaction;
            info!("Transaction Log Entries:");
            info!("  Generation: {}", transaction.generation.load(Ordering::Relaxed));
            info!("  Valid: {}", transaction.valid.load(Ordering::Relaxed));

            for (i, log) in transaction.logs.iter().enumerate() {
                info!("  Log Entry {}:", i);
                info!("    Type: {:?}", log.typ);
                info!("    Offset: 0x{:x}", log.offset);
                info!("    Size: {}", log.size);
                info!("    Generation: {}", log.generation);
                if let Some(old_data) = log.old_data {
                    info!("    Has backup data at: {:?}", old_data);
                }
            }
        }
    }
}

pub struct TransactionContext<'a> {
    pool: &'a mut Pool,
    transaction: *mut Transaction,
}

impl<'a> TransactionContext<'a> {
    pub fn allocate<T: Copy>(&mut self, layout: Layout) -> Result<NonNull<T>, TransactionError> {
        //let layout = Layout::new::<T>();

        unsafe {
            // Get locked heap and perform allocation
            let mut heap = self.pool.heap.lock();
            info!("Allocating {} bytes", layout.size());
            let ptr = heap
                .allocate_first_fit(layout)
                .map_err(|_| TransactionError::AllocationFailed)?;
            info!("Allocated at {:p}", ptr.as_ptr());

            // Create log entry
            let log = LogEntry {
                typ: LogType::Allocation,
                offset: ptr.as_ptr() as u64,
                size: layout.size(),
                old_data: None,
                generation: (*self.transaction).generation.load(Ordering::Relaxed),
                checksum: 0,
            };
            info!("Log created");
            // Push to logs Vec
            (*self.transaction).logs.push(log);

            Ok(ptr.cast())
        }
    }

    pub fn modify<T: Copy>(
        &mut self,
        mut ptr: NonNull<T>,
        f: impl FnOnce(&mut T),
    ) -> Result<(), TransactionError> {
        unsafe {
            // Create backup of old data (since T: Copy, we can safely copy it)
            let old_value = ptr.as_ptr().read();

            // Create log entry
            let log = LogEntry {
                typ: LogType::Modification,
                offset: ptr.as_ptr() as u64,
                size: mem::size_of::<T>(),
                old_data: Some(&old_value as *const T as *mut u8),
                generation: (*self.transaction).generation.load(Ordering::Relaxed),
                checksum: 0,
            };

            // Push to logs Vec
            (*self.transaction).logs.push(log);

            // Perform modification
            f(ptr.as_mut());

            // Ensure persistence
            Pool::flush_cache_line(ptr.as_ptr() as *const u8);

            Ok(())
        }
    }
}

#[derive(Debug)]
pub enum TransactionError {
    AllocationFailed,
    TransactionFailed,
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

/* ╔═════════════════════════════════════════════════════════════════════════╗
   ║ Module: wait_queue                                                      ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Wait queues for blocking i/o.                                           ║
   ║                                                                         ║
   ║ Public functions:                                                       ║
   ║   - wait:       Blocks calling thread if the given predicate is true.   ║
   ║   - notify_one: Deblocks one waiting thread (if any).                   ║
   ║   - notify_all: Deblocks all waiting threads (if any).                  ║
   ╟─────────────────────────────────────────────────────────────────────────╢
   ║ Author: Michael Schoettner, Univ. Duesseldorf, 16.02.2026               ║
   ╚═════════════════════════════════════════════════════════════════════════╝
*/

use alloc::collections::VecDeque;
use uuid::Uuid;

use crate::{process::core_local_storage::scheduler, sync::irqsave_spinlock::IrqSaveSpinlock};

pub struct WaitQueue {
    queue: IrqSaveSpinlock<VecDeque<(Uuid, usize)>>,
}

impl WaitQueue {
    pub fn new() -> WaitQueue {
        WaitQueue {
            queue: IrqSaveSpinlock::new(VecDeque::<(Uuid, usize)>::new()),
        }
    }

    /// Block until `pred()` becomes true.
    pub fn wait<F>(&self, mut pred: F, _message: &str)
    where
        F: FnMut() -> bool,
    {
        loop {
            if pred() {
                return;
            }

            {
                let mut guard = self.queue.lock();

                // re-check under lock
                if pred() {
                    return;
                }

                // park after we are visible to notifiers
                let ids = scheduler().park_current();
                guard.push_back(ids);
            }

            scheduler().block_if_parking(|| !pred());
        }
    }

    /// Wake up exactly one waiter (if any). Returns true if someone was woken up.
    pub fn notify_one(&self) -> bool {
        //  info!("WaitQueue::notify_one");

        let mut guard = self.queue.lock();

        while let Some((pid, tid)) = guard.pop_front() {
            if scheduler().unblock(pid, tid) {
                // info!("WaitQueue::notify_one: found a waiter");
                return true;
            }
            // else: stale waiter (killed/exited) -> keep going
        }
        //    info!("WaitQueue::notify_one: no waiter found");

        false
    }

    /// Wake up all waiters currently queued.
    /// Returns the number of threads actually unblocked (stale entries are ignored).
    pub fn notify_all(&self) -> usize {
        let mut guard = self.queue.lock();
        let mut woke = 0;

        while let Some((pid, tid)) = guard.pop_front() {
            if scheduler().unblock(pid, tid) {
                woke += 1;
            }
            // else: stale waiter (killed/exited) -> ignore
        }

        woke
    }
}

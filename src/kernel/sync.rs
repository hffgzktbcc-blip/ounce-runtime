#![allow(dead_code)]

use crate::mmu::Memory;
use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Horizon OS Kernel Event (`KEvent` — Readable/Writable Event Pair)
#[derive(Debug, Clone)]
pub struct KEvent {
    pub handle: u32,
    pub signaled: bool,
    pub auto_clear: bool,
}

/// Horizon OS Synchronization & Address Arbiter (Futex) Manager.
///
/// Implements the kernel-side primitives used by SDK-style code (`nn::os::MutexType`,
/// `nn::os::ConditionVariableType`) and `libnx` (`Mutex`, `CondVar`):
/// - `svcArbitrateLock` (`0x1A`) & `svcArbitrateUnlock` (`0x1B`)
/// - `svcWaitProcessWideKeyAtomic` (`0x1C`) & `svcSignalProcessWideKey` (`0x1D`)
/// - `svcSignalEvent` (`0x11`), `svcClearEvent` (`0x12`), `svcCreateEvent` (`0x45`)
pub struct SyncManager {
    next_event_handle: u32,
    pub events: HashMap<u32, KEvent>,
    /// Maps guest virtual mutex/condvar address -> queue of waiting thread handles
    pub wait_queues: HashMap<u64, VecDeque<u32>>,
    pub total_locks_arbitrated: u64,
    pub total_condvar_signals: u64,
    pub total_events_signaled: u64,
}

impl Default for SyncManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncManager {
    pub fn new() -> Self {
        Self {
            next_event_handle: 0x3000,
            events: HashMap::new(),
            wait_queues: HashMap::new(),
            total_locks_arbitrated: 0,
            total_condvar_signals: 0,
            total_events_signaled: 0,
        }
    }

    pub fn create_event(&mut self, auto_clear: bool) -> u32 {
        let handle = self.next_event_handle;
        self.next_event_handle += 1;
        self.events.insert(
            handle,
            KEvent {
                handle,
                signaled: false,
                auto_clear,
            },
        );
        handle
    }

    pub fn signal_event(&mut self, handle: u32) -> bool {
        if let Some(ev) = self.events.get_mut(&handle) {
            ev.signaled = true;
            self.total_events_signaled += 1;
            true
        } else {
            false
        }
    }

    pub fn clear_event(&mut self, handle: u32) -> bool {
        if let Some(ev) = self.events.get_mut(&handle) {
            ev.signaled = false;
            true
        } else {
            false
        }
    }

    /// `svcArbitrateLock(wait_tag, mutex_addr, requesting_thread_tag)` (`0x1A`)
    /// Atomically acquires or arbitrates a 32-bit user-space Horizon mutex at `mutex_addr`.
    pub fn arbitrate_lock(
        &mut self,
        mmu: &mut Memory,
        wait_tag: u32,
        mutex_addr: u64,
        req_tag: u32,
    ) -> u64 {
        self.total_locks_arbitrated += 1;
        let current = mmu.read_u32(mutex_addr).unwrap_or(0);
        if current == 0 {
            // Uncontended: grant lock to requesting thread tag
            let _ = mmu.write_u32(mutex_addr, req_tag);
            0
        } else if current == wait_tag {
            // Mark waiter bit (0x4000_0000 in Horizon OS mutex word) and enqueue
            let _ = mmu.write_u32(mutex_addr, current | 0x4000_0000);
            self.wait_queues
                .entry(mutex_addr)
                .or_default()
                .push_back(req_tag);
            0
        } else {
            0
        }
    }

    /// `svcArbitrateUnlock(mutex_addr)` (`0x1B`)
    /// Releases the user-space Horizon mutex at `mutex_addr` or hands it to the next waiting thread.
    pub fn arbitrate_unlock(&mut self, mmu: &mut Memory, mutex_addr: u64) -> u64 {
        self.total_locks_arbitrated += 1;
        if let Some(queue) = self.wait_queues.get_mut(&mutex_addr) {
            if let Some(next_owner) = queue.pop_front() {
                let has_more = if queue.is_empty() { 0 } else { 0x4000_0000 };
                let _ = mmu.write_u32(mutex_addr, next_owner | has_more);
                return 0;
            }
        }
        let _ = mmu.write_u32(mutex_addr, 0);
        0
    }

    /// `svcWaitProcessWideKeyAtomic(mutex_addr, condvar_addr, thread_tag, timeout_ns)` (`0x1C`)
    /// Atomically releases `mutex_addr` and enqueues `thread_tag` on `condvar_addr`.
    pub fn wait_process_wide_key_atomic(
        &mut self,
        mmu: &mut Memory,
        mutex_addr: u64,
        condvar_addr: u64,
        thread_tag: u32,
    ) -> u64 {
        // 1. Release user-space mutex
        self.arbitrate_unlock(mmu, mutex_addr);
        // 2. Enqueue waiter on condition variable key
        self.wait_queues
            .entry(condvar_addr)
            .or_default()
            .push_back(thread_tag);
        // 3. Re-acquire mutex for single-stepped continuation
        let _ = mmu.write_u32(mutex_addr, thread_tag);
        0
    }

    /// `svcSignalProcessWideKey(condvar_addr, count)` (`0x1D`)
    /// Wakes up to `count` threads (or all if `count <= 0`) waiting on `condvar_addr`.
    pub fn signal_process_wide_key(
        &mut self,
        mmu: &mut Memory,
        condvar_addr: u64,
        count: i32,
    ) -> u64 {
        self.total_condvar_signals += 1;
        // Increment the user-space condvar counter word
        let val = mmu.read_u32(condvar_addr).unwrap_or(0);
        let _ = mmu.write_u32(condvar_addr, val.wrapping_add(1));

        if let Some(queue) = self.wait_queues.get_mut(&condvar_addr) {
            if count <= 0 {
                queue.clear();
            } else {
                for _ in 0..count {
                    if queue.pop_front().is_none() {
                        break;
                    }
                }
            }
        }
        0
    }
}

/// Horizon OS `SignalType` for `svcSignalToAddress` (`0x35`).
pub const SIGNAL_STATE_MODIFY: u32 = 0;
pub const SIGNAL_MODIFY_IF_EQUAL: u32 = 1;
pub const SIGNAL_MODIFY_BY_WAITING_COUNT_IF_EQUAL: u32 = 2;

/// Horizon OS `ArbitrationType` for `svcWaitForAddress` (`0x34`).
pub const ARBITRATION_WAIT_IF_LESS_THAN: u32 = 0;
pub const ARBITRATION_DECREMENT_AND_WAIT_IF_LESS_THAN: u32 = 1;
pub const ARBITRATION_WAIT_IF_EQUAL: u32 = 2;

struct FutexBucket {
    waiters: Mutex<HashMap<u64, usize>>,
    cvar: Condvar,
}

impl Default for FutexBucket {
    fn default() -> Self {
        Self {
            waiters: Mutex::new(HashMap::new()),
            cvar: Condvar::new(),
        }
    }
}

/// Multi-core OS-thread Futex table backing `svcWaitForAddress` (`0x34`), `svcSignalToAddress` (`0x35`),
/// `svcArbitrateLock` (`0x1A`), and `svcArbitrateUnlock` (`0x1B`) across parallel host worker threads
/// sharing a `FastmemArena`.
pub struct HostFutexTable {
    buckets: [FutexBucket; 32],
}

impl Default for HostFutexTable {
    fn default() -> Self {
        Self::new()
    }
}

impl HostFutexTable {
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| FutexBucket::default()),
        }
    }

    #[inline]
    fn bucket(&self, addr: u64) -> &FutexBucket {
        &self.buckets[((addr >> 2) as usize) & 31]
    }

    /// `svcWaitForAddress(addr, arb_type, value, timeout_ns)` (`0x34`) across OS threads.
    /// Returns `0` (`ResultSuccess`), `0xFA01` (`InvalidState` when predicate failed immediately),
    /// or `0xEA01` (`TimedOut`).
    pub fn wait_for_address(
        &self,
        mmu: &mut Memory,
        addr: u64,
        arb_type: u32,
        value: i32,
        timeout: Option<Duration>,
    ) -> u64 {
        let b = self.bucket(addr);
        let mut guard = b.waiters.lock().unwrap_or_else(|e| e.into_inner());
        let cur = mmu.read_u32(addr).unwrap_or(0) as i32;

        let should_wait = match arb_type {
            ARBITRATION_WAIT_IF_LESS_THAN => cur < value,
            ARBITRATION_DECREMENT_AND_WAIT_IF_LESS_THAN => {
                if cur < value {
                    let _ = mmu.write_u32(addr, cur.wrapping_sub(1) as u32);
                    true
                } else {
                    false
                }
            }
            ARBITRATION_WAIT_IF_EQUAL => cur == value,
            _ => return 0xF001, // InvalidEnumValue
        };

        if !should_wait {
            return 0xFA01; // InvalidState (condition already changed)
        }

        *guard.entry(addr).or_insert(0) += 1;
        let timed_out = match timeout {
            Some(dur) => b
                .cvar
                .wait_timeout(guard, dur)
                .map(|(mut g, res)| {
                    if let Some(w) = g.get_mut(&addr) {
                        *w = w.saturating_sub(1);
                    }
                    res.timed_out()
                })
                .unwrap_or(true),
            None => {
                let mut g = b.cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
                if let Some(w) = g.get_mut(&addr) {
                    *w = w.saturating_sub(1);
                }
                false
            }
        };

        if timed_out { 0xEA01 } else { 0 }
    }

    /// `svcSignalToAddress(addr, signal_type, value, count)` (`0x35`) across OS threads.
    pub fn signal_to_address(
        &self,
        mmu: &mut Memory,
        addr: u64,
        signal_type: u32,
        value: i32,
        count: i32,
    ) -> u64 {
        let b = self.bucket(addr);
        let guard = b.waiters.lock().unwrap_or_else(|e| e.into_inner());
        let cur = mmu.read_u32(addr).unwrap_or(0) as i32;
        let waiting = guard.get(&addr).copied().unwrap_or(0) as i32;

        match signal_type {
            SIGNAL_STATE_MODIFY => {}
            SIGNAL_MODIFY_IF_EQUAL => {
                if cur != value {
                    return 0xFA01; // InvalidState
                }
                let _ = mmu.write_u32(addr, value as u32);
            }
            SIGNAL_MODIFY_BY_WAITING_COUNT_IF_EQUAL => {
                if cur != value {
                    return 0xFA01;
                }
                let remaining_waiters = if count <= 0 {
                    0
                } else {
                    (waiting - count).max(0)
                };
                let next = if waiting == 0 {
                    value.wrapping_add(1)
                } else if remaining_waiters > 0 {
                    value.wrapping_sub(1)
                } else {
                    value
                };
                let _ = mmu.write_u32(addr, next as u32);
            }
            _ => return 0xF001,
        }

        if count == 1 {
            b.cvar.notify_one();
        } else {
            b.cvar.notify_all();
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmu::Permission;

    #[test]
    fn test_horizon_mutex_and_condvar_arbitration() {
        let mut mmu = Memory::new();
        let mut sync = SyncManager::new();

        let mutex_addr = 0x80070000u64;
        let condvar_addr = 0x80070008u64;
        mmu.map_range(mutex_addr, 4096, Permission::READ_WRITE);

        // Thread 1 (tag 0x101) acquires uncontended lock
        sync.arbitrate_lock(&mut mmu, 0, mutex_addr, 0x101);
        assert_eq!(mmu.read_u32(mutex_addr).unwrap(), 0x101);

        // Thread 2 (tag 0x102) contends on lock held by 0x101
        sync.arbitrate_lock(&mut mmu, 0x101, mutex_addr, 0x102);
        // Waiter bit (0x40000000) should now be set on mutex_addr
        assert_eq!(mmu.read_u32(mutex_addr).unwrap(), 0x4000_0101);

        // Thread 1 unlocks -> lock ownership transfers directly to Thread 2 (0x102)
        sync.arbitrate_unlock(&mut mmu, mutex_addr);
        assert_eq!(mmu.read_u32(mutex_addr).unwrap(), 0x102);

        // Signal condvar
        sync.signal_process_wide_key(&mut mmu, condvar_addr, 1);
        assert_eq!(mmu.read_u32(condvar_addr).unwrap(), 1);
        assert_eq!(sync.total_condvar_signals, 1);
    }
}

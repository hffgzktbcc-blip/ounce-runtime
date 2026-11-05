//! Tipc (tiny IPC) sessions of Switch 2 interfaces.
//!
//! A Tipc request has message type 16 + the command id and raw data right after the 8-byte header; there is
//! no CMIF header. A reply without handles is the header followed by the result at +8. A reply with handles
//! has a special header at +8, the handles after it, and the result as the first raw word after the handles.
//! The command meanings below come from the Nintendo SDK client code in the title's `sdk` module (the
//! calling sequence and which reply words it reads), not from public documentation.

use super::ipc::{IpcManager, ServiceType};
use crate::mmu::Memory;

/// Concurrent sessions the server reports it supports.
pub const MAX_SESSIONS: u32 = 8;
/// Name of the interface `ISelfController` command 2000 returns.
pub const SELF_CONTROLLER_SESSION: &str = "ISelfController 2000";
/// Name of the session command 202 of that interface returns.
pub const SELF_CONTROLLER_202: &str = "ISelfController 2000 / 202";
/// Name of the session command 497 of that interface returns.
pub const SELF_CONTROLLER_497: &str = "ISelfController 2000 / 497";
/// Name of the session command 305 of that interface returns.
pub const SELF_CONTROLLER_305: &str = "ISelfController 2000 / 305";

impl IpcManager {
    /// Reply with a result only.
    fn tipc_reply_result(&self, tls: u64, mmu: &mut Memory, result: u32) -> u64 {
        let _ = mmu.write_u32(tls, 0);
        let _ = mmu.write_u32(tls + 4, 1);
        let _ = mmu.write_u32(tls + 8, result);
        0
    }

    /// Reply with a move handle and a success result.
    fn tipc_reply_move_handle(&self, tls: u64, mmu: &mut Memory, handle: u32) -> u64 {
        let _ = mmu.write_u32(tls, 0);
        let _ = mmu.write_u32(tls + 4, 0x8000_0001);
        let _ = mmu.write_u32(tls + 8, 0x20); // one move handle
        let _ = mmu.write_u32(tls + 12, handle);
        let _ = mmu.write_u32(tls + 16, 0);
        0
    }

    pub(super) fn handle_tipc(&mut self, service: &ServiceType, cmd: u32, tls: u64, mmu: &mut Memory) -> u64 {
        // The request's words, read before the reply poisons or overwrites the message area.
        let mut raw = [0u8; 48];
        let _ = mmu.read_bytes(tls + 8, &mut raw);
        let words: Vec<u32> = raw.chunks(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect();
        super::hipc::poison_reply_area(mmu, tls);
        match (service, cmd) {
            // The client's setup sequence: command 4 carries the process handle, a 64-bit id and the process's
            // remote memory usage and peak; 11 and 101 carry the session handle. Each answers with a result only.
            (ServiceType::Tipc(SELF_CONTROLLER_SESSION), 4 | 11 | 101 | 201) => self.tipc_reply_result(tls, mmu, 0),
            // Command 1402 takes the session handle and returns the GPU driver session (`nvdrv`) as a move handle:
            // its first request is Open("/dev/nvhost-ctrl-gpu").
            (ServiceType::Tipc(SELF_CONTROLLER_SESSION), 1402) => {
                let h = self.allocate_handle(ServiceType::Nvdrv);
                self.tipc_reply_move_handle(tls, mmu, h)
            }
            // Command 202 takes the session handle and returns a handle (move) the client keeps.
            (ServiceType::Tipc(SELF_CONTROLLER_SESSION), 202) => {
                let h = self.allocate_handle(ServiceType::Tipc(SELF_CONTROLLER_202));
                self.tipc_reply_move_handle(tls, mmu, h)
            }
            // Command 0 is a handshake: the client sends a 64-bit token and checks that the reply carries the same one.
            (ServiceType::Tipc(_), 0) => {
                let token = u64::from(words[0]) | u64::from(words[1]) << 32;
                let _ = mmu.write_u32(tls, 0);
                let _ = mmu.write_u32(tls + 4, 3);
                let _ = mmu.write_u32(tls + 8, 0);
                let _ = mmu.write_u64(tls + 12, token);
                0
            }
            // Commands several sessions share: 0 handshake, 1 a u32 below 0x400, 3 an enumeration value below 8.
            // Command 1 returns a u32 the client requires to be below 0x400.
            (ServiceType::Tipc(_), 1) => {
                let _ = mmu.write_u32(tls, 0);
                let _ = mmu.write_u32(tls + 4, 2);
                let _ = mmu.write_u32(tls + 8, 0);
                let _ = mmu.write_u32(tls + 12, 1);
                0
            }
            // Command 115 takes a u64 and returns a u64 the client keeps in a global.
            (ServiceType::Tipc(SELF_CONTROLLER_SESSION), 115) => {
                let _ = mmu.write_u32(tls, 0);
                let _ = mmu.write_u32(tls + 4, 3);
                let _ = mmu.write_u32(tls + 8, 0);
                let _ = mmu.write_u64(tls + 12, 0);
                0
            }
            // Command 497 takes a u64 and returns a session (move handle) the client swaps into one of its objects.
            (ServiceType::Tipc(SELF_CONTROLLER_SESSION), 497) => {
                let h = self.allocate_handle(ServiceType::Tipc(SELF_CONTROLLER_497));
                self.tipc_reply_move_handle(tls, mmu, h)
            }
            // Command 11 returns (count, capacity); the client requires count <= min(capacity, 0x40).
            (ServiceType::Tipc(SELF_CONTROLLER_497), 11) => {
                let _ = mmu.write_u32(tls, 0);
                let _ = mmu.write_u32(tls + 4, 3);
                let _ = mmu.write_u32(tls + 8, 0);
                let _ = mmu.write_u32(tls + 12, 0);
                let _ = mmu.write_u32(tls + 16, 0x40);
                0
            }
            // Command 12 takes a slot index below the capacity and returns one event handle (move) per slot. The
            // client waits on slot events; this model completes every operation at once, so they read as signaled.
            (ServiceType::Tipc(SELF_CONTROLLER_497), 12) => {
                let h = self.allocate_handle(ServiceType::ServiceEvent("tipc slot"));
                self.tipc_reply_move_handle(tls, mmu, h)
            }
            // Command 305 returns a session (move handle) that the client stores in one of its objects.
            (ServiceType::Tipc(SELF_CONTROLLER_SESSION), 305) => {
                let h = self.allocate_handle(ServiceType::Tipc(SELF_CONTROLLER_305));
                self.tipc_reply_move_handle(tls, mmu, h)
            }
            // Command 3 returns a u32 the client requires to be below 8 (an enumeration value).
            (ServiceType::Tipc(_), 3) => {
                let _ = mmu.write_u32(tls, 0);
                let _ = mmu.write_u32(tls + 4, 2);
                let _ = mmu.write_u32(tls + 8, 0);
                let _ = mmu.write_u32(tls + 12, 0);
                0
            }
            // On this session command 11 returns an event (move handle) the client polls with a zero timeout.
            (ServiceType::Tipc(SELF_CONTROLLER_305), 11) => {
                let h = self.allocate_handle(ServiceType::ServiceEvent("tipc event"));
                self.tipc_reply_move_handle(tls, mmu, h)
            }
            // Command 5 returns the session limit minus one (the client requires it below 8 and sizes a pool of
            // that many sessions from it; with fewer than 8 slots its second session allocation fails).
            (ServiceType::Tipc(SELF_CONTROLLER_305), 5) => {
                let _ = mmu.write_u32(tls, 0);
                let _ = mmu.write_u32(tls + 4, 2);
                let _ = mmu.write_u32(tls + 8, 0);
                let _ = mmu.write_u32(tls + 12, MAX_SESSIONS - 1);
                0
            }
            // Command 212 takes a name-sized argument block and returns a u64 the client stores.
            (ServiceType::Tipc(SELF_CONTROLLER_305), 212) => {
                let _ = mmu.write_u32(tls, 0);
                let _ = mmu.write_u32(tls + 4, 3);
                let _ = mmu.write_u32(tls + 8, 0);
                let _ = mmu.write_u64(tls + 12, 0);
                0
            }
            // Command 22 returns an event (move handle); the client waits on it like the slot events.
            (ServiceType::Tipc(SELF_CONTROLLER_497), 22) => {
                let h = self.allocate_handle(ServiceType::ServiceEvent("tipc slot"));
                self.tipc_reply_move_handle(tls, mmu, h)
            }
            _ => {
                if self.reported_tipc.insert((format!("{service:?}"), cmd)) {
                    eprintln!("[HIPC] tipc command {cmd} on {service:?} not modelled (replied with success, no output); words after header: {words:08X?}");
                }
                self.tipc_reply_result(tls, mmu, 0)
            }
        }
    }
}

//! Switch 2 ("Ounce" / Horizon OS 20.x–23.x) userland services required by commercial SDK startups:
//! - `acc:u0` / `acc:u1` (`IAccountServiceForApplication`, `IProfile`, `IManagerForApplication`)
//! - `nifm:u` / `nifm:g` / `nifm:s` (`IStaticService`, `IGeneralService`, `IRequest`)
//! - `bsd:u` / `bsd:s` (Berkeley sockets, including Ounce `Cmd 36` `RegisterClient`)

use super::ipc::{IpcManager, ServiceType};
use crate::mmu::Memory;

/// Deterministic 128-bit active Horizon user UUID (`nn::account::Uid`) exposed to games.
pub const DEFAULT_USER_UID: [u8; 16] = [
    0x01, 0x20, 0x26, 0x00, 0x39, 0x02, 0x00, 0x01,
    0x4F, 0x55, 0x4E, 0x43, 0x45, 0x4E, 0x58, 0x32,
];

pub const DEFAULT_ACCOUNT_NICKNAME: &[u8] = b"OuncePlayer";

/// Builds a 0x38-byte `nn::account::ProfileBase` (`uid[16]`, `last_edit_timestamp: u64`, `nickname[0x20]`).
pub fn build_profile_base() -> [u8; 0x38] {
    let mut out = [0u8; 0x38];
    out[0..16].copy_from_slice(&DEFAULT_USER_UID);
    out[16..24].copy_from_slice(&1_735_689_600u64.to_le_bytes());
    out[24..24 + DEFAULT_ACCOUNT_NICKNAME.len()].copy_from_slice(DEFAULT_ACCOUNT_NICKNAME);
    out
}

impl IpcManager {
    /// `acc:u0` / `acc:u1` (`IAccountServiceForApplication`).
    pub(super) fn handle_acc(&mut self, cmd: u32, _args: u64, tls: u64, mmu: &mut Memory) -> u64 {
        match cmd {
            // GetUserCount -> s32 (1)
            0 => self.reply_data(tls, mmu, &1u32.to_le_bytes()),
            // GetUserExistence(Uid) -> bool (true)
            1 => self.reply_data(tls, mmu, &[1u8]),
            // ListAllUsers / ListOpenUsers -> writes array of 16-byte Uids into out buffer
            2 | 3 => {
                let mut list = [0u8; 8 * 16];
                list[0..16].copy_from_slice(&DEFAULT_USER_UID);
                self.write_out_buffer(mmu, &list);
                self.reply_ok(tls, mmu)
            }
            // GetLastOpenedUser -> Uid (16 bytes)
            4 => self.reply_data(tls, mmu, &DEFAULT_USER_UID),
            // GetProfile(Uid) -> IProfile
            5 => self.reply_object(tls, mmu, ServiceType::AccProfile),
            // IsUserRegistrationRequestPermitted -> bool (false)
            50 => self.reply_data(tls, mmu, &[0u8]),
            // TrySelectUserWithoutInteraction -> Uid (16 bytes)
            51 => self.reply_data(tls, mmu, &DEFAULT_USER_UID),
            // OpenUser / ListOpenContextStoredUsers
            60 => {
                let mut list = [0u8; 8 * 16];
                list[0..16].copy_from_slice(&DEFAULT_USER_UID);
                self.write_out_buffer(mmu, &list);
                self.reply_ok(tls, mmu)
            }
            // InitializeApplicationInfoV0 / GetBaasAccountManagerForApplication / InitializeApplicationInfo / V2
            100 | 102 | 140 | 150 | 160 => {
                if cmd == 101 || cmd == 102 {
                    self.reply_object(tls, mmu, ServiceType::AccManager)
                } else {
                    self.reply_ok(tls, mmu)
                }
            }
            // GetBaasAccountManagerForApplication -> IManagerForApplication
            101 => self.reply_object(tls, mmu, ServiceType::AccManager),
            // StoreSaveDataThumbnail / ClearSaveDataThumbnail
            110..=113 | 120 | 130 => self.reply_ok(tls, mmu),
            // ListQualifiedUsers -> writes active Uid into out buffer
            131 => {
                let mut list = [0u8; 8 * 16];
                list[0..16].copy_from_slice(&DEFAULT_USER_UID);
                self.write_out_buffer(mmu, &list);
                self.reply_ok(tls, mmu)
            }
            _ => self.reply_ok(tls, mmu),
        }
    }

    /// `IProfile` from `acc:u0::GetProfile`.
    pub(super) fn handle_acc_profile(&mut self, cmd: u32, tls: u64, mmu: &mut Memory) -> u64 {
        match cmd {
            // Get -> ProfileBase (0x38 bytes inline) + UserData (0x80 bytes out buffer)
            0 => {
                let user_data = [0u8; 0x80];
                self.write_out_buffer(mmu, &user_data);
                self.reply_data(tls, mmu, &build_profile_base())
            }
            // GetBase -> ProfileBase (0x38 bytes inline)
            1 => self.reply_data(tls, mmu, &build_profile_base()),
            // GetImageSize -> u32 (0 bytes; games fall back to built-in avatar icon)
            10 => self.reply_data(tls, mmu, &0u32.to_le_bytes()),
            // LoadImage -> u32 bytes written (0)
            11 => self.reply_data(tls, mmu, &0u32.to_le_bytes()),
            _ => self.reply_ok(tls, mmu),
        }
    }

    /// `IManagerForApplication` from `acc:u0::GetBaasAccountManagerForApplication`.
    pub(super) fn handle_acc_manager(&mut self, cmd: u32, tls: u64, mmu: &mut Memory) -> u64 {
        match cmd {
            // CheckAvailability -> ResultSuccess
            0 => self.reply_ok(tls, mmu),
            // GetAccountId -> u64 network account id
            1 => self.reply_data(tls, mmu, &0x0239_2026_0000_0001u64.to_le_bytes()),
            _ => self.reply_ok(tls, mmu),
        }
    }

    /// `nifm:u` / `nifm:g` / `nifm:s` (`IStaticService`).
    pub(super) fn handle_nifm(&mut self, cmd: u32, tls: u64, mmu: &mut Memory) -> u64 {
        match cmd {
            // CreateGeneralServiceOld (4) / CreateGeneralService (5) -> IGeneralService
            4 | 5 => self.reply_object(tls, mmu, ServiceType::NifmGeneral),
            _ => self.reply_ok(tls, mmu),
        }
    }

    /// `IGeneralService` from `nifm:u`.
    pub(super) fn handle_nifm_general(&mut self, cmd: u32, tls: u64, mmu: &mut Memory) -> u64 {
        match cmd {
            // GetClientId -> writes 8-byte client id to buffer or inline
            1 => {
                self.write_out_buffer(mmu, &1u64.to_le_bytes());
                self.reply_ok(tls, mmu)
            }
            // CreateRequest(s32 requirement_preset) -> IRequest
            4 => self.reply_object(tls, mmu, ServiceType::NifmRequest),
            // GetCurrentIpAddress -> Ipv4Address (127.0.0.1)
            12 => self.reply_data(tls, mmu, &[127, 0, 0, 1]),
            // GetCurrentIpConfigInfo -> IpAddressSetting (0xD bytes) + DnsSetting (0x9 bytes)
            15 => self.reply_data(tls, mmu, &[0u8; 0x18]),
            // GetInternetConnectionStatus -> [type=1 (WiFi), wifiStrength=3, status=4 (Connected)]
            18 => self.reply_data(tls, mmu, &[1u8, 3u8, 4u8, 0u8]),
            // IsAnyInternetRequestAccepted -> bool (true)
            21 => self.reply_data(tls, mmu, &[1u8]),
            _ => self.reply_ok(tls, mmu),
        }
    }

    /// `IRequest` from `nifm::IGeneralService::CreateRequest`.
    pub(super) fn handle_nifm_request(&mut self, cmd: u32, tls: u64, mmu: &mut Memory) -> u64 {
        match cmd {
            // GetRequestState -> 3 (Accepted / Active local interface so games do not block on network dialogs)
            0 => self.reply_data(tls, mmu, &3u32.to_le_bytes()),
            // GetResult -> ResultSuccess (0)
            1 => self.reply_ok(tls, mmu),
            // GetSystemEventReadableHandles -> 2 copy handles
            2 => {
                let ev0 = self.allocate_handle(ServiceType::ServiceEvent("NifmReq0"));
                let ev1 = self.allocate_handle(ServiceType::ServiceEvent("NifmReq1"));
                self.reply(tls, mmu, 0, &[], &[ev0, ev1], None);
                0
            }
            // Cancel / Submit / SetRequirementPreset / SetConnectionConfirmationOption
            3 | 4 | 5 | 6 | 11 | 12 => self.reply_ok(tls, mmu),
            _ => self.reply_ok(tls, mmu),
        }
    }

    /// `bsd:u` / `bsd:s` Berkeley Socket Driver (including Switch 2 Ounce `Cmd 36` `RegisterClient`).
    pub(super) fn handle_bsd(&mut self, cmd: u32, tls: u64, mmu: &mut Memory) -> u64 {
        match cmd {
            // Switch 1 RegisterClient (0) & Switch 2 Ounce Horizon 20.x+ RegisterClient (36) -> u64 (error = 0)
            0 | 36 => self.reply_data(tls, mmu, &0u64.to_le_bytes()),
            // StartMonitoring -> 0
            1 => self.reply_ok(tls, mmu),
            // Socket(domain, type, protocol) -> (s32 fd = 3, s32 bsd_errno = 0)
            2 => {
                let mut out = [0u8; 8];
                out[0..4].copy_from_slice(&3i32.to_le_bytes());
                out[4..8].copy_from_slice(&0i32.to_le_bytes());
                self.reply_data(tls, mmu, &out)
            }
            // Poll / Select -> (ret = 0, bsd_errno = 0)
            6 | 7 => self.reply_data(tls, mmu, &[0u8; 8]),
            // Connect / Bind / Listen -> (ret = -1, bsd_errno = ENETUNREACH 51) for clean offline fallback
            13 | 14 | 15 | 18 => {
                let mut out = [0u8; 8];
                out[0..4].copy_from_slice(&(-1i32).to_le_bytes());
                out[4..8].copy_from_slice(&51i32.to_le_bytes());
                self.reply_data(tls, mmu, &out)
            }
            // SetSockOpt / Shutdown / Close -> (ret = 0, bsd_errno = 0)
            21 | 22 | 26 => self.reply_data(tls, mmu, &[0u8; 8]),
            _ => self.reply_data(tls, mmu, &[0u8; 8]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::cmif::test_util::{Harness, Req, BUF};

    #[test]
    fn ounce_acc_nifm_and_bsd_cmd36_services_respond_cleanly() {
        let mut h = Harness::new();

        // 1. acc:u0 user & profile handshake
        let acc = h.get_service("acc:u0");
        assert_eq!(h.call_out(acc, Req { cmd: 0, ..Default::default() }, 4).u32(0), 1);
        let uid_reply = h.call_out(acc, Req { cmd: 4, ..Default::default() }, 16);
        assert_eq!(&uid_reply.data[..16], &DEFAULT_USER_UID);
        let prof = h.call(acc, Req { cmd: 5, data: &DEFAULT_USER_UID, ..Default::default() }).object();
        let base = h.call_out(prof, Req { cmd: 1, ..Default::default() }, 0x38);
        assert_eq!(&base.data[0..16], &DEFAULT_USER_UID);
        assert_eq!(&base.data[24..24 + DEFAULT_ACCOUNT_NICKNAME.len()], DEFAULT_ACCOUNT_NICKNAME);
        assert_eq!(h.call(acc, Req { cmd: 2, recv: Some((BUF, 0x80)), ..Default::default() }).result, 0);
        assert_eq!(&h.read_buf(BUF, 16), &DEFAULT_USER_UID);

        // 2. nifm:u -> IGeneralService -> IRequest
        let nifm = h.get_service("nifm:u");
        let general = h.call(nifm, Req { cmd: 5, ..Default::default() }).object();
        let status = h.call_out(general, Req { cmd: 18, ..Default::default() }, 4);
        assert_eq!(&status.data[..3], &[1, 3, 4]);
        let req = h.call(general, Req { cmd: 4, data: &1u32.to_le_bytes(), ..Default::default() }).object();
        assert_eq!(h.call_out(req, Req { cmd: 0, ..Default::default() }, 4).u32(0), 3);

        // 3. bsd:u Switch 2 Ounce Cmd 36 RegisterClient + Socket
        let bsd = h.get_service("bsd:u");
        assert_eq!(h.call_out(bsd, Req { cmd: 36, ..Default::default() }, 8).u64(0), 0);
        let sock = h.call_out(bsd, Req { cmd: 2, data: &[2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0], ..Default::default() }, 8);
        assert_eq!(sock.u32(0), 3);
        assert_eq!(sock.u32(4), 0);
    }
}

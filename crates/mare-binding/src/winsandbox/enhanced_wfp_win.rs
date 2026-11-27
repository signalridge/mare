//! Windows Filtering Platform side of the enhanced tier: the persistent provider, sublayer and
//! filters that block the offline account's outbound connects (permitting loopback to the proxy
//! port range). Split out of `enhanced_win.rs` (module size rule); a child module of it.

use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FWP_ACTION_BLOCK, FWP_ACTION_PERMIT, FWP_ACTRL_MATCH_FILTER, FWP_BYTE_ARRAY16, FWP_BYTE_ARRAY16_TYPE,
    FWP_BYTE_BLOB, FWP_CONDITION_VALUE0, FWP_CONDITION_VALUE0_0, FWP_EMPTY, FWP_MATCH_EQUAL, FWP_MATCH_RANGE,
    FWP_RANGE_TYPE, FWP_RANGE0, FWP_SECURITY_DESCRIPTOR_TYPE, FWP_UINT8, FWP_UINT16, FWP_UINT32, FWP_VALUE0,
    FWP_VALUE0_0, FWPM_ACTION0, FWPM_ACTION0_0, FWPM_CONDITION_ALE_USER_ID, FWPM_CONDITION_IP_PROTOCOL,
    FWPM_CONDITION_IP_REMOTE_ADDRESS, FWPM_CONDITION_IP_REMOTE_PORT, FWPM_DISPLAY_DATA0, FWPM_FILTER_CONDITION0,
    FWPM_FILTER_FLAG_PERSISTENT, FWPM_FILTER0, FWPM_FILTER0_0, FWPM_LAYER_ALE_AUTH_CONNECT_V4,
    FWPM_LAYER_ALE_AUTH_CONNECT_V6, FWPM_LAYER_ALE_AUTH_LISTEN_V4, FWPM_LAYER_ALE_AUTH_LISTEN_V6,
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4, FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6, FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
    FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6, FWPM_PROVIDER_FLAG_PERSISTENT, FWPM_PROVIDER0, FWPM_SESSION0,
    FWPM_SUBLAYER_FLAG_PERSISTENT, FWPM_SUBLAYER0, FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0,
    FwpmFilterDeleteByKey0, FwpmProviderAdd0, FwpmProviderDeleteByKey0, FwpmSubLayerAdd0, FwpmSubLayerDeleteByKey0,
    FwpmTransactionAbort0, FwpmTransactionBegin0, FwpmTransactionCommit0,
};
use windows_sys::Win32::Networking::WinSock::{IPPROTO_ICMP, IPPROTO_ICMPV6};
use windows_sys::Win32::Security::Authorization::{
    BuildExplicitAccessWithNameW, BuildSecurityDescriptorW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
};
use windows_sys::Win32::System::Rpc::RPC_C_AUTHN_DEFAULT;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::core::GUID;

use super::{PROXY_PORT_RANGE, wide};

const FWP_E_ALREADY_EXISTS: u32 = 0x8032_0009;
const FWP_E_NOT_FOUND: u32 = 0x8032_0008;
const FWP_E_FILTER_NOT_FOUND: u32 = 0x8032_0003;
const FWP_E_PROVIDER_NOT_FOUND: u32 = 0x8032_0005;
const FWP_E_SUBLAYER_NOT_FOUND: u32 = 0x8032_0007;
const FWP_E_IN_USE: u32 = 0x8032_000A;

// mare-owned persistent WFP identities (never regenerate: old objects would be orphaned)
const PROVIDER_KEY: GUID = GUID::from_u128(0x6d1b_7c4e_2a3f_4e8b_9c51_0a7e_3d2f_6b11);
const SUBLAYER_KEY: GUID = GUID::from_u128(0x4f2e_8a90_5b17_4c6d_8e3a_1f9b_7c05_d222);
const FILTERS: [(GUID, &str); 10] = [
    (GUID::from_u128(0x0b7e_44d1_6f2a_4b3c_9a8e_5d1c_2e7f_a301), "mare-sbx block connect v4"),
    (GUID::from_u128(0x1c8f_55e2_7a3b_4c4d_8b9f_6e2d_3f80_b402), "mare-sbx block connect v6"),
    (GUID::from_u128(0x2d90_66f3_8b4c_4d5e_9ca0_7f3e_4091_c503), "mare-sbx permit loopback proxy v4"),
    (GUID::from_u128(0x3ea1_7704_9c5d_4e6f_8db1_8040_51a2_d604), "mare-sbx permit loopback proxy v6"),
    (GUID::from_u128(0x4fb2_8815_ad6e_4f70_9ec2_9151_62b3_e705), "mare-sbx block icmp assign v4"),
    (GUID::from_u128(0x50c3_9926_be7f_4081_afb3_a262_73c4_f806), "mare-sbx block icmp assign v6"),
    (GUID::from_u128(0x61d4_aa37_cf80_4192_b0c4_b373_84d5_0907), "mare-sbx block listen v4"),
    (GUID::from_u128(0x72e5_bb48_d091_42a3_c1d5_c484_95e6_1a08), "mare-sbx block listen v6"),
    (GUID::from_u128(0x83f6_cc59_e1a2_43b4_d2e6_d595_a6f7_2b09), "mare-sbx block recv accept v4"),
    (GUID::from_u128(0x9407_dd6a_f2b3_44c5_e3f7_e6a6_b708_3c0a), "mare-sbx block recv accept v6"),
];

struct Engine(HANDLE);
impl Engine {
    fn open() -> Result<Self, String> {
        let name = wide("mare sandbox WFP");
        let mut session: FWPM_SESSION0 = unsafe { std::mem::zeroed() };
        session.displayData = FWPM_DISPLAY_DATA0 { name: name.as_ptr() as *mut u16, description: null_mut() };
        session.txnWaitTimeoutInMSec = INFINITE;
        let mut h: HANDLE = null_mut();
        check(
            unsafe { FwpmEngineOpen0(null(), RPC_C_AUTHN_DEFAULT as u32, null(), &session, &mut h) },
            "FwpmEngineOpen0",
            &[],
        )?;
        Ok(Self(h))
    }
}
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe { FwpmEngineClose0(self.0) };
    }
}

fn check(rc: u32, what: &str, ok: &[u32]) -> Result<(), String> {
    if rc == 0 || ok.contains(&rc) { Ok(()) } else { Err(format!("{what} failed: 0x{rc:08X}")) }
}

fn empty_blob() -> FWP_BYTE_BLOB {
    FWP_BYTE_BLOB { size: 0, data: null_mut() }
}

fn weight(w: u8) -> FWP_VALUE0 {
    FWP_VALUE0 { r#type: FWP_UINT8, Anonymous: FWP_VALUE0_0 { uint8: w } }
}

/// Install the offline account's filters: block every outbound connect (v4 + v6) for the account,
/// permit loopback to [`PROXY_PORT_RANGE`] with a higher weight. Returns the filter count.
pub(super) fn install_wfp(account: &str) -> Result<u64, String> {
    let engine = Engine::open()?;
    check(unsafe { FwpmTransactionBegin0(engine.0, 0) }, "FwpmTransactionBegin0", &[])?;
    let r = (|| -> Result<u64, String> {
        let pname = wide("mare sandbox");
        let provider = FWPM_PROVIDER0 {
            providerKey: PROVIDER_KEY,
            displayData: FWPM_DISPLAY_DATA0 { name: pname.as_ptr() as *mut u16, description: null_mut() },
            flags: FWPM_PROVIDER_FLAG_PERSISTENT,
            providerData: empty_blob(),
            serviceName: null_mut(),
        };
        check(
            unsafe { FwpmProviderAdd0(engine.0, &provider, null_mut()) },
            "FwpmProviderAdd0",
            &[FWP_E_ALREADY_EXISTS],
        )?;
        let provider_key = PROVIDER_KEY;
        let sublayer = FWPM_SUBLAYER0 {
            subLayerKey: SUBLAYER_KEY,
            displayData: FWPM_DISPLAY_DATA0 { name: pname.as_ptr() as *mut u16, description: null_mut() },
            flags: FWPM_SUBLAYER_FLAG_PERSISTENT,
            providerKey: &provider_key as *const _ as *mut _,
            providerData: empty_blob(),
            weight: 0x8000,
        };
        check(
            unsafe { FwpmSubLayerAdd0(engine.0, &sublayer, null_mut()) },
            "FwpmSubLayerAdd0",
            &[FWP_E_ALREADY_EXISTS],
        )?;

        // ALE_USER_ID condition: a security descriptor granting the account FWP_ACTRL_MATCH_FILTER
        let acct = wide(account);
        let mut ea: EXPLICIT_ACCESS_W = unsafe { std::mem::zeroed() };
        unsafe { BuildExplicitAccessWithNameW(&mut ea, acct.as_ptr(), FWP_ACTRL_MATCH_FILTER, GRANT_ACCESS, 0) };
        let mut sd: *mut c_void = null_mut();
        let mut sd_len = 0u32;
        check(
            unsafe { BuildSecurityDescriptorW(null(), null(), 1, &ea, 0, null(), null_mut(), &mut sd_len, &mut sd) },
            "BuildSecurityDescriptorW",
            &[],
        )?;
        let user_blob = FWP_BYTE_BLOB { size: sd_len, data: sd as *mut u8 };
        let user = FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_ALE_USER_ID,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 { sd: &user_blob as *const _ as *mut _ },
            },
        };
        let mut range = FWP_RANGE0 {
            valueLow: FWP_VALUE0 { r#type: FWP_UINT16, Anonymous: FWP_VALUE0_0 { uint16: PROXY_PORT_RANGE.0 } },
            valueHigh: FWP_VALUE0 { r#type: FWP_UINT16, Anonymous: FWP_VALUE0_0 { uint16: PROXY_PORT_RANGE.1 } },
        };
        let ports = FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_REMOTE_PORT,
            matchType: FWP_MATCH_RANGE,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_RANGE_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 { rangeValue: &mut range },
            },
        };
        let lo4 = FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT32,
                Anonymous: FWP_CONDITION_VALUE0_0 { uint32: 0x7F00_0001 },
            },
        };
        let mut v6 = FWP_BYTE_ARRAY16 { byteArray16: [0; 16] };
        v6.byteArray16[15] = 1;
        let lo6 = FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_REMOTE_ADDRESS,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_BYTE_ARRAY16_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 { byteArray16: &mut v6 },
            },
        };
        let icmp4 = FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_PROTOCOL,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT8,
                Anonymous: FWP_CONDITION_VALUE0_0 { uint8: IPPROTO_ICMP as u8 },
            },
        };
        let icmp6 = FWPM_FILTER_CONDITION0 {
            fieldKey: FWPM_CONDITION_IP_PROTOCOL,
            matchType: FWP_MATCH_EQUAL,
            conditionValue: FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT8,
                Anonymous: FWP_CONDITION_VALUE0_0 { uint8: IPPROTO_ICMPV6 as u8 },
            },
        };
        let specs: [(usize, GUID, u32, u8, Vec<FWPM_FILTER_CONDITION0>); 10] = [
            (0, FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWP_ACTION_BLOCK, 1, vec![user]),
            (1, FWPM_LAYER_ALE_AUTH_CONNECT_V6, FWP_ACTION_BLOCK, 1, vec![user]),
            (2, FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWP_ACTION_PERMIT, 15, vec![user, lo4, ports]),
            (3, FWPM_LAYER_ALE_AUTH_CONNECT_V6, FWP_ACTION_PERMIT, 15, vec![user, lo6, ports]),
            (4, FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4, FWP_ACTION_BLOCK, 1, vec![user, icmp4]),
            (5, FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6, FWP_ACTION_BLOCK, 1, vec![user, icmp6]),
            (6, FWPM_LAYER_ALE_AUTH_LISTEN_V4, FWP_ACTION_BLOCK, 1, vec![user]),
            (7, FWPM_LAYER_ALE_AUTH_LISTEN_V6, FWP_ACTION_BLOCK, 1, vec![user]),
            (8, FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4, FWP_ACTION_BLOCK, 1, vec![user]),
            (9, FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6, FWP_ACTION_BLOCK, 1, vec![user]),
        ];
        let mut n = 0;
        for (i, layer, action, w, mut conds) in specs {
            let (key, name) = FILTERS[i];
            let wname = wide(name);
            check(
                unsafe { FwpmFilterDeleteByKey0(engine.0, &key) },
                "FwpmFilterDeleteByKey0",
                &[FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND],
            )?;
            let filter = FWPM_FILTER0 {
                filterKey: key,
                displayData: FWPM_DISPLAY_DATA0 { name: wname.as_ptr() as *mut u16, description: null_mut() },
                flags: FWPM_FILTER_FLAG_PERSISTENT,
                providerKey: &provider_key as *const _ as *mut _,
                providerData: empty_blob(),
                layerKey: layer,
                subLayerKey: SUBLAYER_KEY,
                weight: weight(w),
                numFilterConditions: conds.len() as u32,
                filterCondition: conds.as_mut_ptr(),
                action: FWPM_ACTION0 { r#type: action, Anonymous: FWPM_ACTION0_0 { filterType: GUID::from_u128(0) } },
                Anonymous: FWPM_FILTER0_0 { rawContext: 0 },
                reserved: null_mut(),
                filterId: 0,
                effectiveWeight: FWP_VALUE0 { r#type: FWP_EMPTY, Anonymous: unsafe { std::mem::zeroed() } },
            };
            let mut id = 0u64;
            let rc = unsafe { FwpmFilterAdd0(engine.0, &filter, null_mut(), &mut id) };
            if let Err(e) = check(rc, &format!("FwpmFilterAdd0({name})"), &[]) {
                unsafe { LocalFree(sd as HLOCAL) };
                return Err(e);
            }
            n += 1;
        }
        unsafe { LocalFree(sd as HLOCAL) };
        Ok(n)
    })();
    match r {
        Ok(n) => {
            check(unsafe { FwpmTransactionCommit0(engine.0) }, "FwpmTransactionCommit0", &[])?;
            Ok(n)
        }
        Err(e) => {
            unsafe { FwpmTransactionAbort0(engine.0) };
            Err(e)
        }
    }
}

pub(super) fn remove_wfp() -> Result<(), String> {
    let engine = Engine::open()?;
    for (key, _) in FILTERS {
        check(
            unsafe { FwpmFilterDeleteByKey0(engine.0, &key) },
            "FwpmFilterDeleteByKey0",
            &[FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND],
        )?;
    }
    check(
        unsafe { FwpmSubLayerDeleteByKey0(engine.0, &SUBLAYER_KEY) },
        "FwpmSubLayerDeleteByKey0",
        &[FWP_E_SUBLAYER_NOT_FOUND, FWP_E_NOT_FOUND, FWP_E_IN_USE],
    )?;
    check(
        unsafe { FwpmProviderDeleteByKey0(engine.0, &PROVIDER_KEY) },
        "FwpmProviderDeleteByKey0",
        &[FWP_E_PROVIDER_NOT_FOUND, FWP_E_NOT_FOUND, FWP_E_IN_USE],
    )?;
    Ok(())
}

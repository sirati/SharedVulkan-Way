//! Reading and setting the default audio endpoints, per role. FOR TESTS
//! (`wc-hostint-probe`): host integration itself follows the host's
//! defaults and never sets one.
//!
//! Reading is `IMMDeviceEnumerator::GetDefaultAudioEndpoint`. Setting has
//! no documented API: Windows' own Sound control panel uses the
//! undocumented `IPolicyConfig` of `CPolicyConfigClient`, which every tool
//! that switches defaults uses too, and which has been stable since
//! Windows 7. Its vtable is declared here like the rest of `win::com`.
//! Vista's variant (`IPolicyConfigVista`, without `ResetDeviceFormat`, so
//! `SetDefaultEndpoint` is one slot earlier) is the fallback.

use std::ffi::c_void;

use windows_sys::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
use windows_sys::core::{GUID, HRESULT};

use super::audio::{device_id, enumerator};
use super::com::*;
use crate::defaults::{Default, FLOWS, Flow, ROLES, Role};

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
const CLSID_POLICY_CONFIG_VISTA_CLIENT: GUID = GUID::from_u128(0x294935ce_f637_4e7c_a41b_ab255460b862);
const IID_POLICY_CONFIG_VISTA: GUID = GUID::from_u128(0x568b9108_44bf_40b4_9006_86afe5b5a620);

type SetDefault = unsafe extern "system" fn(*mut c_void, *const u16, i32) -> HRESULT;

/// `IPolicyConfig` (Windows 7 and later).
#[repr(C)]
struct PolicyConfigVtbl {
    base: UnknownVtbl,
    get_mix_format: Fp,
    get_device_format: Fp,
    reset_device_format: Fp,
    set_device_format: Fp,
    get_processing_period: Fp,
    set_processing_period: Fp,
    get_share_mode: Fp,
    set_share_mode: Fp,
    get_property_value: Fp,
    set_property_value: Fp,
    set_default_endpoint: SetDefault,
    set_endpoint_visibility: Fp,
}

/// `IPolicyConfigVista`: no `ResetDeviceFormat`.
#[repr(C)]
struct PolicyConfigVistaVtbl {
    base: UnknownVtbl,
    get_mix_format: Fp,
    get_device_format: Fp,
    set_device_format: Fp,
    get_processing_period: Fp,
    set_processing_period: Fp,
    get_share_mode: Fp,
    set_share_mode: Fp,
    get_property_value: Fp,
    set_property_value: Fp,
    set_default_endpoint: SetDefault,
    set_endpoint_visibility: Fp,
}

const _: () = assert!(std::mem::offset_of!(PolicyConfigVtbl, set_default_endpoint) == 13 * 8);
const _: () = assert!(std::mem::offset_of!(PolicyConfigVistaVtbl, set_default_endpoint) == 12 * 8);

/// Every direction's default for every role now.
pub fn get_defaults() -> Result<Vec<Default>, String> {
    let _com = Apartment::mta();
    let e = enumerator()?;
    let mut all = Vec::new();
    for flow in FLOWS {
        for role in ROLES {
            // SAFETY: a live enumerator. No device of that flow is `None`.
            let d = unsafe {
                out::<DeviceVtbl>("GetDefaultAudioEndpoint", |p| {
                    (e.v().get_default_audio_endpoint)(e.as_raw(), flow.value(), role.value(), p)
                })
            };
            let id = d.ok().and_then(|d| device_id(&d).ok());
            all.push(Default { flow, role, id });
        }
    }
    Ok(all)
}

/// Make endpoint `id` the default for `role`. `flow` is checked against the
/// endpoint (an input cannot be the default output). Returns which
/// interface did it.
pub fn set_default(flow: Flow, role: Role, id: &str) -> Result<&'static str, String> {
    let _com = Apartment::mta();
    // The endpoint must exist, be active, and be of the direction asked for.
    let e = enumerator()?;
    let d = super::audio::device(&e, id)?;
    let want = super::audio::flow_of(&d)?;
    if want != flow.value() {
        return Err(format!("{id} is not a {} endpoint", flow.word()));
    }
    let w = wide(id);
    // SAFETY: CoCreateInstance into an out pointer of the IID's vtable.
    let new = unsafe {
        out::<PolicyConfigVtbl>("CoCreateInstance(PolicyConfigClient, IPolicyConfig)", |p| {
            CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, std::ptr::null_mut(), CLSCTX_ALL, &IID_POLICY_CONFIG, p)
        })
    };
    let (hr, via) = match new {
        // SAFETY: a live IPolicyConfig and a NUL-terminated id.
        Ok(pc) => (unsafe { (pc.v().set_default_endpoint)(pc.as_raw(), w.as_ptr(), role.value()) }, "IPolicyConfig"),
        Err(e1) => {
            // SAFETY: as above, Vista's interface.
            let pc = unsafe {
                out::<PolicyConfigVistaVtbl>("CoCreateInstance(PolicyConfigVistaClient, IPolicyConfigVista)", |p| {
                    CoCreateInstance(&CLSID_POLICY_CONFIG_VISTA_CLIENT, std::ptr::null_mut(), CLSCTX_ALL, &IID_POLICY_CONFIG_VISTA, p)
                })
            }
            .map_err(|e2| format!("{e1}; {e2}"))?;
            // SAFETY: a live IPolicyConfigVista and a NUL-terminated id.
            (unsafe { (pc.v().set_default_endpoint)(pc.as_raw(), w.as_ptr(), role.value()) }, "IPolicyConfigVista")
        }
    };
    if hr < 0 {
        return Err(format!("{via}::SetDefaultEndpoint({}, {}): {}", role.word(), id, hr_text(hr)));
    }
    Ok(via)
}

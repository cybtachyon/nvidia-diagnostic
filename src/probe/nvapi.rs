//! NvAPI client (`libnvidia-api.so.1`) for the GPU hotspot (junction)
//! temperature.
//!
//! `nvidia-smi` only exposes the core (`temperature.gpu`) and memory
//! temperatures; the hotspot is reachable only through NvAPI. This mirrors
//! LACT (`ilya-zlobintsev/LACT`, `lact-daemon/.../nvidia/nvapi.rs`): on
//! Blackwell the aggregated junction sensor is read from the BAR0 register
//! at `0xad0aa0` (8.8 fixed point, `(raw & 0xFFFF) / 256` °C); on other
//! architectures the primary hotspot therm channel is used instead.
//!
//! The library is loaded with `dlopen` and the undocumented
//! `nvapi_QueryInterface` entry point is resolved with `dlsym`, so no C
//! dependency is required. Every read degrades to `None` (missing library,
//! no matching GPU, or a failed register op, e.g. when not running as root).

use libc::{c_char, c_void};
use std::mem::{self, transmute};

/// `libnvidia-api.so.1` ships with the NVIDIA driver.
const LIBRARY_NAME: &[u8] = b"libnvidia-api.so.1\0";
const QUERY_INTERFACE: &[u8] = b"nvapi_QueryInterface\0";

// Undocumented NvAPI interface ids (from LACT; stable across drivers).
const QUERY_INITIALIZE: u32 = 0x0150e828;
const QUERY_UNLOAD: u32 = 0xd22bdd7e;
const QUERY_ENUM_GPUS: u32 = 0xe5ac912f;
const QUERY_GET_BUS_ID: u32 = 0x1be0b8e5;
const QUERY_THERM_INFO: u32 = 0x0bc8163d;
const QUERY_THERM_STATUS: u32 = 0x65fe3aad;
const QUERY_REGISTER_OP: u32 = 0x2eb3c140;

/// Blackwell aggregated junction-sensor register (GB202/GB203/GB205).
const REG_BLACKWELL_HOTSPOT: u32 = 0xad0aa0;
const REG_OP_FLAG_READ: u16 = 1;
const REG_OP_FLAG_32BIT: u16 = 4;
const REG_OP_FLAG_TYPE_GLOBAL: u16 = 16;

/// NvAPI therm channel type index of the primary hotspot channel.
const THERM_CHANNEL_TYPE_HOTSPOT: usize = 1;
const MAX_GPUS: usize = 64;

type NvStatus = i32;
type GpuHandle = *mut c_void;

type QueryInterfaceFn = unsafe extern "C" fn(id: u32) -> *const c_void;
type InitializeFn = unsafe extern "C" fn() -> NvStatus;
type UnloadFn = unsafe extern "C" fn() -> NvStatus;
type EnumGpusFn = unsafe extern "C" fn(handles: *mut GpuHandle, count: *mut u32) -> NvStatus;
type GetBusIdFn = unsafe extern "C" fn(handle: GpuHandle, id: *mut u32) -> NvStatus;
type ThermInfoFn = unsafe extern "C" fn(handle: GpuHandle, info: *mut ThermChannelInfo) -> NvStatus;
type ThermStatusFn = unsafe extern "C" fn(handle: GpuHandle, thermals: *mut Thermals) -> NvStatus;
type RegisterOpFn = unsafe extern "C" fn(handle: GpuHandle, data: *mut RegisterOpData) -> NvStatus;

/// `(size_of::<T>() | (version << 16))`, the NvAPI `version` encoding.
const fn make_version<T>(version: usize) -> u32 {
    (mem::size_of::<T>() | (version << 16)) as u32
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RegisterOp {
    flags: u16,
    status: u16,
    offset: u32,
    write_mask: u64,
    value: u64,
}

#[repr(C)]
struct RegisterOpData {
    version: u32,
    op_count: u32,
    op: [RegisterOp; 256],
}

#[repr(C)]
struct Thermals {
    version: u32,
    mask: i32,
    rsvd: [u8; 32],
    temps: [i32; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ThermChannel {
    class: u32,
    channel_type: u32,
    rel_loc: u32,
    target_gpu: u32,
    scaling: i32,
    offset_sw: i32,
    min_temp: i32,
    max_temp: i32,
    is_temp_sim_supported: u8,
    flags: u8,
    offset_hw: i32,
    rsvd: [u8; 28],
    data: [u8; 16],
}

#[repr(C)]
struct ThermChannelInfo {
    version: u32,
    mask: i32,
    rsvd: [u8; 32],
    channels: [ThermChannel; 32],
    primary_channel_idx: [u8; 5],
}

/// A loaded NvAPI client. Create once with `new()` and reuse; every read is
/// best-effort and returns `None` on any failure.
pub struct NvApi {
    /// Kept alive for the lifetime of the client (the function pointers are
    /// symbols of this library; not read, so the field is `allow(dead_code)`).
    #[allow(dead_code)]
    lib: *mut c_void,
    qi: QueryInterfaceFn,
    gpus: Vec<GpuHandle>,
}

impl NvApi {
    /// Load `libnvidia-api.so.1`, initialize NvAPI, and enumerate the
    /// physical GPUs. Returns `None` when the library is absent or
    /// initialization fails.
    pub fn new() -> Option<Self> {
        unsafe {
            let lib = libc::dlopen(LIBRARY_NAME.as_ptr() as *const c_char, libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            let sym = libc::dlsym(lib, QUERY_INTERFACE.as_ptr() as *const c_char);
            if sym.is_null() {
                return None;
            }
            let qi = transmute::<*mut c_void, QueryInterfaceFn>(sym);

            let init = qi(QUERY_INITIALIZE);
            if init.is_null() {
                return None;
            }
            if (transmute::<*const c_void, InitializeFn>(init))() != 0 {
                return None;
            }

            let enum_gpus_ptr = qi(QUERY_ENUM_GPUS);
            if enum_gpus_ptr.is_null() {
                return None;
            }
            let enum_gpus = transmute::<*const c_void, EnumGpusFn>(enum_gpus_ptr);
            let mut handles: [GpuHandle; MAX_GPUS] = [std::ptr::null_mut(); MAX_GPUS];
            let mut count: u32 = 0;
            if enum_gpus(handles.as_mut_ptr(), &mut count) != 0 {
                return None;
            }
            let gpus = handles[..count as usize].to_vec();
            Some(NvApi { lib, qi, gpus })
        }
    }

    /// The GPU hotspot (junction) temperature in °C for the card identified
    /// by `bdf_full` (e.g. `00000000:01:00.0`). `gpu_name` selects the
    /// Blackwell register path when it names an RTX 50-series card.
    pub fn read_hotspot(&self, bdf_full: &str, gpu_name: &str) -> Option<u32> {
        let bus = bdf_bus_number(bdf_full)?;
        let handle = self.find_gpu(bus)?;
        if is_blackwell(gpu_name) {
            self.read_blackwell_hotspot(handle)
        } else {
            self.read_therm_channel_hotspot(handle)
        }
    }

    fn find_gpu(&self, bus: u32) -> Option<GpuHandle> {
        let get_bus_id =
            unsafe { transmute::<*const c_void, GetBusIdFn>(self.query(QUERY_GET_BUS_ID)?) };
        for &handle in &self.gpus {
            let mut id: u32 = 0;
            if unsafe { get_bus_id(handle, &mut id) } == 0 && id == bus {
                return Some(handle);
            }
        }
        None
    }

    /// Blackwell: read the aggregated junction register and decode the 8.8
    /// fixed-point value.
    fn read_blackwell_hotspot(&self, handle: GpuHandle) -> Option<u32> {
        let register_op =
            unsafe { transmute::<*const c_void, RegisterOpFn>(self.query(QUERY_REGISTER_OP)?) };
        let mut data = RegisterOpData {
            version: make_version::<RegisterOpData>(1),
            op_count: 1,
            op: [RegisterOp {
                flags: REG_OP_FLAG_READ | REG_OP_FLAG_32BIT | REG_OP_FLAG_TYPE_GLOBAL,
                status: 0,
                offset: REG_BLACKWELL_HOTSPOT,
                write_mask: 0,
                value: 0,
            }; 256],
        };
        if unsafe { register_op(handle, &mut data) } != 0 {
            return None;
        }
        decode_temp_register(data.op[0].value)
    }

    /// Pre-Blackwell: the primary hotspot therm channel.
    fn read_therm_channel_hotspot(&self, handle: GpuHandle) -> Option<u32> {
        let therm_info =
            unsafe { transmute::<*const c_void, ThermInfoFn>(self.query(QUERY_THERM_INFO)?) };
        let mut info = ThermChannelInfo {
            version: make_version::<ThermChannelInfo>(2),
            mask: 0,
            rsvd: [0; 32],
            channels: [ThermChannel::default(); 32],
            primary_channel_idx: [u8::MAX; 5],
        };
        if unsafe { therm_info(handle, &mut info) } != 0 {
            return None;
        }
        let channel = info
            .primary_channel_idx
            .get(THERM_CHANNEL_TYPE_HOTSPOT)
            .copied()
            .map(|c| c as usize)
            .filter(|&c| c < 32 && info.mask & (1 << c) != 0)?;

        let therm_status =
            unsafe { transmute::<*const c_void, ThermStatusFn>(self.query(QUERY_THERM_STATUS)?) };
        let mut thermals = Thermals {
            version: make_version::<Thermals>(2),
            mask: info.mask,
            rsvd: [0; 32],
            temps: [0; 32],
        };
        if unsafe { therm_status(handle, &mut thermals) } != 0 {
            return None;
        }
        let value = *thermals.temps.get(channel)? / 256;
        (value > 0 && value < 255).then_some(value as u32)
    }

    /// Resolve one NvAPI interface to a function-pointer-sized raw value.
    /// Callers transmute it to the concrete `*const c_void` → fn-pointer
    /// type (a generic transmute is not allowed, so the cast happens at the
    /// concrete call site).
    fn query(&self, id: u32) -> Option<*const c_void> {
        let ptr = unsafe { (self.qi)(id) };
        (!ptr.is_null()).then_some(ptr)
    }
}

impl Drop for NvApi {
    fn drop(&mut self) {
        // Best-effort unload; the dlopen handle is released by the process.
        if let Some(ptr) = self.query(QUERY_UNLOAD) {
            let unload = unsafe { transmute::<*const c_void, UnloadFn>(ptr) };
            let _ = unsafe { unload() };
        }
    }
}

/// The PCI bus number (hex) of a full BDF like `00000000:01:00.0`. NvAPI's
/// `GetBusId` reports the bus number only, so that is the match key.
fn bdf_bus_number(bdf_full: &str) -> Option<u32> {
    let (_, rest) = bdf_full.split_once(':')?;
    let (bus, _) = rest.split_once(':')?;
    u32::from_str_radix(bus, 16).ok()
}

/// The RTX 50 series (GB202/GB203/GB205) is Blackwell and uses the
/// register-based junction sensor.
fn is_blackwell(name: &str) -> bool {
    name.to_ascii_lowercase().contains("rtx 50")
}

/// Decode a Blackwell temperature register (8.8 fixed point, low 16 bits).
fn decode_temp_register(raw: u64) -> Option<u32> {
    let value = (raw & 0xFFFF) / 256;
    (value > 0 && value < 255).then_some(value as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bdf_bus_number_parses_full_form() {
        assert_eq!(bdf_bus_number("00000000:01:00.0"), Some(1));
        assert_eq!(bdf_bus_number("00000000:2b:00.0"), Some(0x2b));
        assert_eq!(bdf_bus_number("garbage"), None);
    }

    #[test]
    fn blackwell_detection_is_by_name() {
        assert!(is_blackwell("NVIDIA GeForce RTX 5090"));
        assert!(is_blackwell("NVIDIA GeForce RTX 5080"));
        assert!(!is_blackwell("NVIDIA GeForce RTX 4090"));
    }

    #[test]
    fn decodes_the_8_8_fixed_point_temp() {
        // 69 °C → 69 * 256 = 0x4500 in the low 16 bits.
        assert_eq!(decode_temp_register(0x4500), Some(69));
        // 0 and out-of-range values are rejected.
        assert_eq!(decode_temp_register(0), None);
        assert_eq!(decode_temp_register(255 * 256), None);
    }

    #[test]
    fn new_is_none_without_the_library() {
        // On a box without the NVIDIA driver the dlopen fails and the client
        // degrades to None (machine-dependent: skipped when present).
        if std::path::Path::new("/proc/driver/nvidia").exists() {
            return;
        }
        assert!(NvApi::new().is_none());
    }
}

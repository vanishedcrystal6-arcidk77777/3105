//! Grappa token generation for AirTraffic sync auth.
//! Adapted from AirCard-iOS (MIT) — https://github.com/Mak5er/AirCard-iOS
//!
//! Calls `TBGetGrappaToken` defined in `GrappaHelper.m` (compiled into the
//! app), resolved dynamically via dlsym.

#[cfg(target_os = "ios")]
use std::ffi::CStr;

#[cfg(target_os = "ios")]
type TBGetGrappaTokenFn = unsafe extern "C" fn(
    in_version: u32,
    in_device_type: u32,
    in_protocol_version: u32,
    out_buf: *mut u8,
    max_len: usize,
    out_len: *mut usize,
    err_buf: *mut libc::c_char,
    err_len: usize,
) -> libc::c_int;

/// Generate an authentic Grappa client token for AirTraffic sync.
#[cfg(target_os = "ios")]
pub fn generate_grappa_token<F: Fn(&str)>(
    device_info: Option<(u32, u32, u32)>,
    log: F,
) -> Option<Vec<u8>> {
    unsafe {
        let sym_name = b"TBGetGrappaToken\0";
        let sym_ptr = libc::dlsym(libc::RTLD_DEFAULT, sym_name.as_ptr() as _);

        if sym_ptr.is_null() {
            log("bridge: grappa: TBGetGrappaToken symbol not found");
            return None;
        }

        let get_token_fn: TBGetGrappaTokenFn = std::mem::transmute(sym_ptr);

        let (ver, dt, pv) = device_info.unwrap_or((1, 0, 1));
        let mut buf = vec![0u8; 512];
        let mut out_len: usize = 0;
        let mut err_buf = [0 as libc::c_char; 256];

        let rc = get_token_fn(
            ver,
            dt,
            pv,
            buf.as_mut_ptr(),
            buf.len(),
            &mut out_len,
            err_buf.as_mut_ptr(),
            err_buf.len(),
        );

        if rc != 0 || out_len == 0 {
            let err_str = CStr::from_ptr(err_buf.as_ptr()).to_string_lossy();
            log(&format!("bridge: grappa: token generation failed (rc={rc}): {err_str}"));
            return None;
        }

        buf.truncate(out_len);
        log(&format!("bridge: grappa: generated token ({out_len} bytes)"));
        Some(buf)
    }
}

/// Non-iOS stub (grappa only exists on-device).
#[cfg(not(target_os = "ios"))]
pub fn generate_grappa_token<F: Fn(&str)>(
    _device_info: Option<(u32, u32, u32)>,
    _log: F,
) -> Option<Vec<u8>> {
    None
}

//! Protect the printer's LAN access code with Windows DPAPI (current user).
//! No third-party credential server or additional Rust dependency is required.
//! Other platforms deliberately do not persist the secret.
#[cfg(windows)]
mod platform {
    use std::ffi::c_void;
    use base64::Engine as _;

    #[repr(C)]
    struct DataBlob {
        cb_data: u32,
        pb_data: *mut u8,
    }

    #[link(name = "Crypt32")]
    extern "system" {
        fn CryptProtectData(
            input: *const DataBlob, description: *const u16,
            entropy: *const DataBlob, reserved: *mut c_void,
            prompt: *const c_void, flags: u32, output: *mut DataBlob,
        ) -> i32;
        fn CryptUnprotectData(
            input: *const DataBlob, description: *mut *mut u16,
            entropy: *const DataBlob, reserved: *mut c_void,
            prompt: *const c_void, flags: u32, output: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "Kernel32")]
    extern "system" {
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }

    const UI_FORBIDDEN: u32 = 0x1;

    pub fn protect(value: &str) -> Option<String> {
        if value.is_empty() { return None; }
        let input = DataBlob {
            cb_data: u32::try_from(value.len()).ok()?,
            pb_data: value.as_ptr() as *mut u8,
        };
        let mut output = DataBlob { cb_data: 0, pb_data: std::ptr::null_mut() };
        let ok = unsafe {
            CryptProtectData(
                &input, std::ptr::null(), std::ptr::null(),
                std::ptr::null_mut(), std::ptr::null(), UI_FORBIDDEN, &mut output,
            )
        };
        if ok == 0 || output.pb_data.is_null() { return None; }
        let bytes = unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize) };
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        unsafe { LocalFree(output.pb_data.cast()); }
        Some(encoded)
    }

    pub fn unprotect(encoded: &str) -> Option<String> {
        let mut input_bytes = base64::engine::general_purpose::STANDARD.decode(encoded).ok()?;
        if input_bytes.is_empty() { return None; }
        let input = DataBlob {
            cb_data: u32::try_from(input_bytes.len()).ok()?,
            pb_data: input_bytes.as_mut_ptr(),
        };
        let mut output = DataBlob { cb_data: 0, pb_data: std::ptr::null_mut() };
        let ok = unsafe {
            CryptUnprotectData(
                &input, std::ptr::null_mut(), std::ptr::null(),
                std::ptr::null_mut(), std::ptr::null(), UI_FORBIDDEN, &mut output,
            )
        };
        if ok == 0 || output.pb_data.is_null() { return None; }
        let result = unsafe { std::slice::from_raw_parts(output.pb_data, output.cb_data as usize) };
        let value = std::str::from_utf8(result).ok().map(str::to_owned);
        unsafe { LocalFree(output.pb_data.cast()); }
        value
    }
}

#[cfg(windows)]
pub use platform::{protect, unprotect};

#[cfg(not(windows))]
pub fn protect(_value: &str) -> Option<String> { None }
#[cfg(not(windows))]
pub fn unprotect(_value: &str) -> Option<String> { None }

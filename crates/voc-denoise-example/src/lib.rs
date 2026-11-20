//! 降噪插件接口（plugins/voc_denoise.h）的最小 Rust 实现：把音量减半。
//! 只用于验证客户端的插件加载流程；真正的 Krisp 适配见 plugins/krisp_shim。

use std::ffi::{c_char, c_void, CStr};

struct State {
    frame: u32,
}

#[no_mangle]
pub extern "C" fn voc_denoise_abi_version() -> u32 {
    1
}

#[no_mangle]
pub extern "C" fn voc_denoise_name() -> *const c_char {
    c"示例插件（音量减半）".as_ptr()
}

/// # Safety
/// 参数需符合 voc_denoise.h 的约定
#[no_mangle]
pub unsafe extern "C" fn voc_denoise_create(
    model_path: *const c_char,
    _sample_rate: u32,
    frame_size: u32,
    err: *mut c_char,
    err_len: u32,
) -> *mut c_void {
    let model = if model_path.is_null() { "" } else { CStr::from_ptr(model_path).to_str().unwrap_or("") };
    if model == "fail" {
        let msg = "故意失败（测试用）\0".as_bytes();
        if !err.is_null() && err_len as usize >= msg.len() {
            std::ptr::copy_nonoverlapping(msg.as_ptr(), err as *mut u8, msg.len());
        }
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(State { frame: frame_size })) as *mut c_void
}

/// # Safety
/// handle 来自 voc_denoise_create，in/out 各有 n 个元素
#[no_mangle]
pub unsafe extern "C" fn voc_denoise_process(handle: *mut c_void, input: *const f32, out: *mut f32, n: u32) -> i32 {
    let st = &*(handle as *const State);
    if n != st.frame {
        return -1;
    }
    let (i, o) = (std::slice::from_raw_parts(input, n as usize), std::slice::from_raw_parts_mut(out, n as usize));
    for (o, i) in o.iter_mut().zip(i) {
        *o = i * 0.5;
    }
    0
}

/// # Safety
/// handle 来自 voc_denoise_create，且只释放一次
#[no_mangle]
pub unsafe extern "C" fn voc_denoise_destroy(handle: *mut c_void) {
    if !handle.is_null() {
        drop(Box::from_raw(handle as *mut State));
    }
}

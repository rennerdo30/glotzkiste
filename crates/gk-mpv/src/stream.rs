//! mpv's stream callback API (`stream_cb.h`): mpv reads a custom protocol
//! through Rust code, in-process, with no local server.

use crate::ffi;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::Arc;

/// A byte source mpv reads from its demuxer thread.
pub trait StreamReader: Send + Sync {
    /// Blocks until at least one byte is available. `Ok(0)` ends the stream;
    /// an error aborts it.
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize>;
    /// Called from another thread to unblock a pending `read`.
    fn cancel(&self) {}
}

/// Opens a reader for a full URI (`protocol://rest`); `None` refuses it.
pub type OpenFn = Box<dyn Fn(&str) -> Option<Arc<dyn StreamReader>> + Send + Sync>;

type Cookie = Arc<dyn StreamReader>;

pub(crate) unsafe extern "C" fn open_trampoline(
    user: *mut c_void,
    uri: *mut c_char,
    info: *mut ffi::StreamCbInfo,
) -> c_int {
    let result = std::panic::catch_unwind(|| {
        if user.is_null() || uri.is_null() || info.is_null() {
            return ffi::ERROR_LOADING_FAILED;
        }
        // SAFETY: `user` is the boxed OpenFn registered by add_stream_protocol;
        // `uri` is a NUL-terminated string from mpv.
        let open = unsafe { &*(user as *const OpenFn) };
        let uri = unsafe { CStr::from_ptr(uri) }.to_string_lossy();
        let Some(reader) = open(&uri) else { return ffi::ERROR_LOADING_FAILED };
        let cookie = Box::into_raw(Box::new(reader)).cast::<c_void>();
        // SAFETY: `info` is mpv's struct to fill; the cookie is freed in close.
        unsafe {
            (*info).cookie = cookie;
            (*info).read_fn = Some(read_trampoline);
            (*info).seek_fn = None;
            (*info).size_fn = None;
            (*info).close_fn = Some(close_trampoline);
            (*info).cancel_fn = Some(cancel_trampoline);
        }
        0
    });
    result.unwrap_or(ffi::ERROR_LOADING_FAILED)
}

unsafe extern "C" fn read_trampoline(cookie: *mut c_void, buf: *mut c_char, n: u64) -> i64 {
    std::panic::catch_unwind(|| {
        // SAFETY: cookie is a live Box<Cookie> until close; buf has n bytes.
        let reader = unsafe { &*(cookie as *const Cookie) };
        let out = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), n.min(isize::MAX as u64) as usize) };
        match reader.read(out) {
            Ok(k) => k as i64,
            Err(_) => -1,
        }
    })
    .unwrap_or(-1)
}

unsafe extern "C" fn cancel_trampoline(cookie: *mut c_void) {
    let _ = std::panic::catch_unwind(|| {
        // SAFETY: as above; cancel may run concurrently with read, which the
        // trait requires readers to allow (`Sync`).
        let reader = unsafe { &*(cookie as *const Cookie) };
        reader.cancel();
    });
}

unsafe extern "C" fn close_trampoline(cookie: *mut c_void) {
    if !cookie.is_null() {
        // SAFETY: created by Box::into_raw in open; mpv calls close once.
        drop(unsafe { Box::from_raw(cookie as *mut Cookie) });
    }
}

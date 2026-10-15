//! Logging spine: forwards `tracing` events through the FFI callback.
//! Adapted from AirCard-iOS (MIT) — https://github.com/Mak5er/AirCard-iOS

use std::ffi::{c_char, c_void, CString};
use std::io;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::sync::OnceLock;

pub type LogCallback = Option<extern "C" fn(ctx: *mut c_void, msg: *const c_char)>;

static LOG_CB: AtomicUsize = AtomicUsize::new(0);
static LOG_CTX: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static INIT: OnceLock<()> = OnceLock::new();

/// Call the installed callback with `msg`, if one was registered.
pub fn emit(msg: &str) {
    let fn_addr = LOG_CB.load(Ordering::Relaxed);
    if fn_addr == 0 {
        return;
    }
    let cb: extern "C" fn(*mut c_void, *const c_char) =
        unsafe { std::mem::transmute(fn_addr) };
    let ctx = LOG_CTX.load(Ordering::Relaxed);
    if let Ok(c) = CString::new(msg) {
        cb(ctx, c.as_ptr());
    }
}

struct FfiWriter;

impl io::Write for FfiWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let msg = String::from_utf8_lossy(buf)
            .trim_end_matches('\n')
            .to_string();
        if !msg.is_empty() {
            emit(&msg);
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

unsafe impl Send for FfiWriter {}
unsafe impl Sync for FfiWriter {}

/// Install the global tracing subscriber. Returns 0 on success, 1 if already
/// initialised. Call once at launch.
pub fn init(cb: LogCallback, ctx: *mut c_void) -> i32 {
    if INIT.set(()).is_err() {
        return 1;
    }

    if let Some(cb_fn) = cb {
        LOG_CB.store(cb_fn as usize, Ordering::Relaxed);
        LOG_CTX.store(ctx, Ordering::Relaxed);
    }

    let subscriber = tracing_subscriber::fmt()
        .with_writer(|| FfiWriter)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
    0
}

/// Convenience: log one line to tracing (which forwards to the FFI callback).
pub fn log(msg: impl AsRef<str>) {
    tracing::info!("{}", msg.as_ref());
}

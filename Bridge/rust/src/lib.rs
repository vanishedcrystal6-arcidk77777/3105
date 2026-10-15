//! tofs_bridge — C FFI surface for the 3105 device bridge.
//!
//! Exports (see Bridge/include/tofs_bridge.h):
//!   tb_log_init / tb_pairing_run_host / tb_pairing_result_free
//!   tb_set_target_host / tb_set_target_hosts / tb_clear_target_hosts
//!   tb_open / tb_close
//!   tb_services_json / tb_list_apps_json / tb_find_container
//!   tb_fs_list_json / tb_fs_read / tb_fs_write / tb_fs_mkdir / tb_fs_remove / tb_fs_rename
//!   tb_atc_write_file / tb_atc_write_batch
//!   tb_self_test_json
//!   tb_string_free / tb_buf_free

mod atc;
mod ffi_util;
mod fileservice;
mod grappa;
mod logging;
mod pairing;
mod tunnel;

use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use tokio::runtime::Runtime;

pub use pairing::{TBPairPinCb, TBPairReadyCb, TBPairResult};

// ---------------------------------------------------------------------------
// Runtime + session registry
// ---------------------------------------------------------------------------

pub(crate) struct Session {
    tunnel: tunnel::AppDeviceTunnel,
    fs_clients: HashMap<String, fileservice::FsClient>,
}

impl Session {
    async fn fs_client(&mut self, bundle_id: &str) -> Result<&mut fileservice::FsClient, String> {
        if !self.fs_clients.contains_key(bundle_id) {
            let client = fileservice::FsClient::connect(&mut self.tunnel, bundle_id).await?;
            self.fs_clients.insert(bundle_id.to_string(), client);
        }
        Ok(self.fs_clients.get_mut(bundle_id).unwrap())
    }
}

static SESSIONS: OnceLock<Mutex<HashMap<u32, Session>>> = OnceLock::new();
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(1);
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

pub(crate) fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

fn sessions() -> &'static Mutex<HashMap<u32, Session>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `f` on a large-stack worker thread and block until done. Panics inside
/// `f` are converted to Err.
fn ffi_block<F, R>(name: &str, f: F) -> Result<R, String>
where
    F: FnOnce() -> Result<R, String> + Send + 'static,
    R: Send + 'static,
{
    match ffi_util::run_with_large_stack(name, f) {
        Ok(r) => r,
        Err(panic) => Err(panic),
    }
}

/// Take the session out of the registry, run the async op, put it back.
async fn with_session<F, R>(handle: u32, f: F) -> Result<R, String>
where
    F: for<'a> FnOnce(
        &'a mut Session,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<R, String>> + 'a>>,
{
    let mut session = {
        let mut lock = sessions()
            .lock()
            .map_err(|_| "session registry poisoned".to_string())?;
        lock.remove(&handle)
            .ok_or_else(|| "invalid session handle".to_string())?
    };
    let res = f(&mut session).await;
    if let Ok(mut lock) = sessions().lock() {
        lock.insert(handle, session);
    }
    res
}

// ---------------------------------------------------------------------------
// FFI helpers
// ---------------------------------------------------------------------------

unsafe fn set_err(out_error: *mut *mut c_char, msg: impl std::fmt::Display) {
    if !out_error.is_null() {
        *out_error = ffi_util::cstr(msg);
    }
}

unsafe fn set_out_str(out: *mut *mut c_char, s: impl std::fmt::Display) {
    if !out.is_null() {
        *out = ffi_util::cstr(s);
    }
}

macro_rules! ffi_entry {
    ($name:expr, $out_error:expr, $body:expr) => {{
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body));
        match res {
            Ok(rc) => rc,
            Err(e) => {
                set_err($out_error, format!("rust panic in {}: {:?}", $name, e));
                1
            }
        }
    }};
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn tb_log_init(cb: logging::LogCallback, ctx: *mut c_void) -> i32 {
    logging::init(cb, ctx)
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn tb_pairing_run_host(
    bind_addr: *const c_char,
    port: u16,
    name: *const c_char,
    model: *const c_char,
    out_path: *const c_char,
    host_alt_irk_hex: *const c_char,
    ready_cb: TBPairReadyCb,
    pin_cb: TBPairPinCb,
    ctx: *mut c_void,
    out: *mut TBPairResult,
) -> i32 {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pairing::run_host(
            bind_addr,
            port,
            name,
            model,
            out_path,
            host_alt_irk_hex,
            ready_cb,
            pin_cb,
            ctx,
            out,
        )
    }));
    match res {
        Ok(rc) => rc,
        Err(e) => {
            if !out.is_null() {
                (*out).error = ffi_util::cstr(format!("rust panic in tb_pairing_run_host: {e:?}"));
            }
            1
        }
    }
}

/// # Safety
/// `r` must be null or a `TBPairResult` populated by `tb_pairing_run_host`.
#[no_mangle]
pub unsafe extern "C" fn tb_pairing_result_free(r: *mut TBPairResult) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pairing::result_free(r);
    }));
}

// ---------------------------------------------------------------------------
// Target host configuration
// ---------------------------------------------------------------------------

/// # Safety
/// `host` must be null or a valid C string.
#[no_mangle]
pub unsafe extern "C" fn tb_set_target_host(host: *const c_char) -> i32 {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let h = if host.is_null() {
            None
        } else {
            let s = ffi_util::opt_str(host, "").trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        };
        tunnel::set_target_host(h);
        0
    }));
    res.unwrap_or(1)
}

/// # Safety
/// `hosts` must be null or point to `count` valid-or-null C strings.
#[no_mangle]
pub unsafe extern "C" fn tb_set_target_hosts(hosts: *const *const c_char, count: usize) -> i32 {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut list = Vec::new();
        if !hosts.is_null() {
            for i in 0..count {
                let p = *hosts.add(i);
                if !p.is_null() {
                    let s = ffi_util::opt_str(p, "").trim().to_string();
                    if !s.is_empty() && !list.contains(&s) {
                        list.push(s);
                    }
                }
            }
        }
        tunnel::set_target_hosts(list);
        0
    }));
    res.unwrap_or(1)
}

#[no_mangle]
pub extern "C" fn tb_clear_target_hosts() {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tunnel::clear_target_hosts();
    }));
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_open(
    pairing_path: *const c_char,
    out_handle: *mut u32,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_open", out_error, {
        if out_handle.is_null() {
            set_err(out_error, "out_handle is null");
            return 2;
        }
        *out_handle = 0;
        let pairing_path = ffi_util::opt_str(pairing_path, "");
        if pairing_path.is_empty() {
            set_err(out_error, "pairing_path is empty");
            return 1;
        }
        match ffi_block("tb_open", move || {
            runtime().block_on(async {
                let bytes = std::fs::read(&pairing_path)
                    .map_err(|e| format!("read pairing file '{pairing_path}': {e}"))?;
                let tun = tunnel::connect_tunnel(&bytes).await?;
                let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
                let mut lock = sessions()
                    .lock()
                    .map_err(|_| "session registry poisoned".to_string())?;
                lock.insert(
                    handle,
                    Session {
                        tunnel: tun,
                        fs_clients: HashMap::new(),
                    },
                );
                Ok(handle)
            })
        }) {
            Ok(h) => {
                *out_handle = h;
                0
            }
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn tb_close(handle: u32) -> i32 {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Ok(mut lock) = sessions().lock() {
            lock.remove(&handle);
        }
    }));
    0
}

// ---------------------------------------------------------------------------
// Info / diagnostics
// ---------------------------------------------------------------------------

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_services_json(
    handle: u32,
    out_json: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_services_json", out_error, {
        match ffi_block("tb_services_json", move || {
            runtime().block_on(with_session(handle, |s| Box::pin(async move {
                let services = s
                    .tunnel
                    .rsd_services()
                    .ok_or_else(|| "not an RSD tunnel".to_string())?;
                let list: Vec<serde_json::Value> = services
                    .iter()
                    .map(|(name, port)| serde_json::json!({ "name": name, "port": port }))
                    .collect();
                serde_json::to_string(&list).map_err(|e| e.to_string())
            })))
        }) {
            Ok(j) => {
                set_out_str(out_json, j);
                0
            }
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_find_container(
    handle: u32,
    bundle_id: *const c_char,
    out_path: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_find_container", out_error, {
        let bundle_id = ffi_util::opt_str(bundle_id, "");
        if bundle_id.is_empty() {
            set_err(out_error, "bundle_id is empty");
            return 1;
        }
        match ffi_block("tb_find_container", move || {
            runtime().block_on(with_session(handle, |s| {
                let bundle_id = bundle_id.clone();
                Box::pin(async move { find_container_inner(s, &bundle_id).await })
            }))
        }) {
            Ok(p) => {
                set_out_str(out_path, p);
                0
            }
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

async fn find_container_inner(s: &mut Session, bundle_id: &str) -> Result<String, String> {
    let mut inst = s.tunnel.connect_installation_proxy().await?;
    let apps = inst
        .get_apps(Some("Any"), Some(vec![bundle_id.to_string()]))
        .await
        .map_err(|e| format!("installation_proxy lookup '{bundle_id}': {e:?}"))?;
    if let Some(info) = apps.get(bundle_id) {
        if let Some(dict) = info.as_dictionary() {
            if let Some(container) = dict.get("Container").and_then(|v| v.as_string()) {
                return Ok(normalize_path(container));
            }
        }
    }
    Err(format!(
        "container for '{bundle_id}' not found (is the app installed?)"
    ))
}

fn normalize_path(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("/private/var/") {
        format!("/var/{rest}")
    } else {
        p.to_string()
    }
}

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_list_apps_json(
    handle: u32,
    out_json: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_list_apps_json", out_error, {
        match ffi_block("tb_list_apps_json", move || {
            runtime().block_on(with_session(handle, |s| Box::pin(async move {
                let mut inst = s.tunnel.connect_installation_proxy().await?;
                let apps = inst
                    .get_apps(Some("User"), None)
                    .await
                    .map_err(|e| format!("installation_proxy lookup: {e:?}"))?;
                let mut list: Vec<serde_json::Value> = Vec::new();
                for (bid, info) in apps.iter() {
                    let dict = info.as_dictionary();
                    let name = dict
                        .and_then(|d| {
                            d.get("CFBundleDisplayName")
                                .or_else(|| d.get("CFBundleName"))
                        })
                        .and_then(|v| v.as_string())
                        .unwrap_or("");
                    let container = dict
                        .and_then(|d| d.get("Container"))
                        .and_then(|v| v.as_string())
                        .map(normalize_path);
                    let mut entry = serde_json::json!({ "bundleID": bid, "name": name });
                    if let Some(c) = container {
                        entry["container"] = serde_json::Value::String(c);
                    }
                    list.push(entry);
                }
                list.sort_by(|a, b| {
                    let an = a["name"].as_str().unwrap_or("");
                    let bn = b["name"].as_str().unwrap_or("");
                    an.cmp(bn)
                });
                serde_json::to_string(&list).map_err(|e| e.to_string())
            })))
        }) {
            Ok(j) => {
                set_out_str(out_json, j);
                0
            }
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

// ---------------------------------------------------------------------------
// File service (container-relative)
// ---------------------------------------------------------------------------

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_fs_list_json(
    handle: u32,
    bundle_id: *const c_char,
    rel_path: *const c_char,
    out_json: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_fs_list_json", out_error, {
        let bundle_id = ffi_util::opt_str(bundle_id, "");
        let rel_path = {
            let p = ffi_util::opt_str(rel_path, ".");
            if p.is_empty() { ".".to_string() } else { p }
        };
        match ffi_block("tb_fs_list_json", move || {
            runtime().block_on(with_session(handle, |s| {
                let bundle_id = bundle_id.clone();
                let rel_path = rel_path.clone();
                Box::pin(async move {
                    let fs = s.fs_client(&bundle_id).await?;
                    let names = fs.list(&rel_path).await?;
                    let mut entries: Vec<serde_json::Value> = Vec::new();
                    for name in names.into_iter().take(4000) {
                        let child = join_rel(&rel_path, &name);
                        let mut entry = serde_json::json!({ "name": name });
                        if let Ok(attrs) = fs.attributes(&child).await {
                            entry["attrs"] = attrs;
                        }
                        entries.push(entry);
                    }
                    serde_json::to_string(&entries).map_err(|e| e.to_string())
                })
            }))
        }) {
            Ok(j) => {
                set_out_str(out_json, j);
                0
            }
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

fn join_rel(base: &str, name: &str) -> String {
    let b = base.trim_matches('/');
    if b.is_empty() || b == "." {
        name.to_string()
    } else {
        format!("{b}/{name}")
    }
}

/// # Safety
/// All pointer args must be null or valid for their documented use.
/// `*out_buf` must be freed with `tb_buf_free`.
#[no_mangle]
pub unsafe extern "C" fn tb_fs_read(
    handle: u32,
    bundle_id: *const c_char,
    rel_path: *const c_char,
    out_buf: *mut *mut u8,
    out_len: *mut usize,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_fs_read", out_error, {
        if out_buf.is_null() || out_len.is_null() {
            set_err(out_error, "out_buf/out_len null");
            return 2;
        }
        *out_buf = std::ptr::null_mut();
        *out_len = 0;
        let bundle_id = ffi_util::opt_str(bundle_id, "");
        let rel_path = ffi_util::opt_str(rel_path, "");
        match ffi_block("tb_fs_read", move || {
            runtime().block_on(with_session(handle, |s| {
                let bundle_id = bundle_id.clone();
                let rel_path = rel_path.clone();
                Box::pin(async move {
                    let fs = s.fs_client(&bundle_id).await?;
                    fs.read_file(&rel_path).await
                })
            }))
        }) {
            Ok(mut data) => {
                let len = data.len();
                data.shrink_to_fit();
                let ptr = data.as_mut_ptr();
                std::mem::forget(data);
                *out_buf = ptr;
                *out_len = len;
                0
            }
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

/// # Safety
/// `data` must point to `len` readable bytes; other pointers null or valid.
#[no_mangle]
pub unsafe extern "C" fn tb_fs_write(
    handle: u32,
    bundle_id: *const c_char,
    rel_path: *const c_char,
    data: *const u8,
    len: usize,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_fs_write", out_error, {
        if data.is_null() && len > 0 {
            set_err(out_error, "data is null");
            return 2;
        }
        let bundle_id = ffi_util::opt_str(bundle_id, "");
        let rel_path = ffi_util::opt_str(rel_path, "");
        let payload = if len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, len).to_vec()
        };
        match ffi_block("tb_fs_write", move || {
            runtime().block_on(with_session(handle, |s| {
                let bundle_id = bundle_id.clone();
                let rel_path = rel_path.clone();
                let payload = payload.clone();
                Box::pin(async move {
                    let fs = s.fs_client(&bundle_id).await?;
                    fs.write_file(&rel_path, &payload).await
                })
            }))
        }) {
            Ok(()) => 0,
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_fs_mkdir(
    handle: u32,
    bundle_id: *const c_char,
    rel_path: *const c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_fs_mkdir", out_error, {
        let bundle_id = ffi_util::opt_str(bundle_id, "");
        let rel_path = ffi_util::opt_str(rel_path, "");
        match ffi_block("tb_fs_mkdir", move || {
            runtime().block_on(with_session(handle, |s| {
                let bundle_id = bundle_id.clone();
                let rel_path = rel_path.clone();
                Box::pin(async move {
                    let fs = s.fs_client(&bundle_id).await?;
                    fs.mkdir(&rel_path).await
                })
            }))
        }) {
            Ok(()) => 0,
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_fs_remove(
    handle: u32,
    bundle_id: *const c_char,
    rel_path: *const c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_fs_remove", out_error, {
        let bundle_id = ffi_util::opt_str(bundle_id, "");
        let rel_path = ffi_util::opt_str(rel_path, "");
        match ffi_block("tb_fs_remove", move || {
            runtime().block_on(with_session(handle, |s| {
                let bundle_id = bundle_id.clone();
                let rel_path = rel_path.clone();
                Box::pin(async move {
                    let fs = s.fs_client(&bundle_id).await?;
                    fs.remove(&rel_path).await
                })
            }))
        }) {
            Ok(()) => 0,
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_fs_rename(
    handle: u32,
    bundle_id: *const c_char,
    old_rel: *const c_char,
    new_rel: *const c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_fs_rename", out_error, {
        let bundle_id = ffi_util::opt_str(bundle_id, "");
        let old_rel = ffi_util::opt_str(old_rel, "");
        let new_rel = ffi_util::opt_str(new_rel, "");
        match ffi_block("tb_fs_rename", move || {
            runtime().block_on(with_session(handle, |s| {
                let bundle_id = bundle_id.clone();
                let old_rel = old_rel.clone();
                let new_rel = new_rel.clone();
                Box::pin(async move {
                    let fs = s.fs_client(&bundle_id).await?;
                    fs.rename(&old_rel, &new_rel).await
                })
            }))
        }) {
            Ok(()) => 0,
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

// ---------------------------------------------------------------------------
// ATC arbitrary-path writes
// ---------------------------------------------------------------------------

/// # Safety
/// `data` must point to `len` readable bytes; other pointers null or valid.
#[no_mangle]
pub unsafe extern "C" fn tb_atc_write_file(
    handle: u32,
    abs_path: *const c_char,
    data: *const u8,
    len: usize,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_atc_write_file", out_error, {
        if data.is_null() && len > 0 {
            set_err(out_error, "data is null");
            return 2;
        }
        let abs_path = ffi_util::opt_str(abs_path, "");
        let payload = if len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, len).to_vec()
        };
        match ffi_block("tb_atc_write_file", move || {
            runtime().block_on(with_session(handle, |s| {
                let abs_path = abs_path.clone();
                let payload = payload.clone();
                Box::pin(async move { atc::write_file(&mut s.tunnel, &abs_path, &payload).await })
            }))
        }) {
            Ok(()) => 0,
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

/// # Safety
/// `names` points to `count` C strings; `datas` to `count` buffers whose
/// lengths come from `lens`.
#[no_mangle]
pub unsafe extern "C" fn tb_atc_write_batch(
    handle: u32,
    target_dir: *const c_char,
    names: *const *const c_char,
    datas: *const *const u8,
    lens: *const usize,
    count: usize,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_atc_write_batch", out_error, {
        if count > 0 && (names.is_null() || datas.is_null() || lens.is_null()) {
            set_err(out_error, "null array pointer");
            return 2;
        }
        let target_dir = ffi_util::opt_str(target_dir, "");
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..count {
            let name = ffi_util::opt_str(*names.add(i), "");
            if name.is_empty() || name.contains('/') {
                set_err(out_error, format!("bad leaf name: '{name}'"));
                return 1;
            }
            let len = *lens.add(i);
            let data = if len == 0 {
                Vec::new()
            } else {
                let p = *datas.add(i);
                if p.is_null() {
                    set_err(out_error, format!("null data for '{name}'"));
                    return 2;
                }
                std::slice::from_raw_parts(p, len).to_vec()
            };
            files.push((name, data));
        }
        match ffi_block("tb_atc_write_batch", move || {
            runtime().block_on(with_session(handle, |s| {
                let target_dir = target_dir.clone();
                let files = files.clone();
                Box::pin(async move { atc::write_files(&mut s.tunnel, &target_dir, &files).await })
            }))
        }) {
            Ok(()) => 0,
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Self test: ATC-write a canary into our own container, read it back via the
// file service, byte-compare, remove it. Exercises both paths end to end.
// ---------------------------------------------------------------------------

/// # Safety
/// All pointer args must be null or valid for their documented use.
#[no_mangle]
pub unsafe extern "C" fn tb_self_test_json(
    handle: u32,
    own_bundle_id: *const c_char,
    out_json: *mut *mut c_char,
    out_error: *mut *mut c_char,
) -> i32 {
    ffi_entry!("tb_self_test_json", out_error, {
        let own_bundle_id = ffi_util::opt_str(own_bundle_id, "");
        if own_bundle_id.is_empty() {
            set_err(out_error, "own_bundle_id is empty");
            return 1;
        }
        match ffi_block("tb_self_test_json", move || {
            runtime().block_on(with_session(handle, |s| {
                let own_bundle_id = own_bundle_id.clone();
                Box::pin(async move {
                    let container = find_container_inner(s, &own_bundle_id).await?;
                    let leaf = format!(".tofs-canary-{}", uuid::Uuid::new_v4().simple());
                    let rel = format!("Documents/{leaf}");
                    let abs = format!("{container}/{rel}");

                    let mut payload = vec![0u8; 64];
                    {
                        use std::io::Read;
                        let _ = std::fs::File::open("/dev/urandom")
                            .and_then(|mut f| f.read_exact(&mut payload));
                    }

                    atc::write_file(&mut s.tunnel, &abs, &payload).await?;

                    let fs = s.fs_client(&own_bundle_id).await?;
                    let readback = fs.read_file(&rel).await.unwrap_or_default();
                    let matches = readback == payload;
                    let _ = fs.remove(&rel).await;

                    let json = serde_json::json!({
                        "ok": matches,
                        "container": container,
                        "bytes": payload.len(),
                        "readbackBytes": readback.len(),
                        "exactBytes": matches,
                    });
                    serde_json::to_string_pretty(&json).map_err(|e| e.to_string())
                })
            }))
        }) {
            Ok(j) => {
                set_out_str(out_json, j);
                0
            }
            Err(e) => {
                set_err(out_error, e);
                1
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Memory management
// ---------------------------------------------------------------------------

/// # Safety
/// `p` must be null or a pointer returned by this library.
#[no_mangle]
pub unsafe extern "C" fn tb_string_free(p: *mut c_char) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ffi_util::string_free(p);
    }));
}

/// # Safety
/// `p`/`len` must be a pair returned by `tb_fs_read`.
#[no_mangle]
pub unsafe extern "C" fn tb_buf_free(p: *mut u8, len: usize) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !p.is_null() {
            drop(Vec::from_raw_parts(p, len, len));
        }
    }));
}

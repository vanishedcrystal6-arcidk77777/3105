//! Tunnel establishment and service plumbing.
//! Adapted from AirCard-iOS (MIT) — https://github.com/Mak5er/AirCard-iOS
//! with pure-`idevice`-crate tunnel creation (no idevice-ffi dependency).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use idevice::afc::AfcClient;
use idevice::pairing_file::PairingFile;
use idevice::provider::{IdeviceProvider, TcpProvider};
use idevice::remote_pairing::{
    connect_tls_psk_tunnel_native, RemotePairingClient, RpPairingFile, RpPairingSocket,
    RpPairingSocketProvider,
};
use idevice::services::installation_proxy::InstallationProxyClient;
use idevice::services::lockdown::LockdownClient;
use idevice::services::rsd::RsdHandshake;
use idevice::{Idevice, IdeviceService, ReadWrite, RemoteXpcClient};

pub const RSD_PORT: u16 = 49152;
pub const LOCKDOWN_PORT: u16 = 62078;
pub const HOST_LABEL: &str = "3105";

// ---------------------------------------------------------------------------
// Tunnel type
// ---------------------------------------------------------------------------

pub enum AppDeviceTunnel {
    Rsd {
        adapter: idevice::tcp::handle::AdapterHandle,
        handshake: RsdHandshake,
    },
    Lockdown {
        provider: TcpProvider,
        pairing_file: PairingFile,
        legacy: bool,
    },
}

impl AppDeviceTunnel {
    pub async fn connect_afc(&mut self) -> Result<AfcClient, String> {
        match self {
            AppDeviceTunnel::Rsd { adapter, handshake } => handshake
                .connect::<AfcClient>(adapter)
                .await
                .map_err(|e| format!("AFC connect over RSD failed: {e:?}")),
            AppDeviceTunnel::Lockdown {
                provider,
                pairing_file,
                legacy,
            } => {
                let stream =
                    connect_service_lockdown(provider, pairing_file, *legacy, "com.apple.afc")
                        .await?;
                let idevice = Idevice::new(stream, HOST_LABEL);
                Ok(AfcClient::new(idevice))
            }
        }
    }

    pub async fn connect_installation_proxy(&mut self) -> Result<InstallationProxyClient, String> {
        match self {
            AppDeviceTunnel::Rsd { adapter, handshake } => handshake
                .connect::<InstallationProxyClient>(adapter)
                .await
                .map_err(|e| format!("InstallationProxy connect over RSD failed: {e:?}")),
            AppDeviceTunnel::Lockdown {
                provider,
                pairing_file,
                legacy,
            } => {
                let stream = connect_service_lockdown(
                    provider,
                    pairing_file,
                    *legacy,
                    "com.apple.mobile.installation_proxy",
                )
                .await?;
                let idevice = Idevice::new(stream, HOST_LABEL);
                Ok(InstallationProxyClient::new(idevice))
            }
        }
    }

    /// Connect to a lockdown-style service (AFC shims, com.apple.atc,
    /// streaming_zip_conduit, diagnostics_relay, ...). Performs the RSD
    /// checkin when the service is reached through an RSD shim.
    pub async fn connect_service(
        &mut self,
        service_base: &str,
    ) -> Result<Box<dyn ReadWrite>, String> {
        match self {
            AppDeviceTunnel::Rsd { adapter, handshake } => {
                connect_service_rsd(adapter, handshake, service_base).await
            }
            AppDeviceTunnel::Lockdown {
                provider,
                pairing_file,
                legacy,
            } => connect_service_lockdown(provider, pairing_file, *legacy, service_base).await,
        }
    }

    /// Connect to a CoreDevice XPC service (no RSD checkin; speaks RemoteXPC
    /// directly on the advertised port).
    pub async fn connect_xpc_service(
        &mut self,
        service_name: &str,
    ) -> Result<Box<dyn ReadWrite>, String> {
        match self {
            AppDeviceTunnel::Rsd { adapter, handshake } => {
                let s = handshake
                    .services
                    .get(service_name)
                    .ok_or_else(|| format!("RSD does not advertise '{service_name}'"))?;
                let port = s.port;
                let stream = adapter
                    .connect(port)
                    .await
                    .map_err(|e| format!("connect to '{service_name}' port {port}: {e:?}"))?;
                Ok(Box::new(stream) as Box<dyn ReadWrite>)
            }
            AppDeviceTunnel::Lockdown { .. } => Err(
                "CoreDevice XPC services require the RSD tunnel (not available over Lockdown)"
                    .to_string(),
            ),
        }
    }

    pub fn rsd_services(&self) -> Option<Vec<(String, u16)>> {
        match self {
            AppDeviceTunnel::Rsd { handshake, .. } => Some(
                handshake
                    .services
                    .iter()
                    .map(|(k, v)| (k.clone(), v.port))
                    .collect(),
            ),
            AppDeviceTunnel::Lockdown { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// RSD service connection (shim -> bare -> fresh Lockdown start)
// ---------------------------------------------------------------------------

async fn connect_service_rsd(
    adapter: &mut idevice::tcp::handle::AdapterHandle,
    handshake: &mut RsdHandshake,
    service_base: &str,
) -> Result<Box<dyn ReadWrite>, String> {
    // 1. Standard shim name, e.g. "com.apple.atc.shim.remote"
    let shim_name = format!("{service_base}.shim.remote");
    if let Some(s) = handshake.services.get(&shim_name) {
        let port = s.port;
        let stream = adapter
            .connect(port)
            .await
            .map_err(|e| format!("connect to '{shim_name}' port {port}: {e:?}"))?;
        let mut dev = Idevice::new(Box::new(stream), HOST_LABEL);
        dev.rsd_checkin()
            .await
            .map_err(|e| format!("RSD checkin for '{shim_name}' failed: {e:?}"))?;
        return dev
            .get_socket()
            .ok_or_else(|| "no socket after rsd_checkin".to_string());
    }

    // 2. Bare name as an RSD-advertised lockdown shim
    if let Some(s) = handshake.services.get(service_base) {
        let port = s.port;
        let stream = adapter
            .connect(port)
            .await
            .map_err(|e| format!("connect to '{service_base}' port {port}: {e:?}"))?;
        let mut dev = Idevice::new(Box::new(stream), HOST_LABEL);
        dev.rsd_checkin()
            .await
            .map_err(|e| format!("RSD checkin for '{service_base}' failed: {e:?}"))?;
        return dev
            .get_socket()
            .ok_or_else(|| "no socket after rsd_checkin".to_string());
    }

    // 3. Fresh Lockdown connection through the tunnel, then StartService
    let mut lockdown = handshake
        .connect::<LockdownClient>(adapter)
        .await
        .map_err(|e| format!("Lockdown connect for '{service_base}' failed: {e:?}"))?;

    let (port, _ssl) = lockdown
        .start_service(service_base)
        .await
        .map_err(|e| format!("Lockdown start_service('{service_base}') failed: {e:?}"))?;

    let stream = adapter
        .connect(port)
        .await
        .map_err(|e| format!("connect to service port {port} failed: {e:?}"))?;
    Ok(Box::new(stream))
}

async fn connect_service_lockdown(
    provider: &TcpProvider,
    pairing_file: &PairingFile,
    legacy: bool,
    service_name: &str,
) -> Result<Box<dyn ReadWrite>, String> {
    let mut lockdown = LockdownClient::connect(provider)
        .await
        .map_err(|e| format!("Lockdown connect failed: {e:?}"))?;

    let _ = lockdown
        .start_session(pairing_file)
        .await
        .map_err(|e| format!("Lockdown start_session failed: {e:?}"))?;

    let (port, ssl) = lockdown
        .start_service(service_name)
        .await
        .map_err(|e| format!("Lockdown start_service('{service_name}') failed: {e:?}"))?;

    let mut idevice = provider
        .connect(port)
        .await
        .map_err(|e| format!("connect to service port {port} failed: {e:?}"))?;

    if ssl {
        idevice
            .start_session(pairing_file, legacy)
            .await
            .map_err(|e| format!("service TLS session failed: {e:?}"))?;
    }

    idevice
        .get_socket()
        .ok_or_else(|| "no socket from idevice".to_string())
}

// ---------------------------------------------------------------------------
// Target host configuration + route cache
// ---------------------------------------------------------------------------

static TARGET_HOST: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static EXTRA_TARGET_HOSTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[derive(Clone, Debug)]
pub enum CachedRouteMode {
    RsdRawRpPairing,
    RsdRemoteXpc,
    Lockdown,
}

#[derive(Clone, Debug)]
pub struct CachedRoute {
    pub host: String,
    pub mode: CachedRouteMode,
}

static LAST_WORKING_ROUTE: std::sync::Mutex<Option<CachedRoute>> = std::sync::Mutex::new(None);

pub fn set_last_working_route(route: CachedRoute) {
    if let Ok(mut lock) = LAST_WORKING_ROUTE.lock() {
        *lock = Some(route);
    }
}

pub fn get_last_working_route() -> Option<CachedRoute> {
    LAST_WORKING_ROUTE.lock().ok().and_then(|g| g.clone())
}

pub fn clear_last_working_route() {
    if let Ok(mut lock) = LAST_WORKING_ROUTE.lock() {
        *lock = None;
    }
}

pub fn set_target_host(host: Option<String>) {
    if let Ok(mut lock) = TARGET_HOST.lock() {
        *lock = host;
    }
}

pub fn set_target_hosts(hosts: Vec<String>) {
    if let Ok(mut lock) = EXTRA_TARGET_HOSTS.lock() {
        *lock = hosts;
    }
}

pub fn clear_target_hosts() {
    if let Ok(mut lock) = TARGET_HOST.lock() {
        *lock = None;
    }
    if let Ok(mut lock) = EXTRA_TARGET_HOSTS.lock() {
        lock.clear();
    }
    clear_last_working_route();
}

fn resolve_target_hosts() -> Vec<String> {
    let mut targets: Vec<String> = Vec::new();

    if let Ok(guard) = TARGET_HOST.lock() {
        if let Some(ref host) = *guard {
            let h = host.trim();
            if !h.is_empty() && !targets.contains(&h.to_string()) {
                targets.push(h.to_string());
            }
        }
    }

    if let Ok(guard) = EXTRA_TARGET_HOSTS.lock() {
        for host in guard.iter() {
            let h = host.trim();
            if !h.is_empty() && !targets.contains(&h.to_string()) {
                targets.push(h.to_string());
            }
        }
    }

    // Known loopback-VPN gateway IPs (LocalDevVPN default first).
    // NB: never try 127.0.0.1 — remotepairingdeviced enters a policy-drop
    // state on local-machine connections until Developer Mode is toggled.
    for fb in ["10.7.0.1", "10.7.0.2", "10.7.0.3", "10.0.0.1", "172.16.0.1"] {
        let s = fb.to_string();
        if !targets.contains(&s) {
            targets.push(s);
        }
    }

    targets
}

// ---------------------------------------------------------------------------
// Tunnel creation (pure idevice crate)
// ---------------------------------------------------------------------------

/// After RPPairing completes (either transport), ask the device to open a TCP
/// listener, dial it, wrap in TLS-PSK + CDTunnel and run the RSD handshake
/// through the resulting software tunnel.
async fn finish_tunnel<R: RpPairingSocketProvider>(
    rpc: &mut RemotePairingClient<R>,
    connect_addr: SocketAddr,
) -> Result<(idevice::tcp::handle::AdapterHandle, RsdHandshake), String> {
    let tunnel_port = rpc
        .create_tcp_listener()
        .await
        .map_err(|e| format!("create_tcp_listener: {e:?}"))?;

    let mut tunnel_addr = connect_addr;
    tunnel_addr.set_port(tunnel_port);
    let tunnel_stream = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        tokio::net::TcpStream::connect(tunnel_addr),
    )
    .await
    .map_err(|_| "TLS tunnel connect timed out".to_string())?
    .map_err(|e| format!("TLS tunnel connect: {e}"))?;

    let tunnel = connect_tls_psk_tunnel_native(tunnel_stream, rpc.encryption_key())
        .await
        .map_err(|e| format!("TLS-PSK/CDTunnel handshake: {e:?}"))?;

    let client_ip: IpAddr = tunnel
        .info
        .client_address
        .parse()
        .map_err(|e| format!("bad client addr: {e}"))?;
    let server_ip: IpAddr = tunnel
        .info
        .server_address
        .parse()
        .map_err(|e| format!("bad server addr: {e}"))?;
    let mtu = tunnel.info.mtu as usize;
    let rsd_port = tunnel.info.server_rsd_port;

    let raw = tunnel.into_inner();
    let mut adapter = idevice::tcp::adapter::Adapter::new(Box::new(raw), client_ip, server_ip);
    adapter.set_mss(mtu.saturating_sub(60));
    let mut adapter = adapter.to_async_handle();

    let rsd_stream = adapter
        .connect(rsd_port)
        .await
        .map_err(|e| format!("RSD connect through tunnel: {e:?}"))?;
    let handshake = RsdHandshake::new(rsd_stream)
        .await
        .map_err(|e| format!("RSD handshake: {e:?}"))?;

    Ok((adapter, handshake))
}

async fn try_rsd(
    target_ips: &[String],
    mut rpf: RpPairingFile,
    last_err: &mut String,
) -> Option<AppDeviceTunnel> {
    crate::logging::log(format!(
        "bridge: attempting RSD tunnel on port {RSD_PORT}..."
    ));

    for ip_str in target_ips {
        let Ok(ip) = ip_str.parse::<Ipv4Addr>() else {
            continue;
        };
        if ip.is_loopback() {
            continue;
        }
        let socket_addr = SocketAddr::new(IpAddr::V4(ip), RSD_PORT);

        // Path 1: RemoteXPC (remotepairingd speaks RSD on 49152)
        let xpc_res = tokio::time::timeout(std::time::Duration::from_millis(2500), async {
            let rsd_stream = tokio::net::TcpStream::connect(socket_addr)
                .await
                .map_err(|e| format!("RSD connect: {e}"))?;
            let rsd_handshake = RsdHandshake::new(rsd_stream)
                .await
                .map_err(|e| format!("RSD handshake: {e:?}"))?;

            let ts = rsd_handshake
                .services
                .get("com.apple.internal.dt.coredevice.untrusted.tunnelservice")
                .ok_or_else(|| "tunnel service not advertised".to_string())?;

            let ts_addr = SocketAddr::new(socket_addr.ip(), ts.port);
            let ts_stream = tokio::net::TcpStream::connect(ts_addr)
                .await
                .map_err(|e| format!("tunnel service connect: {e}"))?;
            let mut conn = RemoteXpcClient::new(ts_stream)
                .await
                .map_err(|e| format!("RemoteXPC init: {e:?}"))?;
            conn.do_handshake()
                .await
                .map_err(|e| format!("RemoteXPC handshake: {e:?}"))?;
            let _ = conn.recv_root().await;

            let mut rpc = RemotePairingClient::new(conn, HOST_LABEL);
            rpc.connect(&mut rpf, async || "000000".to_string())
                .await
                .map_err(|e| format!("RPPairing over RemoteXPC: {e:?}"))?;

            finish_tunnel(&mut rpc, socket_addr).await
        })
        .await;

        match xpc_res {
            Ok(Ok((adapter, handshake))) => {
                crate::logging::log(format!(
                    "bridge: tunnel up on {ip_str}:{RSD_PORT} via RemoteXPC"
                ));
                set_last_working_route(CachedRoute {
                    host: ip_str.clone(),
                    mode: CachedRouteMode::RsdRemoteXpc,
                });
                return Some(AppDeviceTunnel::Rsd { adapter, handshake });
            }
            Ok(Err(e)) => {
                crate::logging::log(format!(
                    "bridge: RemoteXPC on {ip_str}:{RSD_PORT} failed: {e}"
                ));
                *last_err = format!("{ip_str}:{RSD_PORT}: {e}");
            }
            Err(_) => {
                *last_err = format!("{ip_str}:{RSD_PORT}: RemoteXPC timed out");
                crate::logging::log(format!("bridge: {last_err}"));
            }
        }

        // Path 2: raw RPPairing (JSON) on the same port
        let raw_res = tokio::time::timeout(std::time::Duration::from_millis(2500), async {
            let stream = tokio::net::TcpStream::connect(socket_addr)
                .await
                .map_err(|e| format!("connect: {e}"))?;
            let conn = RpPairingSocket::new(stream);
            let mut rpc = RemotePairingClient::new(conn, HOST_LABEL);
            rpc.connect(&mut rpf, async || "000000".to_string())
                .await
                .map_err(|e| format!("RPPairing: {e:?}"))?;
            finish_tunnel(&mut rpc, socket_addr).await
        })
        .await;

        match raw_res {
            Ok(Ok((adapter, handshake))) => {
                crate::logging::log(format!(
                    "bridge: tunnel up on {ip_str}:{RSD_PORT} via raw RPPairing"
                ));
                set_last_working_route(CachedRoute {
                    host: ip_str.clone(),
                    mode: CachedRouteMode::RsdRawRpPairing,
                });
                return Some(AppDeviceTunnel::Rsd { adapter, handshake });
            }
            Ok(Err(e)) => {
                crate::logging::log(format!(
                    "bridge: raw RPPairing on {ip_str}:{RSD_PORT} failed: {e}"
                ));
                *last_err = format!("{ip_str}:{RSD_PORT}: {e}");
            }
            Err(_) => {
                *last_err = format!("{ip_str}:{RSD_PORT}: raw RPPairing timed out");
                crate::logging::log(format!("bridge: {last_err}"));
            }
        }
    }
    None
}

async fn try_lockdown(
    target_ips: &[String],
    pf: &PairingFile,
    last_err: &mut String,
) -> Option<AppDeviceTunnel> {
    for ip_str in target_ips {
        let Ok(ip) = ip_str.parse::<Ipv4Addr>() else {
            continue;
        };
        if ip.is_loopback() {
            continue;
        }

        let provider = TcpProvider {
            addr: IpAddr::V4(ip),
            scope_id: None,
            pairing_file: pf.clone(),
            label: HOST_LABEL.to_string(),
        };

        match tokio::time::timeout(
            std::time::Duration::from_millis(800),
            LockdownClient::connect(&provider),
        )
        .await
        {
            Ok(Ok(mut lockdown)) => match lockdown.start_session(pf).await {
                Ok(legacy) => {
                    crate::logging::log(format!(
                        "bridge: lockdownd session on {ip_str}:{LOCKDOWN_PORT} (legacy={legacy})"
                    ));
                    set_last_working_route(CachedRoute {
                        host: ip_str.clone(),
                        mode: CachedRouteMode::Lockdown,
                    });
                    return Some(AppDeviceTunnel::Lockdown {
                        provider,
                        pairing_file: pf.clone(),
                        legacy,
                    });
                }
                Err(e) => {
                    *last_err = format!("lockdownd start_session on {ip_str}: {e:?}");
                }
            },
            Ok(Err(e)) => {
                *last_err = format!("lockdownd connect on {ip_str}: {e:?}");
            }
            Err(_) => {
                *last_err = format!("lockdownd connect on {ip_str} timed out");
            }
        }
    }
    None
}

/// Establish a tunnel using a pairing file (RPPairing and/or Lockdown format).
pub async fn connect_tunnel(pairing_bytes: &[u8]) -> Result<AppDeviceTunnel, String> {
    let mut last_error = String::new();
    let target_ips = resolve_target_hosts();

    let parsed_lockdown = PairingFile::from_bytes(pairing_bytes).ok();
    let parsed_rp = RpPairingFile::from_bytes(pairing_bytes).ok();

    if parsed_lockdown.is_none() && parsed_rp.is_none() {
        return Err(
            "pairing file is invalid: carries neither a Lockdown record nor Remote Pairing credentials"
                .to_string(),
        );
    }

    // Fast path: retry the route that worked last time.
    if let Some(cached) = get_last_working_route() {
        if cached.host.parse::<Ipv4Addr>().is_ok() {
            let res = match cached.mode {
                CachedRouteMode::RsdRemoteXpc | CachedRouteMode::RsdRawRpPairing => {
                    match RpPairingFile::from_bytes(pairing_bytes) {
                        Ok(rpf) => try_rsd(&[cached.host.clone()], rpf, &mut last_error).await,
                        Err(_) => None,
                    }
                }
                CachedRouteMode::Lockdown => match &parsed_lockdown {
                    Some(pf) => try_lockdown(&[cached.host.clone()], pf, &mut last_error).await,
                    None => None,
                },
            };
            if let Some(t) = res {
                return Ok(t);
            }
            crate::logging::log("bridge: cached route failed, re-probing...");
            clear_last_working_route();
        }
    }

    // RSD first (iOS 17+ primary), Lockdown as fallback.
    if let Some(rpf) = parsed_rp {
        if let Some(tunnel) = try_rsd(&target_ips, rpf, &mut last_error).await {
            return Ok(tunnel);
        }
        crate::logging::log("bridge: RSD route unavailable, trying Lockdown...");
    }

    if let Some(pf) = &parsed_lockdown {
        if let Some(tunnel) = try_lockdown(&target_ips, pf, &mut last_error).await {
            return Ok(tunnel);
        }
    }

    Err(format!(
        "device connection failed: {last_error}. Ensure a loopback VPN (e.g. LocalDevVPN) is active."
    ))
}

# Device Bridge (iOS 26.1+ / 27)

Since iOS 26.1 removed the MCM identity-trust path and the kernel exploit is
dead on modern releases, 3105 reaches app data containers through a **loopback
VPN device bridge** instead of a sandbox escape.

## Architecture

```
┌──────────── 3105.app ─────────────┐
│ SwiftUI views / PatchTransaction  │
│ DeviceBridge.swift  BridgeStaging │
│ ────────── C FFI ──────────────── │
│ tofs_bridge (Rust staticlib)      │
│  · pairing host (RPPairing)       │
│  · RSD tunnel (RemoteXPC / raw)   │
│  · CoreDevice file service client │
│  · AirTraffic (Book sync) writes  │
└───────────────┬───────────────────┘
                │ TCP over loopback VPN (10.7.0.1)
┌───────────────┴───────────────────┐
│ remotepairingd / lockdownd / atcd │
└───────────────────────────────────┘
```

- **Pairing** — the app advertises `_remotepairing-pairable-host._tcp` over
  the VPN via NetService; the device initiates pair-setup; credentials are
  stored in `Documents/tofs_pairing.plist`.
- **Session** — `tb_open` builds an RSD tunnel (TLS-PSK + CDTunnel + userspace
  TCP stack) and caches the working route for fast reconnects.
- **Reads / listings** — CoreDevice file service
  (`com.apple.coredevice.fileservice.control`), sessions scoped to an app's
  data container (domain 1). Supports list, attributes, read, write, mkdir,
  remove, rename.
- **Writes** — file service first; automatic fallback to AirTraffic Book-sync
  writes (streaming_zip_conduit staging + `com.apple.atc` FileComplete), which
  can also target absolute paths outside containers.
- **Patch flow** — when direct container resolution fails, `DevicePatchService`
  mirrors the patch targets through the bridge into a deterministic staging
  directory, runs the unmodified `PatchTransaction` engine on the mirror, then
  diffs and pushes changes back, byte-verifying every written file. Journal
  fingerprints stay stable because the staging path is deterministic.

## First-run (on device)

1. Install **LocalDevVPN** (App Store id6755608044) and enable its VPN.
2. 3105 → Settings → **Device Bridge** → *Pair with this iPhone*; keep the
   screen open and accept the pairing prompt on the device.
3. *Connect tunnel*, then *Run bridge self test* (ATC canary write into 3105's
   own container, read back through the file service, byte-compared).

After that, patching and the Files tab work transparently. If the VPN drops,
operations fail and the next attempt re-opens the tunnel automatically.

## Building

The Rust bridge is built by CI (`.github/workflows/build-ipa.yml`) for
`aarch64-apple-ios` with `IPHONEOS_DEPLOYMENT_TARGET=16.0`, producing
`Bridge/rust/target/aarch64-apple-ios/release/libtofs_bridge.a`, which the
Xcode target links via the Frameworks phase.

Local type-check on non-Apple hosts (uses the `ring` TLS backend instead of
`aws-lc`):

```
cd Bridge/rust
cargo check --target x86_64-pc-windows-msvc --no-default-features --features ring
```

## Credits

Bridge implementation adapted from
[AirCard-iOS](https://github.com/Mak5er/AirCard-iOS) (MIT) on top of
[jkcoxson/idevice](https://github.com/jkcoxson/idevice). Protocol reference:
pymobiledevice3 (`remote/core_device/file_service.py`).

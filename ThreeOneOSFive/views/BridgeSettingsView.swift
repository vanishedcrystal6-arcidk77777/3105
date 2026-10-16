import SwiftUI

// MARK: - Device bridge settings (LocalDevVPN pairing + tunnel status)
//
// Drives the first-run flow for the iOS 26.1+/27 data path:
//   1. Install + enable LocalDevVPN (loopback VPN to this device).
//   2. Pair: the app advertises a pairable host over the VPN; accept the
//      prompt on the device.
//   3. Connect: open the RSD tunnel. Patch + Files then work through it.
//
// No polling timer: a repeating timer rebuilds the Form mid-gesture and
// eats taps. State refreshes on appear, on scene activation (returning
// from LocalDevVPN), on bridge state changes, and after every action.

struct BridgeSettingsSection: View {
    @Environment(\.scenePhase) private var scenePhase
    @State private var vpnActive = false
    @State private var gateway: String?
    @State private var paired = false
    @State private var connected = false
    @State private var pairing = false
    @State private var busy = false
    @State private var statusMessage: String?
    @State private var pin: String?
    @State private var pairingAttempt = 0

    var body: some View {
        Section {
            LabeledContent("Loopback VPN", value: vpnActive ? "Active (\(gateway ?? "?"))" : "Not detected")
            LabeledContent("Pairing", value: paired ? "Paired" : "Not paired")
            LabeledContent("Tunnel", value: connected ? "Connected" : (pairing ? "Pairing…" : "Closed"))

            if let pin {
                Text("Device PIN: \(pin)")
                    .font(.headline.monospaced())
            }

            if !vpnActive {
                Text("LocalDevVPN is not active. Open LocalDevVPN, turn the VPN on, then come back — this page refreshes when you return.")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }

            if let statusMessage {
                Text(statusMessage)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Link(destination: URL(string: "https://apps.apple.com/app/id6755608044")!) {
                Label("Get LocalDevVPN (App Store)", systemImage: "arrow.down.app")
            }

            if pairing {
                Button(role: .cancel) {
                    cancelPairing()
                } label: {
                    Label("Cancel pairing", systemImage: "xmark.circle")
                }
            } else {
                Button {
                    startPairing()
                } label: {
                    Label("Pair with this iPhone", systemImage: "link.badge.plus")
                }
                .disabled(busy)
            }

            Button {
                connectTunnel()
            } label: {
                Label("Connect tunnel", systemImage: "point.3.connected.trianglepath.dotted")
            }
            .disabled(!paired || connected || busy || pairing)

            Button {
                runSelfTest()
            } label: {
                Label("Run bridge self test", systemImage: "checkmark.shield")
            }
            .disabled(!connected || busy || pairing)

            if connected {
                Button(role: .destructive) {
                    DeviceBridge.shared.disconnect()
                    refresh()
                } label: {
                    Label("Disconnect", systemImage: "xmark.circle")
                }
            }

            if paired {
                Button(role: .destructive) {
                    DeviceBridge.shared.resetPairing()
                    refresh()
                } label: {
                    Label("Forget pairing", systemImage: "trash")
                }
            }

            Button {
                refresh()
            } label: {
                Label("Refresh status", systemImage: "arrow.clockwise")
            }
        } header: {
            Text("Device Bridge")
        } footer: {
            Text("Required on iOS 26.1+ to reach app data. Order matters: 1) turn LocalDevVPN on, 2) pair, 3) connect, 4) run the self test.")
        }
        .onAppear {
            DeviceBridge.shared.onStateChange = { _ in
                Task { @MainActor in refresh() }
            }
            DeviceBridge.shared.onPairingPIN = { value in
                Task { @MainActor in pin = value }
            }
            refresh()
        }
        .onDisappear {
            DeviceBridge.shared.onStateChange = nil
            DeviceBridge.shared.onPairingPIN = nil
        }
        .onChange(of: scenePhase) { phase in
            if phase == .active { refresh() }
        }
    }

    /// Write state only when a value actually changed, so the Form never
    /// rebuilds under an in-flight touch.
    private func refresh() {
        let addrs = DeviceBridge.shared.detectTunnelAddresses()
        if vpnActive != addrs.vpnActive { vpnActive = addrs.vpnActive }
        if gateway != addrs.gateway { gateway = addrs.gateway }
        let isPaired = DeviceBridge.shared.hasPairingFile
        if paired != isPaired { paired = isPaired }
        let isConnected = DeviceBridge.shared.isConnected
        if connected != isConnected { connected = isConnected }
    }

    private func startPairing() {
        busy = true
        pairing = true
        pairingAttempt += 1
        let attempt = pairingAttempt
        statusMessage = "Waiting for the device… keep this screen open and accept the pairing prompt."
        DeviceBridge.shared.startPairing { result in
            guard attempt == pairingAttempt else { return }
            busy = false
            pairing = false
            switch result {
            case .success:
                statusMessage = "Paired. Now tap “Connect tunnel”."
                pin = nil
            case .failure(let error):
                statusMessage = error.localizedDescription
            }
            refresh()
        }
    }

    private func cancelPairing() {
        pairingAttempt += 1
        busy = false
        pairing = false
        pin = nil
        statusMessage = "Pairing cancelled."
        DeviceBridge.shared.abandonPairing()
        refresh()
    }

    private func connectTunnel() {
        busy = true
        statusMessage = nil
        DeviceBridge.shared.connect { result in
            busy = false
            if case .failure(let error) = result {
                statusMessage = error.localizedDescription
            }
            refresh()
        }
    }

    private func runSelfTest() {
        busy = true
        statusMessage = "Running self test…"
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                let report = try DeviceBridge.shared.selfTest()
                DispatchQueue.main.async {
                    busy = false
                    statusMessage = report.contains("\"ok\": true") || report.contains("\"ok\":true")
                        ? "Self test passed — writes verified byte-for-byte."
                        : "Self test report: \(report)"
                }
            } catch {
                DispatchQueue.main.async {
                    busy = false
                    statusMessage = error.localizedDescription
                }
            }
        }
    }
}

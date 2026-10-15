import SwiftUI

// MARK: - Device bridge settings (LocalDevVPN pairing + tunnel status)
//
// Drives the first-run flow for the iOS 26.1+/27 data path:
//   1. Install + enable LocalDevVPN (loopback VPN to this device).
//   2. Pair: the app advertises a pairable host over the VPN; accept the
//      prompt on the device.
//   3. Connect: open the RSD tunnel. Patch + Files then work through it.

struct BridgeSettingsSection: View {
    @State private var vpnActive = false
    @State private var gateway: String?
    @State private var paired = false
    @State private var connected = false
    @State private var pairing = false
    @State private var busy = false
    @State private var statusMessage: String?
    @State private var pin: String?

    var body: some View {
        Section {
            LabeledContent("Loopback VPN", value: vpnActive ? "Active (\(gateway ?? "?"))" : "Not detected")
            LabeledContent("Pairing", value: paired ? "Paired" : "Not paired")
            LabeledContent("Tunnel", value: connected ? "Connected" : (pairing ? "Pairing…" : "Closed"))

            if let pin {
                Text("Device PIN: \(pin)")
                    .font(.headline.monospaced())
            }

            if let statusMessage {
                Text(statusMessage)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Link(destination: URL(string: "https://apps.apple.com/app/id6755608044")!) {
                Label("Get LocalDevVPN (App Store)", systemImage: "arrow.down.app")
            }

            Button {
                startPairing()
            } label: {
                Label("Pair with this iPhone", systemImage: "link.badge.plus")
            }
            .disabled(!vpnActive || busy || pairing)

            Button {
                connectTunnel()
            } label: {
                Label("Connect tunnel", systemImage: "point.3.connected.trianglepath.dotted")
            }
            .disabled(!paired || connected || busy)

            Button {
                runSelfTest()
            } label: {
                Label("Run bridge self test", systemImage: "checkmark.shield")
            }
            .disabled(!connected || busy)

            if connected {
                Button(role: .destructive) {
                    DeviceBridge.shared.disconnect()
                    refresh()
                } label: {
                    Label("Disconnect", systemImage: "xmark.circle")
                }
            }
        } header: {
            Text("Device Bridge")
        } footer: {
            Text("Required on iOS 26.1+ to reach app data. Enable LocalDevVPN, keep this screen open, pair, then connect.")
        }
        .onAppear {
            DeviceBridge.shared.onStateChange = { _ in refresh() }
            DeviceBridge.shared.onPairingPIN = { value in pin = value }
            refresh()
        }
        .onDisappear {
            DeviceBridge.shared.onStateChange = nil
            DeviceBridge.shared.onPairingPIN = nil
        }
    }

    private func refresh() {
        let addrs = DeviceBridge.shared.detectTunnelAddresses()
        vpnActive = addrs.vpnActive
        gateway = addrs.gateway
        paired = DeviceBridge.shared.hasPairingFile
        connected = DeviceBridge.shared.isConnected
        if !connected { pairing = false }
    }

    private func startPairing() {
        busy = true
        pairing = true
        statusMessage = "Waiting for the device… accept the pairing prompt."
        DeviceBridge.shared.startPairing { result in
            busy = false
            pairing = false
            switch result {
            case .success:
                statusMessage = "Paired. Now connect the tunnel."
                pin = nil
            case .failure(let error):
                statusMessage = error.localizedDescription
            }
            refresh()
        }
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

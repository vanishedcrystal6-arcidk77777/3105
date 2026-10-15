import Darwin
import Foundation

// MARK: - Device bridge (LocalDevVPN + RSD tunnel via tofs_bridge Rust core)
//
// Provides the iOS 26.1+/27 data path for patching and the Files tab:
//   pairing  — device-initiated RPPairing over the loopback VPN (Bonjour
//              advertisement via NetService, listener in Rust).
//   session  — RSD tunnel (tb_open/tb_close).
//   io       — CoreDevice file service (container-relative read/write/list)
//              with AirTraffic (Book sync) writes as the fallback for paths
//              the file service refuses.

final class DeviceBridge: NSObject {
    static let shared = DeviceBridge()

    enum BridgeError: Error, LocalizedError {
        case notConnected
        case vpnMissing
        case alreadyPairing
        case bridge(String)

        var errorDescription: String? {
            switch self {
            case .notConnected: return "Device bridge is not connected."
            case .vpnMissing: return "Loopback VPN (LocalDevVPN) is not active."
            case .alreadyPairing: return "Pairing is already in progress."
            case .bridge(let message): return message
            }
        }
    }

    enum State: Equatable {
        case disconnected
        case pairing
        case connected
    }

    struct BridgedApp: Identifiable, Hashable {
        let bundleID: String
        let name: String
        let container: String?
        var id: String { bundleID }
    }

    struct BridgedFileInfo {
        let name: String
        let isDirectory: Bool
        let size: Int64
        let modifiedAt: Date?
    }

    // MARK: State

    private let opQueue = DispatchQueue(label: "one3105.bridge.ops", qos: .userInitiated)
    private let opQueueKey = DispatchSpecificKey<Bool>()

    private var sessionHandle: UInt32 = 0
    private var containerCache: [String: String] = [:]
    private var pairingThreadActive = false
    private var publishedService: NetService?

    private(set) var state: State = .disconnected {
        didSet {
            let value = state
            DispatchQueue.main.async { [weak self] in
                self?.onStateChange?(value)
            }
        }
    }
    var onStateChange: ((State) -> Void)?
    var onPairingPIN: ((String) -> Void)?

    var isConnected: Bool {
        syncOp { sessionHandle != 0 }
    }

    private static var didInitLogging = false

    private override init() {
        super.init()
        opQueue.setSpecific(key: opQueueKey, value: true)
        if !Self.didInitLogging {
            Self.didInitLogging = true
            tb_log_init({ _, msg in
                guard let msg else { return }
                log(String(cString: msg))
            }, nil)
        }
    }

    // MARK: Serialisation helper

    @discardableResult
    private func syncOp<T>(_ work: () throws -> T) rethrows -> T {
        if DispatchQueue.getSpecific(key: opQueueKey) == true {
            return try work()
        }
        return try opQueue.sync(execute: work)
    }

    // MARK: Persistence

    private var pairingFileURL: URL {
        let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        return docs.appendingPathComponent("tofs_pairing.plist")
    }

    var hasPairingFile: Bool {
        FileManager.default.fileExists(atPath: pairingFileURL.path)
    }

    private var altIRK: String? {
        get { UserDefaults.standard.string(forKey: "tofs.altIRK") }
        set { UserDefaults.standard.set(newValue, forKey: "tofs.altIRK") }
    }

    // MARK: VPN / interface detection

    struct TunnelAddresses {
        let vpnActive: Bool
        let gateway: String?
        let local: [String]
    }

    func detectTunnelAddresses() -> TunnelAddresses {
        var iflist: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&iflist) == 0, let first = iflist else {
            return TunnelAddresses(vpnActive: false, gateway: nil, local: [])
        }
        defer { freeifaddrs(iflist) }

        var vpnLocal: [String] = []
        var otherLocal: [String] = []
        var cursor: UnsafeMutablePointer<ifaddrs>? = first
        while let ifa = cursor?.pointee {
            let isUp = (ifa.ifa_flags & UInt32(IFF_UP)) != 0
            if isUp, let addr = ifa.ifa_addr, addr.pointee.sa_family == UInt8(AF_INET) {
                var sin = addr.withMemoryRebound(to: sockaddr_in.self, capacity: 1) { $0.pointee }
                var buf = [CChar](repeating: 0, count: Int(INET_ADDRSTRLEN))
                if inet_ntop(AF_INET, &sin.sin_addr, &buf, socklen_t(INET_ADDRSTRLEN)) != nil {
                    let ip = String(cString: buf)
                    let name = String(cString: ifa.ifa_name)
                    if name.hasPrefix("utun"), ip.hasPrefix("10.7.0.") {
                        vpnLocal.append(ip)
                    } else if !ip.hasPrefix("127."), !ip.hasPrefix("169.254."), !name.hasPrefix("utun") {
                        otherLocal.append(ip)
                    }
                }
            }
            cursor = ifa.ifa_next
        }

        if let clientIP = vpnLocal.first {
            var parts = clientIP.split(separator: ".").map(String.init)
            if parts.count == 4 {
                parts[3] = "1"
                return TunnelAddresses(vpnActive: true, gateway: parts.joined(separator: "."), local: vpnLocal + otherLocal)
            }
        }
        return TunnelAddresses(vpnActive: false, gateway: nil, local: otherLocal)
    }

    private func configureTargets() throws {
        let addrs = detectTunnelAddresses()
        guard addrs.vpnActive, let gateway = addrs.gateway else {
            throw BridgeError.vpnMissing
        }
        tb_set_target_host(gateway)
        if !addrs.local.isEmpty {
            var cStrings = addrs.local.map { strdup($0) }
            defer { for s in cStrings { free(s) } }
            cStrings.withUnsafeBufferPointer { buf in
                _ = tb_set_target_hosts(
                    UnsafeRawPointer(buf.baseAddress)?
                        .assumingMemoryBound(to: UnsafePointer<CChar>?.self),
                    buf.count
                )
            }
        }
    }

    // MARK: Pairing

    /// Start device-initiated pairing. The device must see the Bonjour
    /// advertisement and the user must accept the prompt on the device.
    /// Blocks in the background until paired; completion fires on main.
    func startPairing(completion: @escaping (Result<Void, Error>) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async {
            do {
                try self.configureTargets()
            } catch {
                DispatchQueue.main.async { completion(.failure(error)) }
                return
            }
            let already = self.syncOp { self.pairingThreadActive }
            if already {
                DispatchQueue.main.async { completion(.failure(BridgeError.alreadyPairing)) }
                return
            }
            self.syncOp { self.pairingThreadActive = true }
            self.state = .pairing

            let path = self.pairingFileURL.path
            let savedIRK = self.altIRK

            var result = TBPairResult()
            let rc = tb_pairing_run_host(
                "0.0.0.0",
                0,
                "3105",
                "Mac17,7",
                path,
                savedIRK,
                { ctx, serviceID, port, keys, vals, count in
                    guard let ctx else { return }
                    let bridge = Unmanaged<DeviceBridge>.fromOpaque(ctx).takeUnretainedValue()
                    bridge.publishPairingService(
                        serviceID: serviceID.map { String(cString: $0) } ?? "3105",
                        port: port,
                        keys: keys,
                        vals: vals,
                        count: count
                    )
                },
                { pin, ctx in
                    guard let ctx, let pin else { return }
                    let bridge = Unmanaged<DeviceBridge>.fromOpaque(ctx).takeUnretainedValue()
                    let pinString = String(cString: pin)
                    DispatchQueue.main.async { bridge.onPairingPIN?(pinString) }
                },
                Unmanaged.passUnretained(self).toOpaque(),
                &result
            )

            self.stopPublishingPairingService()
            self.syncOp { self.pairingThreadActive = false }

            if rc == 0 {
                if let irk = result.host_alt_irk_hex {
                    self.altIRK = String(cString: irk)
                }
                let device = result.device_name.map { String(cString: $0) } ?? "device"
                log("bridge: paired with \(device)")
                tb_pairing_result_free(&result)
                DispatchQueue.main.async { completion(.success(())) }
            } else {
                let message = result.error.map { String(cString: $0) } ?? "pairing failed"
                tb_pairing_result_free(&result)
                self.state = .disconnected
                DispatchQueue.main.async { completion(.failure(BridgeError.bridge(message))) }
            }
        }
    }

    private func publishPairingService(
        serviceID: String,
        port: UInt16,
        keys: UnsafePointer<UnsafePointer<CChar>?>?,
        vals: UnsafePointer<UnsafePointer<CChar>?>?,
        count: Int
    ) {
        var txt: [String: Data] = [:]
        if let keys, let vals {
            for i in 0..<count {
                guard let k = keys[i], let v = vals[i] else { continue }
                txt[String(cString: k)] = String(cString: v).data(using: .utf8)
            }
        }
        DispatchQueue.main.async {
            self.stopPublishingPairingService()
            let service = NetService(
                domain: "local.",
                type: "_remotepairing-pairable-host._tcp",
                name: serviceID,
                port: Int32(port)
            )
            service.setTXTRecord(NetService.data(fromTXTRecord: txt))
            service.publish()
            self.publishedService = service
            log("bridge: advertising pairable host '\(serviceID)' on port \(port)")
        }
    }

    private func stopPublishingPairingService() {
        let stop = {
            self.publishedService?.stop()
            self.publishedService = nil
        }
        if Thread.isMainThread { stop() } else { DispatchQueue.main.sync(execute: stop) }
    }

    // MARK: Session

    /// Open the tunnel session if a pairing file exists and the VPN is up.
    func connect(completion: @escaping (Result<Void, Error>) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Result<Void, Error> {
                guard self.hasPairingFile else {
                    throw BridgeError.bridge("Not paired yet.")
                }
                try self.configureTargets()
                try self.openSession()
            }
            DispatchQueue.main.async { completion(result) }
        }
    }

    /// Blocking; call off the main thread.
    private func openSession() throws {
        try syncOp {
            if sessionHandle != 0 { return }
            var handle: UInt32 = 0
            var error: UnsafeMutablePointer<CChar>?
            let rc = tb_open(pairingFileURL.path, &handle, &error)
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "tb_open failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
            sessionHandle = handle
            state = .connected
            log("bridge: tunnel session open (handle \(handle))")
        }
    }

    func disconnect() {
        syncOp {
            if sessionHandle != 0 {
                _ = tb_close(sessionHandle)
                sessionHandle = 0
            }
            containerCache.removeAll()
            state = .disconnected
        }
    }

    /// Drop a broken session and reopen once. Throws if reconnect fails.
    private func reconnect() throws {
        try syncOp {
            if sessionHandle != 0 {
                _ = tb_close(sessionHandle)
                sessionHandle = 0
            }
            containerCache.removeAll()
        }
        try openSession()
    }

    // MARK: Info

    /// Absolute container path for a bundle ID, via installation_proxy.
    /// Blocking; call off the main thread.
    func findContainer(bundleID: String) -> String? {
        do {
            return try performWithReconnect { handle in
                if let cached = containerCache[bundleID] { return cached }
                var out: UnsafeMutablePointer<CChar>?
                var error: UnsafeMutablePointer<CChar>?
                let rc = tb_find_container(handle, bundleID, &out, &error)
                if rc != 0 {
                    let message = error.map { String(cString: $0) } ?? "lookup failed"
                    if let error { tb_string_free(error) }
                    throw BridgeError.bridge(message)
                }
                guard let out else { throw BridgeError.bridge("no container returned") }
                let path = String(cString: out)
                tb_string_free(out)
                containerCache[bundleID] = path
                return path
            }
        } catch {
            return nil
        }
    }

    /// All user apps visible through installation_proxy.
    /// Blocking; call off the main thread.
    func listApps() throws -> [BridgedApp] {
        try performWithReconnect { handle in
            var json: UnsafeMutablePointer<CChar>?
            var error: UnsafeMutablePointer<CChar>?
            let rc = tb_list_apps_json(handle, &json, &error)
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "list failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
            guard let json else { return [] }
            defer { tb_string_free(json) }
            let data = Data(String(cString: json).utf8)
            let raw = try JSONSerialization.jsonObject(with: data) as? [[String: Any]] ?? []
            return raw.compactMap { entry in
                guard let bundleID = entry["bundleID"] as? String else { return nil }
                let name = entry["name"] as? String ?? bundleID
                let container = entry["container"] as? String
                if let container { containerCache[bundleID] = container }
                return BridgedApp(bundleID: bundleID, name: name, container: container)
            }
        }
    }

    // MARK: File service IO (container-relative). Blocking; off-main only.

    func fsList(bundleID: String, path: String) throws -> [BridgedFileInfo] {
        try performWithReconnect { handle in
            var json: UnsafeMutablePointer<CChar>?
            var error: UnsafeMutablePointer<CChar>?
            let rc = tb_fs_list_json(handle, bundleID, path, &json, &error)
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "list failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
            guard let json else { return [] }
            defer { tb_string_free(json) }
            let data = Data(String(cString: json).utf8)
            let raw = try JSONSerialization.jsonObject(with: data) as? [[String: Any]] ?? []
            return raw.map { entry in
                let name = entry["name"] as? String ?? ""
                let attrs = entry["attrs"] as? [String: Any]
                let fileType = attrs?["fileType"] as? String ?? ""
                let size = (attrs?["size"] as? NSNumber)?.int64Value ?? (fileType == "directory" ? -1 : 0)
                var modified: Date?
                if let mtime = (attrs?["modificationTime"] as? NSNumber)?.doubleValue {
                    modified = Date(timeIntervalSinceReferenceDate: mtime)
                }
                return BridgedFileInfo(
                    name: name,
                    isDirectory: fileType == "directory",
                    size: size,
                    modifiedAt: modified
                )
            }
        }
    }

    func fsRead(bundleID: String, path: String) throws -> Data {
        try performWithReconnect { handle in
            var buffer: UnsafeMutablePointer<UInt8>?
            var length = 0
            var error: UnsafeMutablePointer<CChar>?
            let rc = tb_fs_read(handle, bundleID, path, &buffer, &length, &error)
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "read failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
            guard let buffer else { return Data() }
            defer { tb_buf_free(buffer, length) }
            return Data(bytes: buffer, count: length)
        }
    }

    func fsWrite(bundleID: String, path: String, data: Data) throws {
        try performWithReconnect { handle in
            var error: UnsafeMutablePointer<CChar>?
            let rc = data.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) -> Int32 in
                tb_fs_write(
                    handle,
                    bundleID,
                    path,
                    bytes.bindMemory(to: UInt8.self).baseAddress,
                    bytes.count,
                    &error
                )
            }
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "write failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
        }
    }

    func fsMkdir(bundleID: String, path: String) throws {
        try performWithReconnect { handle in
            var error: UnsafeMutablePointer<CChar>?
            let rc = tb_fs_mkdir(handle, bundleID, path, &error)
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "mkdir failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
        }
    }

    func fsRemove(bundleID: String, path: String) throws {
        try performWithReconnect { handle in
            var error: UnsafeMutablePointer<CChar>?
            let rc = tb_fs_remove(handle, bundleID, path, &error)
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "remove failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
        }
    }

    func fsRename(bundleID: String, from oldPath: String, to newPath: String) throws {
        try performWithReconnect { handle in
            var error: UnsafeMutablePointer<CChar>?
            let rc = tb_fs_rename(handle, bundleID, oldPath, newPath, &error)
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "rename failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
        }
    }

    /// Attributes of a container-relative path; nil when it does not exist.
    func fsAttributes(bundleID: String, path: String) -> BridgedFileInfo? {
        do {
            let list = try fsList(
                bundleID: bundleID,
                path: (path as NSString).deletingLastPathComponent
            )
            return list.first { $0.name == (path as NSString).lastPathComponent }
        } catch {
            return nil
        }
    }

    // MARK: ATC arbitrary-path writes. Blocking; off-main only.

    func atcWriteFile(absolutePath: String, data: Data) throws {
        try performWithReconnect { handle in
            var error: UnsafeMutablePointer<CChar>?
            let rc = data.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) -> Int32 in
                tb_atc_write_file(
                    handle,
                    absolutePath,
                    bytes.bindMemory(to: UInt8.self).baseAddress,
                    bytes.count,
                    &error
                )
            }
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "ATC write failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
        }
    }

    func atcWriteBatch(targetDirectory: String, files: [(name: String, data: Data)]) throws {
        guard !files.isEmpty else { return }
        try performWithReconnect { handle in
            var cNames: [UnsafeMutablePointer<CChar>?] = files.map { strdup($0.name) }
            var buffers: [UnsafeMutablePointer<UInt8>?] = files.map { file in
                let p = UnsafeMutablePointer<UInt8>.allocate(capacity: max(file.data.count, 1))
                file.data.copyBytes(to: p, count: file.data.count)
                return p
            }
            var lengths: [Int] = files.map { $0.data.count }
            defer {
                for s in cNames { free(s) }
                for (i, p) in buffers.enumerated() { p?.deallocate(capacity: max(lengths[i], 1)) }
            }
            var error: UnsafeMutablePointer<CChar>?
            let rc = cNames.withUnsafeBufferPointer { namesPtr in
                buffers.withUnsafeBufferPointer { datasPtr in
                    lengths.withUnsafeBufferPointer { lensPtr in
                        tb_atc_write_batch(
                            handle,
                            targetDirectory,
                            UnsafeRawPointer(namesPtr.baseAddress)?
                                .assumingMemoryBound(to: UnsafePointer<CChar>?.self),
                            UnsafeRawPointer(datasPtr.baseAddress)?
                                .assumingMemoryBound(to: UnsafePointer<UInt8>?.self),
                            lensPtr.baseAddress,
                            files.count,
                            &error
                        )
                    }
                }
            }
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "ATC batch write failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
        }
    }

    // MARK: Self test

    /// Canary write+readback through ATC and the file service.
    /// Blocking; off-main only. Returns the JSON report.
    func selfTest() throws -> String {
        try performWithReconnect { handle in
            let bundleID = Bundle.main.bundleIdentifier ?? "com.apple.mobile.MobileHouseArrest"
            var json: UnsafeMutablePointer<CChar>?
            var error: UnsafeMutablePointer<CChar>?
            let rc = tb_self_test_json(handle, bundleID, &json, &error)
            if rc != 0 {
                let message = error.map { String(cString: $0) } ?? "self test failed"
                if let error { tb_string_free(error) }
                throw BridgeError.bridge(message)
            }
            guard let json else { return "{}" }
            defer { tb_string_free(json) }
            return String(cString: json)
        }
    }

    // MARK: Reconnect wrapper

    private func performWithReconnect<T>(_ work: (UInt32) throws -> T) throws -> T {
        try openSessionIfNeeded()
        let handle = syncOp { sessionHandle }
        do {
            return try syncOp { try work(handle) }
        } catch {
            // One reconnect attempt, then rethrow.
            try reconnect()
            let handle2 = syncOp { sessionHandle }
            return try syncOp { try work(handle2) }
        }
    }

    private func openSessionIfNeeded() throws {
        let connected = syncOp { sessionHandle != 0 }
        if !connected {
            try openSession()
        }
    }
}

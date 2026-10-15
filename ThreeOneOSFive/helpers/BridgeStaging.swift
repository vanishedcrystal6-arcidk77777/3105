import CryptoKit
import Foundation

// MARK: - Bridge staging
//
// On iOS builds where the app cannot reach other apps' containers directly
// (MCM identity trust removed, no sandbox escape), patch operations run
// against a local mirror of the device-side container content:
//
//   1. Pull every patch target through the device bridge into a deterministic
//      per-app staging directory (stable path → stable journal fingerprints).
//   2. Run the unmodified PatchTransaction engine on the mirror.
//   3. Diff the mirror and push the changes back through the bridge,
//      byte-verifying every file that was written.

enum BridgeStaging {
    private struct StagedContainer {
        let bundleID: String
        let deviceContainerPath: String
        let localRoot: URL
    }

    private struct TreeSnapshot {
        var files: [String: Data] = [:] // relPath -> sha256
        var directories: Set<String> = []
    }

    // MARK: Entry point

    static func withStagedContainers<T>(
        bundleIDs: [String],
        files: [(bundleID: String, relativePath: String)],
        directories: [(bundleID: String, relativePath: String)],
        into roots: inout [String: URL],
        operation: ([String: URL]) throws -> T
    ) throws -> T {
        let bridge = DeviceBridge.shared
        guard bridge.hasPairingFile else {
            throw PatchPackageError.targetAppUnavailable(
                bundleIDs.first ?? ""
            )
        }

        // 1. Resolve device containers and rebuild fresh mirrors.
        var staged: [String: StagedContainer] = [:]
        for bundleID in bundleIDs {
            guard let container = bridge.findContainer(bundleID: bundleID) else {
                throw PatchPackageError.targetAppUnavailable(bundleID)
            }
            let local = try stagingRootURL()
                .appendingPathComponent(bundleID, isDirectory: true)
            let fm = FileManager.default
            try? fm.removeItem(at: local)
            try fm.createDirectory(at: local, withIntermediateDirectories: true)
            staged[bundleID] = StagedContainer(
                bundleID: bundleID,
                deviceContainerPath: container,
                localRoot: local
            )
        }

        // 2. Mirror device state into staging.
        for directory in directories {
            guard let container = staged[directory.bundleID] else { continue }
            if bridge.fsAttributes(bundleID: directory.bundleID, path: directory.relativePath)?.isDirectory == true {
                try? FileManager.default.createDirectory(
                    at: container.localRoot.appendingPathComponent(directory.relativePath, isDirectory: true),
                    withIntermediateDirectories: true
                )
            }
        }
        for file in files {
            guard let container = staged[file.bundleID] else { continue }
            let localTarget = container.localRoot.appendingPathComponent(file.relativePath)
            if let attrs = bridge.fsAttributes(bundleID: file.bundleID, path: file.relativePath) {
                if attrs.isDirectory {
                    try? FileManager.default.createDirectory(
                        at: localTarget,
                        withIntermediateDirectories: true
                    )
                } else {
                    let data = try bridge.fsRead(bundleID: file.bundleID, path: file.relativePath)
                    try FileManager.default.createDirectory(
                        at: localTarget.deletingLastPathComponent(),
                        withIntermediateDirectories: true
                    )
                    try data.write(to: localTarget)
                }
            } else {
                // Missing on device: mirror any ancestor directories that do
                // exist so created-directory tracking matches reality.
                var cursor = (file.relativePath as NSString).deletingLastPathComponent
                while !cursor.isEmpty && cursor != "." {
                    if bridge.fsAttributes(bundleID: file.bundleID, path: cursor)?.isDirectory == true {
                        try? FileManager.default.createDirectory(
                            at: container.localRoot.appendingPathComponent(cursor, isDirectory: true),
                            withIntermediateDirectories: true
                        )
                        break
                    }
                    cursor = (cursor as NSString).deletingLastPathComponent
                }
            }
        }

        // 3. Snapshot, run the transaction on the mirror, then push changes.
        var snapshots: [String: TreeSnapshot] = [:]
        for (bundleID, container) in staged {
            snapshots[bundleID] = snapshotTree(root: container.localRoot)
        }

        for (bundleID, container) in staged {
            roots[bundleID] = PatchPathValidator.canonicalFileURL(container.localRoot)
        }
        let result = try operation(roots)

        for (bundleID, container) in staged {
            try pushChanges(
                container: container,
                before: snapshots[bundleID] ?? TreeSnapshot()
            )
        }
        return result
    }

    // MARK: Staging root

    private static func stagingRootURL() throws -> URL {
        let docs = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let root = docs.appendingPathComponent("BridgeStaging", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root
    }

    // MARK: Tree snapshot / diff

    private static func snapshotTree(root: URL) -> TreeSnapshot {
        var snapshot = TreeSnapshot()
        let fm = FileManager.default
        guard let enumerator = fm.enumerator(
            at: root,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: []
        ) else { return snapshot }
        for case let url as URL in enumerator {
            let rel = String(url.path.dropFirst(root.path.count + 1))
            guard !rel.isEmpty else { continue }
            let isDir = (try? url.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory == true
            if isDir {
                snapshot.directories.insert(rel)
            } else if let data = try? Data(contentsOf: url) {
                snapshot.files[rel] = Data(SHA256.hash(data: data))
            }
        }
        return snapshot
    }

    private static func pushChanges(container: StagedContainer, before: TreeSnapshot) throws {
        let bridge = DeviceBridge.shared
        let after = snapshotTree(root: container.localRoot)

        // New directories (parents first).
        let newDirs = after.directories.subtracting(before.directories)
            .sorted { $0.filter { $0 == "/" }.count < $1.filter { $0 == "/" }.count }
        for dir in newDirs {
            try? bridge.fsMkdir(bundleID: container.bundleID, path: dir)
        }

        // New / changed files.
        var pushed = 0
        for (rel, digest) in after.files where before.files[rel] != digest {
            let localURL = container.localRoot.appendingPathComponent(rel)
            let data = try Data(contentsOf: localURL)
            try pushFile(container: container, relativePath: rel, data: data)
            pushed += 1
        }

        // Removed files.
        for rel in before.files.keys where after.files[rel] == nil {
            try? bridge.fsRemove(bundleID: container.bundleID, path: rel)
        }

        // Removed directories (deepest first).
        let removedDirs = before.directories.subtracting(after.directories)
            .sorted { $0.filter { $0 == "/" }.count > $1.filter { $0 == "/" }.count }
        for dir in removedDirs {
            try? bridge.fsRemove(bundleID: container.bundleID, path: dir)
        }

        if pushed > 0 {
            log("bridge: pushed \(pushed) file(s) to \(container.bundleID)")
        }
    }

    private static func pushFile(
        container: StagedContainer,
        relativePath: String,
        data: Data
    ) throws {
        let bridge = DeviceBridge.shared
        do {
            try bridge.fsWrite(bundleID: container.bundleID, path: relativePath, data: data)
        } catch {
            // Fall back to the AirTraffic absolute-path write.
            let absolute = container.deviceContainerPath + "/" + relativePath
            try bridge.atcWriteFile(absolutePath: absolute, data: data)
        }
        // Byte-verify through the file service.
        let readback = try bridge.fsRead(bundleID: container.bundleID, path: relativePath)
        guard readback == data else {
            throw PatchPackageError.applyFailed
        }
    }
}

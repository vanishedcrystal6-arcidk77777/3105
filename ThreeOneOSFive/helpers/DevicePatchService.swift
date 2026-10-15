import Foundation

enum DevicePatchService {
    static func apply(project: PatchProject) throws -> PatchTransactionReceipt {
        let bundleIDs = orderedBundleIdentifiers(in: project)
        let files = project.rules.map { (bundleID: $0.bundleID, relativePath: $0.relativePath) }
        let directories = project.directories.map { (bundleID: $0.bundleID, relativePath: $0.relativePath) }
        return try withResolvedContainers(bundleIDs: bundleIDs, files: files, directories: directories) { roots in
            try PatchTransaction.apply(
                project: project,
                backupRoot: try PatchProjectLibrary.backupRootURL(),
                containerResolver: { bundleID in
                    guard let root = roots[bundleID] else {
                        throw PatchPackageError.targetAppUnavailable(bundleID)
                    }
                    return root
                }
            )
        }
    }

    static func inspectRestore(receipt: PatchTransactionReceipt) throws -> PatchRestoreInspection {
        let bundleIDs = try PatchTransaction.requiredBundleIdentifiers(for: receipt)
        let paths = try PatchTransaction.requiredPaths(for: receipt)
        return try withResolvedContainers(
            bundleIDs: bundleIDs,
            files: paths.files,
            directories: paths.directories
        ) { roots in
            try PatchTransaction.inspectRestore(
                receipt: receipt,
                containerResolver: { bundleID in
                    guard let root = roots[bundleID] else {
                        throw PatchPackageError.targetAppUnavailable(bundleID)
                    }
                    return root
                }
            )
        }
    }

    static func restore(
        receipt: PatchTransactionReceipt,
        allowChangedTargets: Bool = false
    ) throws {
        let bundleIDs = try PatchTransaction.requiredBundleIdentifiers(for: receipt)
        let paths = try PatchTransaction.requiredPaths(for: receipt)
        try withResolvedContainers(
            bundleIDs: bundleIDs,
            files: paths.files,
            directories: paths.directories
        ) { roots in
            try PatchTransaction.restore(
                receipt: receipt,
                allowChangedTargets: allowChangedTargets,
                containerResolver: { bundleID in
                    guard let root = roots[bundleID] else {
                        throw PatchPackageError.targetAppUnavailable(bundleID)
                    }
                    return root
                }
            )
        }
    }

    static func resetToAppliedState(
        receipt: PatchTransactionReceipt,
        project: PatchProject
    ) throws {
        let bundleIDs = try PatchTransaction.requiredBundleIdentifiers(for: receipt)
        let paths = try PatchTransaction.requiredPaths(for: receipt)
        try withResolvedContainers(
            bundleIDs: bundleIDs,
            files: paths.files,
            directories: paths.directories
        ) { roots in
            try PatchTransaction.resetToAppliedState(
                receipt: receipt,
                fallbackProject: project,
                containerResolver: { bundleID in
                    guard let root = roots[bundleID] else {
                        throw PatchPackageError.targetAppUnavailable(bundleID)
                    }
                    return root
                }
            )
        }
    }

    static func latestReceipt(projectID: UUID) -> PatchTransactionReceipt? {
        guard let backupRoot = try? PatchProjectLibrary.backupRootURL() else { return nil }
        return PatchTransaction.latestReceipt(projectID: projectID, backupRoot: backupRoot)
    }

    private static func orderedBundleIdentifiers(in project: PatchProject) -> [String] {
        project.allBundleIdentifiers
    }

    private static func withResolvedContainers<T>(
        bundleIDs: [String],
        files: [(bundleID: String, relativePath: String)],
        directories: [(bundleID: String, relativePath: String)],
        operation: ([String: URL]) throws -> T
    ) throws -> T {
        var roots: [String: URL] = [:]
        var unresolved: [String] = []

        for bundleID in bundleIDs {
            if let path = ContainerStore.resolveAppContainerPath(bundleID: bundleID),
               ContainerStore.isApplicationContainerPath(path) {
                roots[bundleID] = PatchPathValidator.canonicalFileURL(URL(fileURLWithPath: path, isDirectory: true))
            } else {
                unresolved.append(bundleID)
            }
        }
        if unresolved.isEmpty {
            return try operation(roots)
        }

        // Direct container access failed (stock iOS 26.1+): mirror the device
        // state through the LocalDevVPN bridge, run the transaction on the
        // mirror, and push the changes back.
        let unresolvedSet = Set(unresolved)
        return try BridgeStaging.withStagedContainers(
            bundleIDs: unresolved,
            files: files.filter { unresolvedSet.contains($0.bundleID) },
            directories: directories.filter { unresolvedSet.contains($0.bundleID) },
            into: &roots,
            operation: operation
        )
    }
}

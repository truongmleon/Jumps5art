// Stages all of an asset's PhotoKit resources together in the private app
// container. The manifest is a local handoff, with no server backup claim.

import Foundation

/// The resource set that must be available before an asset can be staged.
enum AssetKind: String, Codable {
    case photo
    case livePhoto
    case video

    var title: String {
        switch self {
        case .photo: "Photo"
        case .livePhoto: "Live Photo"
        case .video: "Video"
        }
    }
}

/// The original or companion purpose reported by PhotoKit.
enum ResourceRole: String, Codable {
    case originalStill, originalVideo, pairedMotion, alternateStill, audio
    case editedRendition, editedMotion, adjustment, adjustmentBase, proxy, unknown

    var title: String {
        switch self {
        case .originalStill: "Original still"
        case .originalVideo: "Original video"
        case .pairedMotion: "Original paired motion"
        case .alternateStill: "Alternate still"
        case .audio: "Audio"
        case .editedRendition: "Edited rendition"
        case .editedMotion: "Edited motion"
        case .adjustment: "Adjustment sidecar"
        case .adjustmentBase: "Adjustment base"
        case .proxy: "Photo proxy"
        case .unknown: "Other PhotoKit resource"
        }
    }
}

/// Keeps source metadata separately from the path used for storing bytes.
struct SourceResource: Codable, Equatable {
    let role: ResourceRole
    let nativeType: Int
    let typeIdentifier: String
    let originalFilename: String
}

/// The read-only Photos observation used to detect source changes during staging.
struct SourceAsset: Codable, Equatable {
    let nativeID: String
    let kind: AssetKind
    let createdAt: Date?
    let modifiedAt: Date?
    let resources: [SourceResource]

    /// Refuse an incomplete Live Photo or a rendition without an original.
    func validate() throws {
        let roles = Set(resources.map(\.role))
        let required: Set<ResourceRole>
        switch kind {
        case .photo: required = [.originalStill]
        case .livePhoto: required = [.originalStill, .pairedMotion]
        case .video: required = [.originalVideo]
        }
        guard required.isSubset(of: roles) else {
            throw roles.contains(.proxy) ? StageFailure.waitingForSource : StageFailure.missingOriginal
        }
    }
}

/// Describes one opaque file in a staged package and the number of bytes written.
struct StagedResource: Codable {
    let file: String
    let byteLength: UInt64
}

/// One published local package. Resource paths correspond to the source manifest order.
struct PreparedOriginals: Codable, Identifiable {
    let formatVersion: Int
    let id: UUID
    let source: SourceAsset
    let files: [StagedResource]
}

/// Distinguishes an asset waiting for local bytes from one needing user attention.
enum StageFailure: LocalizedError {
    case waitingForSource
    case missingOriginal
    case changed
    case incompleteResource
    case inaccessible

    var errorDescription: String? {
        switch self {
        case .waitingForSource:
            "Photos needs to download at least one original. Make it available in Apple Photos, then retry."
        case .missingOriginal:
            "Photos did not provide every required original component for this asset."
        case .changed:
            "The asset changed while its originals were being read. Refresh and retry."
        case .incompleteResource:
            "An original resource could not be staged as a nonempty file."
        case .inaccessible:
            "This asset is no longer in the authorized, visible Photos library."
        }
    }
}

/// Owns the private filesystem boundary for locally prepared original packages.
struct OriginalStaging {
    let root: URL
    private let fileManager = FileManager.default

    /// Use a dedicated queue below the app's Application Support directory.
    static var applicationQueue: OriginalStaging {
        OriginalStaging(root: URL.applicationSupportDirectory
            .appendingPathComponent("Photogenic/Prepared", isDirectory: true))
    }

    /// List complete packages and remove interrupted writes before a new session.
    func recover() throws -> [PreparedOriginals] {
        try makeQueue()
        let entries = try fileManager.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
        var packages: [PreparedOriginals] = []
        for entry in entries {
            let name = entry.lastPathComponent
            if name.hasPrefix(".partial-"), UUID(uuidString: String(name.dropFirst(9))) != nil {
                try fileManager.removeItem(at: entry)
            } else if let id = UUID(uuidString: name) {
                let data = try Data(contentsOf: entry.appendingPathComponent("manifest.json"))
                let package = try JSONDecoder().decode(PreparedOriginals.self, from: data)
                guard package.id == id, package.formatVersion == 1 else { throw StageFailure.incompleteResource }
                packages.append(package)
            }
        }
        return packages
    }

    /// Read every resource, recheck the source, then publish one atomic directory.
    func prepare(
        _ source: SourceAsset,
        writeResource: (Int, URL) async throws -> Void,
        verifySource: () async throws -> Void
    ) async throws -> PreparedOriginals {
        try source.validate()
        try makeQueue()
        let id = UUID()
        let scratch = root.appendingPathComponent(".partial-" + id.uuidString, isDirectory: true)
        try fileManager.createDirectory(at: scratch, withIntermediateDirectories: false,
                                        attributes: [.posixPermissions: 0o700])
        var published = false
        defer { if !published { try? fileManager.removeItem(at: scratch) } }

        var files: [StagedResource] = []
        for index in source.resources.indices {
            try Task.checkCancellation()
            // PhotoKit's original filename is metadata, never a local path.
            let name = "resource-" + String(index)
            let output = scratch.appendingPathComponent(name)
            try await writeResource(index, output)
            let properties = try output.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
            guard properties.isRegularFile == true, let bytes = properties.fileSize, bytes > 0 else {
                throw StageFailure.incompleteResource
            }
            files.append(StagedResource(file: name, byteLength: UInt64(bytes)))
        }
        try await verifySource()
        try Task.checkCancellation()
        let package = PreparedOriginals(formatVersion: 1, id: id, source: source, files: files)
        let manifest = try JSONEncoder().encode(package)
        try manifest.write(to: scratch.appendingPathComponent("manifest.json"), options: .atomic)
        try fileManager.moveItem(at: scratch, to: packageURL(id))
        published = true
        return package
    }

    /// Remove only the app-owned package selected by the user.
    func remove(_ id: UUID) throws {
        try fileManager.removeItem(at: packageURL(id))
    }

    /// Point to a published package inside the queue.
    private func packageURL(_ id: UUID) -> URL {
        root.appendingPathComponent(id.uuidString, isDirectory: true)
    }

    /// Create the queue and keep this transient local handoff out of OS backups.
    private func makeQueue() throws {
        try fileManager.createDirectory(at: root, withIntermediateDirectories: true,
                                        attributes: [.posixPermissions: 0o700])
        var folder = root
        var options = URLResourceValues()
        options.isExcludedFromBackup = true
        try folder.setResourceValues(options)
    }
}

// Uses synthetic originals to exercise the publish boundary without Photos access.

import Foundation

@main
struct OriginalStagingCheck {
    /// Check that failures publish nothing and a complete Live Photo survives reopening.
    static func main() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let queue = OriginalStaging(root: root)
        defer { try? FileManager.default.removeItem(at: root) }

        let still = SourceResource(role: .originalStill, nativeType: 1,
                                   typeIdentifier: "public.heic", originalFilename: "../Still.HEIC")
        let motion = SourceResource(role: .pairedMotion, nativeType: 9,
                                    typeIdentifier: "com.apple.quicktime-movie", originalFilename: "Motion.MOV")
        let sidecar = SourceResource(role: .unknown, nativeType: 42,
                                     typeIdentifier: "public.data", originalFilename: "Extra.bin")
        let incomplete = livePhoto([still])
        let full = livePhoto([still, motion, sidecar])

        do {
            _ = try await queue.prepare(incomplete, writeResource: { _, _ in
                preconditionFailure("an incomplete pair must fail before reading")
            }, verifySource: {})
            preconditionFailure("published only half a Live Photo")
        } catch StageFailure.missingOriginal {}

        do {
            _ = try await queue.prepare(full, writeResource: { index, url in
                if index == 1 { throw StageFailure.waitingForSource }
                try Data("still".utf8).write(to: url)
            }, verifySource: {})
            preconditionFailure("published an incomplete cloud asset")
        } catch StageFailure.waitingForSource {}
        try expectEmpty(root)

        do {
            _ = try await queue.prepare(full, writeResource: write, verifySource: {
                throw StageFailure.changed
            })
            preconditionFailure("published an asset that changed during staging")
        } catch StageFailure.changed {}
        try expectEmpty(root)

        let ready = try await queue.prepare(full, writeResource: write, verifySource: {})
        let interrupted = root.appendingPathComponent(".partial-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: interrupted, withIntermediateDirectories: false)
        let recovered = try queue.recover()
        precondition(recovered.map(\.id) == [ready.id])
        precondition(!FileManager.default.fileExists(atPath: interrupted.path))
        precondition(recovered[0].source.resources == [still, motion, sidecar])
        precondition(recovered[0].files.map(\.file) == ["resource-0", "resource-1", "resource-2"])
        for (index, record) in recovered[0].files.enumerated() {
            let file = root.appendingPathComponent(ready.id.uuidString).appendingPathComponent(record.file)
            let bytes = try Data(contentsOf: file)
            precondition(bytes == Data("resource \(index)".utf8))
            precondition(record.byteLength == UInt64(bytes.count))
        }
        try queue.remove(ready.id)
        try expectEmpty(root)
    }

    /// Make a fixture whose declared components mimic a Live Photo.
    private static func livePhoto(_ resources: [SourceResource]) -> SourceAsset {
        SourceAsset(nativeID: "fixture/photo", kind: .livePhoto, createdAt: nil, modifiedAt: nil, resources: resources)
    }

    /// Write a distinct payload for each resource to catch accidental swapping.
    private static func write(_ index: Int, _ url: URL) async throws {
        try Data("resource \(index)".utf8).write(to: url)
    }

    /// Require cleanup of both incomplete bytes and unpublished manifests.
    private static func expectEmpty(_ root: URL) throws {
        let contents = try FileManager.default.contentsOfDirectory(atPath: root.path)
        precondition(contents.isEmpty)
    }
}

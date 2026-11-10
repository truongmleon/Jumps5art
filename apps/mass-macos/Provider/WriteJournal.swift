// Retains Finder's proposed item before attempting a network write. The
// pending directory is separate from Finder's evictable download cache.

import CryptoKit
import Darwin
import Foundation

/// The stable intent Finder can replay after a crash or lost acknowledgement.
struct CreateIntent: Codable, Equatable {
    let id: UUID
    let parent: String
    let name: String
    let hash: String?
}

/// Durable, private storage for pending new files and folders.
struct WriteJournal {
    static let limit = 32 * 1024 * 1024
    let root: URL
    private let files = FileManager.default

    /// Seal the exact proposed path and bytes, returning the same ID on replay.
    func seal(template: String, parent: String, name: String, source: URL?) throws -> CreateIntent {
        guard !template.isEmpty, UUID(uuidString: parent) != nil,
              (1...255).contains(name.utf8.count), !name.contains("/"),
              name.rangeOfCharacter(from: .controlCharacters) == nil,
              !name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              name != ".", name != ".." else {
            throw WriteFailure(message: "Choose a valid name inside an existing Mass folder.")
        }
        let content = try source.map(readFile)
        let digest = content.map { Self.hex(SHA256.hash(data: $0)) }
        // An encoded array includes the field boundaries and distinguishes an
        // empty file (a digest) from a folder (no digest).
        let identity = try JSONEncoder().encode([template, parent, name, digest])
        let id = UUID(uuid: Data(SHA256.hash(data: identity).prefix(16)).withUnsafeBytes {
            $0.loadUnaligned(as: uuid_t.self)
        })
        let intent = CreateIntent(id: id, parent: parent, name: name, hash: digest)
        let target = directory(id)
        try files.createDirectory(at: root, withIntermediateDirectories: true,
                                  attributes: [.posixPermissions: 0o700])
        var excluded = root
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try excluded.setResourceValues(values)
        if files.fileExists(atPath: target.path) {
            let stored = try JSONDecoder().decode(CreateIntent.self, from: Data(contentsOf: target.appendingPathComponent("intent.json")))
            let hash = try input(stored).map { Self.hex(SHA256.hash(data: try readFile($0))) }
            guard stored == intent, hash == digest else {
                throw WriteFailure(message: "Mass could not verify the existing pending copy.")
            }
            return intent
        }

        let scratch = root.appendingPathComponent(".sealing-" + UUID().uuidString, isDirectory: true)
        try files.createDirectory(at: scratch, withIntermediateDirectories: false,
                                  attributes: [.posixPermissions: 0o700])
        var sealed = false
        defer { if !sealed { try? files.removeItem(at: scratch) } }
        if let content { try write(content, to: scratch.appendingPathComponent("original")) }
        try write(JSONEncoder().encode(intent), to: scratch.appendingPathComponent("intent.json"))
        try syncDirectory(scratch)
        try files.moveItem(at: scratch, to: target)
        sealed = true
        try syncDirectory(root)
        try syncDirectory(root.deletingLastPathComponent())
        return intent
    }

    /// Pass only the sealed file to the Rust helper. A folder has no input.
    func input(_ intent: CreateIntent) -> URL? {
        intent.hash.map { _ in directory(intent.id).appendingPathComponent("original") }
    }

    /// Release the private copy after the helper verifies the server's receipt.
    func finish(_ intent: CreateIntent) throws {
        try files.removeItem(at: directory(intent.id))
        try syncDirectory(root)
    }

    /// Look up the package by its opaque identifier, never by user filename.
    private func directory(_ id: UUID) -> URL {
        root.appendingPathComponent(id.uuidString, isDirectory: true)
    }

    /// Read a regular File Provider snapshot with a hard size bound.
    private func readFile(_ url: URL) throws -> Data {
        let descriptor = open(url.path, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
        guard descriptor >= 0 else {
            throw WriteFailure(message: "Mass could not open a regular Finder snapshot.")
        }
        let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        defer { try? handle.close() }
        var before = stat()
        guard fstat(descriptor, &before) == 0, before.st_mode & S_IFMT == S_IFREG,
              before.st_size >= 0, before.st_size <= Self.limit else {
            throw WriteFailure(message: "Mass accepts regular files up to 32 MiB in this preview.")
        }
        var data = Data()
        while let next = try handle.read(upToCount: 1024 * 1024), !next.isEmpty {
            guard data.count <= Self.limit - next.count else {
                throw WriteFailure(message: "The file exceeded the 32 MiB Mass limit.")
            }
            data.append(next)
        }
        var after = stat()
        guard fstat(descriptor, &after) == 0, data.count == before.st_size,
              before.st_size == after.st_size,
              before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec,
              before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec else {
            throw WriteFailure(message: "The local file changed before it could be sealed.")
        }
        return data
    }

    /// Persist file contents before exposing the sealed package.
    private func write(_ bytes: Data, to url: URL) throws {
        try bytes.write(to: url, options: .withoutOverwriting)
        let handle = try FileHandle(forWritingTo: url)
        defer { try? handle.close() }
        try handle.synchronize()
    }

    /// Persist directory entries so a crash cannot lose a sealed intent.
    private func syncDirectory(_ directory: URL) throws {
        let descriptor = open(directory.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard descriptor >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        defer { _ = close(descriptor) }
        guard fsync(descriptor) == 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
    }

    /// Encode a local integrity digest without exposing a source path.
    private static func hex(_ digest: SHA256.Digest) -> String {
        digest.map { String(format: "%02x", $0) }.joined()
    }
}

/// A local preflight or journal refusal shown in Finder.
struct WriteFailure: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

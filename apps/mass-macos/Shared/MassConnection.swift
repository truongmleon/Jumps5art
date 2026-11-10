// The Finder extension and setup app share the Keychain profile and typed
// helper interface. The Rust process performs authenticated network I/O.

import Foundation
import Security

/// The private pairing input copied from the Linux server's temporary invitation.
struct Invitation: Codable {
    let pairing_id: String
    let server_id: String
    let server_key: String
    let lan_endpoint: String
    let token: [UInt8]
}

/// A Mac installation's persistent direct endpoint and private identity.
struct MassProfile: Codable {
    let address: String
    let server_key: String
    let server_id: String
    let instance_id: String
    let seed: [UInt8]
}

/// New local key material from the operating system's random source.
struct Identity: Codable {
    let instance_id: String
    let seed: [UInt8]
}

/// Comparison data shown by both the native app and the server operator.
struct Review: Decodable {
    let request_id: String
    let transcript: String
    let phrase: String
    let fingerprint: String
}

/// The pairing grant returned after its signature and binding verify.
struct Grant: Decodable {
    let device_id: String
}

/// An authenticated Mass catalog entry and its immutable file version.
struct MassEntry: Codable {
    let id: String
    let parent: String?
    let name: String
    let created_at_ms: String?
    let file: FileVersion?
}

/// The file's version, exact byte count, and verified BLAKE3 identity.
struct FileVersion: Codable {
    let version_id: String
    let byte_length: String
    let blob_id: String
}

/// One page of catalog entries and the opaque continuation token.
struct CatalogPage: Decodable {
    let entities: [MassEntry]
    let next_page: String?
}

/// The helper's result for a completed authenticated connection check.
struct Probe: Decodable {
    let connected: Bool
}

/// Private request sent through stdin to the bundled Rust executable.
struct HelperRequest: Encodable {
    let action: String
    var profile: MassProfile? = nil
    var invitation: Invitation? = nil
    var name: String? = nil
    var parent: String? = nil
    var after: String? = nil
    var id: String? = nil
    var version: String? = nil
    var output: String? = nil
    var request_id: UUID? = nil
    var input: String? = nil
    var sha256: String? = nil
}

/// Read the opt-in build flag from the app or extension bundle.
func finderWritePreview(_ bundle: Bundle) -> Bool {
    bundle.object(forInfoDictionaryKey: "MassWritePreview") as? Bool == true
}

/// An operation refusal shown in setup or mapped to a File Provider error.
struct MassFailure: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

/// A per-team access group shared only by the signed app and extension.
struct MassKeychain {
    private let query: [String: Any]

    /// Build the exact data-protection Keychain lookup for this signed target.
    init(bundle: Bundle) throws {
        guard let group = bundle.object(forInfoDictionaryKey: "MassKeychainGroup") as? String,
              !group.isEmpty, !group.contains("$") else {
            throw MassFailure(message: "The signed Mass Keychain group is missing.")
        }
        query = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "com.jumpstart.mass.finder",
            kSecAttrAccount as String: "paired-installation",
            kSecAttrAccessGroup as String: group,
            kSecUseDataProtectionKeychain as String: true,
        ]
    }

    /// Load the paired profile, returning nil only when it has never been stored.
    func load() throws -> MassProfile? {
        var lookup = query
        lookup[kSecReturnData as String] = true
        lookup[kSecMatchLimit as String] = kSecMatchLimitOne
        var value: CFTypeRef?
        let status = SecItemCopyMatching(lookup as CFDictionary, &value)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = value as? Data else {
            throw MassFailure(message: "Mass could not read its secure pairing profile (Keychain \(status)).")
        }
        return try JSONDecoder().decode(MassProfile.self, from: data)
    }

    /// Save an approved installation without exposing its seed as a local file.
    func save(_ profile: MassProfile) throws {
        let data = try JSONEncoder().encode(profile)
        let update = [kSecValueData as String: data] as CFDictionary
        var status = SecItemUpdate(query as CFDictionary, update)
        if status == errSecItemNotFound {
            var item = query
            item[kSecValueData as String] = data
            item[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
            status = SecItemAdd(item as CFDictionary, nil)
        }
        guard status == errSecSuccess else {
            throw MassFailure(message: "Mass could not save its pairing profile (Keychain \(status)).")
        }
    }

    /// Forget the local credential after the user removes the Finder location.
    func forget() throws {
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw MassFailure(message: "Mass could not remove its pairing profile (Keychain \(status)).")
        }
    }
}

/// The common native IPC entry point used by Finder and the setup app.
struct MassHelper {
    let bundle: Bundle

    /// Run the bundled helper, returning the final JSON value and any pending review.
    func run<T: Decodable>(
        _ request: HelperRequest,
        as type: T.Type,
        progress: Progress? = nil,
        pending: ((Review) -> Void)? = nil
    ) throws -> T {
        if progress?.isCancelled == true { throw CancellationError() }
        let process = Process()
        process.executableURL = bundle.bundleURL.appendingPathComponent("Contents/Helpers/mass-device")
        let input = Pipe()
        let output = Pipe()
        process.standardInput = input
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        try process.run()
        defer {
            progress?.cancellationHandler = nil
            if process.isRunning { process.terminate() }
        }
        progress?.cancellationHandler = { if process.isRunning { process.terminate() } }
        if progress?.isCancelled == true { throw CancellationError() }
        try input.fileHandleForWriting.write(contentsOf: JSONEncoder().encode(request))
        try input.fileHandleForWriting.close()

        var buffer = Data()
        while let bytes = try output.fileHandleForReading.read(upToCount: 16384), !bytes.isEmpty {
            buffer.append(bytes)
            while let newline = buffer.firstIndex(of: 10) {
                let line = Data(buffer[..<newline])
                buffer.removeSubrange(...newline)
                let header = try JSONDecoder().decode(HelperHeader.self, from: line)
                switch header.status {
                case "pending":
                    guard let review = try JSONDecoder().decode(HelperEvent<Review>.self, from: line).value else {
                        throw MassFailure(message: "The Mass helper returned no pairing review.")
                    }
                    guard UUID(uuidString: review.request_id) != nil,
                          review.transcript.range(of: "^[a-f0-9]{64}$", options: .regularExpression) != nil,
                          review.fingerprint.range(of: "^[a-f0-9]{16}$", options: .regularExpression) != nil,
                          review.phrase.range(of: "^[a-z]{1,12}( [a-z]{1,12}){5}$", options: .regularExpression) != nil else {
                        throw MassFailure(message: "The server returned an invalid pairing comparison.")
                    }
                    pending?(review)
                case "ok":
                    if progress?.isCancelled == true { throw CancellationError() }
                    guard let value = try JSONDecoder().decode(HelperEvent<T>.self, from: line).value else {
                        // Decoding Optional<MassEntry> intentionally permits null.
                        if let null = try? JSONDecoder().decode(T.self, from: Data("null".utf8)) { return null }
                        throw MassFailure(message: "The Mass helper returned no result.")
                    }
                    return value
                case "error":
                    throw MassFailure(message: header.message ?? "The Mass request failed.")
                default:
                    throw MassFailure(message: "The Mass helper returned an unknown event.")
                }
            }
            guard buffer.count <= 1024 * 1024 else {
                throw MassFailure(message: "The Mass helper reply exceeded its limit.")
            }
        }
        if progress?.isCancelled == true { throw CancellationError() }
        throw MassFailure(message: "The Mass helper stopped before returning a result.")
    }
}

/// Selects the schema for each newline-delimited helper event.
private struct HelperHeader: Decodable {
    let status: String
    let message: String?
}

/// Decodes the typed value only after the event type is known.
private struct HelperEvent<T: Decodable>: Decodable {
    let value: T?
}

// Carries one typed request from Tauri to the Apple Photos adapter. Original
// media stays in the app container and never crosses the webview bridge.

import Foundation

/// Byte callback borrowed for the duration of one response.
public typealias PhotoReply = @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<UInt8>?, Int, Bool) -> Void

/// Copy the borrowed request, then answer once on the Apple main actor.
@_cdecl("photogenic_photos")
public func photogenicPhotos(_ bytes: UnsafePointer<UInt8>, _ length: Int,
                             _ context: UnsafeMutableRawPointer?, _ reply: @escaping PhotoReply) {
    let request = Data(bytes: bytes, count: length)
    Task { @MainActor in
        let output: Data
        let success: Bool
        do {
            output = try await PhotosBridge.perform(request)
            success = true
        } catch {
            output = Data(error.localizedDescription.utf8)
            success = false
        }
        output.withUnsafeBytes { raw in
            reply(context, raw.bindMemory(to: UInt8.self).baseAddress, raw.count, success)
        }
    }
}

/// Validates every command and serializes dates as UTC ISO-8601 instants.
@MainActor
private enum PhotosBridge {
    private static let library = PhotoLibrary()

    /// Dispatch a validated Rust command to the matching native operation.
    static func perform(_ data: Data) async throws -> Data {
        let request = try JSONDecoder().decode(Request.self, from: data)
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        switch request.action {
        case .permission:
            await library.requestAccess()
        case .settings:
            library.openSettings()
        case .page:
            guard let filter = request.filter.flatMap(LibraryFilter.init(rawValue:)), let offset = request.offset else {
                throw StageFailure.inaccessible
            }
            return try encoder.encode(library.page(filter, offset: offset))
        case .thumbnail:
            return try await encoder.encode(library.thumbnail(request.item()))
        case .details:
            return try encoder.encode(library.details(request.item()))
        case .prepare:
            return try await encoder.encode(library.prepare(request.item()))
        case .queue:
            return try encoder.encode(library.queueRows())
        case .remove:
            guard let id = request.id.flatMap(UUID.init(uuidString:)) else { throw StageFailure.inaccessible }
            try library.remove(id)
        }
        return Data("null".utf8)
    }
}

/// Tauri passes a bounded request with action-specific fields.
private struct Request: Decodable {
    enum Action: String, Decodable {
        case permission, settings, page, thumbnail, details, prepare, queue, remove
    }
    let action: Action
    let id: String?
    let filter: String?
    let offset: Int?

    /// Validate a Photos local ID before passing it to PhotoKit.
    func item() throws -> String {
        guard let id, (1...512).contains(id.utf8.count) else { throw StageFailure.inaccessible }
        return id
    }
}

// Presents the paired Mass catalog as a read-only Finder location.
// Only the explicit Mass Download action can retrieve remote file bytes.

import CryptoKit
import FileProvider
import Foundation
import UniformTypeIdentifiers

/// The principal class loaded by the macOS File Provider service.
final class MassProvider: NSObject, NSFileProviderReplicatedExtension, NSFileProviderCustomAction {
    private let domain: NSFileProviderDomain
    private let storage: URL
    private let names: MountNames
    private let bundle: Bundle
    private let work = DispatchQueue(label: "com.jumpstart.mass.provider", qos: .userInitiated)
    private static let downloadAction = NSFileProviderExtensionActionIdentifier("com.jumpstart.mass.download")

    /// Retain the domain and its private filename and verified download state.
    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        bundle = Bundle(for: MassProvider.self)
        storage = URL.applicationSupportDirectory.appendingPathComponent("MassFinder")
            .appendingPathComponent(domain.identifier.rawValue, isDirectory: true)
        names = MountNames(url: storage.appendingPathComponent("names.json"))
        super.init()
    }

    /// File Provider can release the domain once its outstanding work completes.
    func invalidate() {}

    /// Resolve a Finder ID directly against the paired catalog.
    func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        run({ progress in
            if identifier == .rootContainer {
                return FinderItem(entry: MassEntry(id: identifier.rawValue, parent: nil, name: "Mass", created_at_ms: nil, file: nil), display: "Mass")
            }
            return try self.finderItem(self.entry(identifier, progress: progress))
        }, completion: completionHandler)
    }

    /// Enumerate root and ordinary folders. The preview has no trash or offline working set.
    func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        if containerItemIdentifier == .workingSet || containerItemIdentifier == .trashContainer {
            return MassEnumerator(provider: self, parent: nil, empty: true)
        }
        let parent: String?
        if containerItemIdentifier == .rootContainer {
            parent = nil
        } else {
            parent = try checkedID(containerItemIdentifier)
        }
        return MassEnumerator(provider: self, parent: parent, empty: false)
    }

    /// Return only local bytes previously verified by an explicit Finder action.
    func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        run({ _ in
            try self.preparedContents(for: itemIdentifier, version: requestedVersion)
        }, completion: { result, error in
            completionHandler(result?.0, result?.1, error)
        })
    }

    /// Download an explicitly selected Finder item and ask the system to materialize it.
    func performAction(
        identifier: NSFileProviderExtensionActionIdentifier,
        onItemsWithIdentifiers itemIdentifiers: [NSFileProviderItemIdentifier],
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        guard identifier == Self.downloadAction, itemIdentifiers.count == 1, let item = itemIdentifiers.first else {
            completionHandler(MassFailure(message: "Select one Mass file to download."))
            return progress
        }
        work.async {
            do {
                try self.prepareDownload(item, progress: progress)
                guard let manager = NSFileProviderManager(for: self.domain) else {
                    throw MassFailure(message: "The Mass Finder domain is unavailable.")
                }
                // Requesting materialization is safe here. fetchContents only reads
                // the verified local staging copy and never starts a remote fetch.
                manager.requestDownloadForItem(withIdentifier: item, requestedRange: nil) { error in
                    progress.completedUnitCount = 1
                    completionHandler(error)
                }
            } catch {
                completionHandler(self.fileProviderError(error))
            }
        }
        return progress
    }

    /// Refuse Finder writes while the durable offline save journal is unavailable.
    func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        completionHandler(nil, [], false, CocoaError(.fileWriteNoPermission))
        return Progress(totalUnitCount: 0)
    }

    /// Refuse changes to either file bytes or the mounted namespace.
    func modifyItem(
        _ item: NSFileProviderItem,
        baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields,
        contents newContents: URL?,
        options: NSFileProviderModifyItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void
    ) -> Progress {
        completionHandler(nil, [], false, CocoaError(.fileWriteNoPermission))
        return Progress(totalUnitCount: 0)
    }

    /// Refuse deletion through the read-only Finder preview.
    func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions,
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        completionHandler(CocoaError(.fileWriteNoPermission))
        return Progress(totalUnitCount: 0)
    }

    /// Fetch one page of catalog metadata and reserve its stable Finder names.
    fileprivate func page(parent: String?, after: String?, progress: Progress) throws -> (items: [FinderItem], next: String?) {
        let profile = try profile()
        let page = try MassHelper(bundle: bundle).run(
            HelperRequest(action: "list", profile: profile, parent: parent, after: after),
            as: CatalogPage.self, progress: progress
        )
        let items = try page.entities.map { try finderItem($0) }
        return (items, page.next_page)
    }

    /// Serialize extension work with one completion and cancellation path.
    fileprivate func run<T>(
        _ action: @escaping (Progress) throws -> T,
        completion: @escaping (T?, Error?) -> Void
    ) -> Progress {
        let progress = Progress(totalUnitCount: 1)
        work.async {
            do {
                if progress.isCancelled { throw CancellationError() }
                let result = try action(progress)
                progress.completedUnitCount = 1
                completion(result, nil)
            } catch {
                completion(nil, self.fileProviderError(error))
            }
        }
        return progress
    }

    /// Read only the profile that matches this native domain's server ID.
    private func profile() throws -> MassProfile {
        guard let profile = try MassKeychain(bundle: bundle).load(),
              domain.identifier.rawValue == Self.domainIdentifier(for: profile) else {
            throw NSFileProviderError(.notAuthenticated)
        }
        return profile
    }

    /// Read a catalog entry by the full identity supplied by Finder.
    private func entry(_ identifier: NSFileProviderItemIdentifier, progress: Progress) throws -> MassEntry {
        let id = try checkedID(identifier)
        guard let item = try MassHelper(bundle: bundle).run(
            HelperRequest(action: "item", profile: try profile(), id: id),
            as: Optional<MassEntry>.self, progress: progress
        ), item.id == id else {
            throw NSFileProviderError(.noSuchItem)
        }
        return item
    }

    /// Validate the ID independently of filenames or local paths.
    private func checkedID(_ identifier: NSFileProviderItemIdentifier) throws -> String {
        guard UUID(uuidString: identifier.rawValue) != nil else {
            throw NSFileProviderError(.noSuchItem)
        }
        return identifier.rawValue
    }

    /// Reserve the stable, collision-safe name and wrap the underlying entry.
    private func finderItem(_ entry: MassEntry) throws -> FinderItem {
        let display = try names.displayName(parent: entry.parent, id: entry.id, canonical: entry.name)
        return FinderItem(entry: entry, display: display)
    }

    /// Verify an explicit download and retain a single ready cache record.
    private func prepareDownload(_ id: NSFileProviderItemIdentifier, progress: Progress) throws {
        let item = try entry(id, progress: progress)
        guard let version = item.file?.version_id else {
            throw MassFailure(message: "Select a file. This preview downloads one file at a time.")
        }
        try FileManager.default.createDirectory(at: storage, withIntermediateDirectories: true)
        let previous = try? loadReady()
        let local = UUID()
        let bytes = storage.appendingPathComponent(local.uuidString)
        var published = false
        defer { if !published { try? FileManager.default.removeItem(at: bytes) } }
        let verified = try MassHelper(bundle: bundle).run(
            HelperRequest(action: "fetch", profile: try profile(), id: item.id, version: version, output: bytes.path),
            as: MassEntry.self, progress: progress
        )
        guard verified.id == item.id, verified.file?.version_id == version else {
            throw MassFailure(message: "The verified file version changed during download.")
        }
        let record = Ready(entry: verified, local: local)
        try JSONEncoder().encode(record).write(to: storage.appendingPathComponent("ready.json"), options: .atomic)
        published = true
        if let previous {
            try? FileManager.default.removeItem(at: storage.appendingPathComponent(previous.local.uuidString))
        }
    }

    /// Copy a matching verified local file into the system's handoff directory.
    private func preparedContents(
        for id: NSFileProviderItemIdentifier,
        version: NSFileProviderItemVersion?
    ) throws -> (URL, FinderItem) {
        guard let ready = try? loadReady(), ready.entry.id == id.rawValue,
              let file = ready.entry.file,
              version == nil || version?.contentVersion == Data(file.version_id.utf8) else {
            throw NSError(
                domain: NSFileProviderErrorDomain,
                code: NSFileProviderError.Code.serverUnreachable.rawValue,
                userInfo: [NSLocalizedDescriptionKey: "Right-click this file and choose Download from Mass."]
            )
        }
        guard let manager = NSFileProviderManager(for: domain) else {
            throw NSFileProviderError(.serverUnreachable)
        }
        let original = storage.appendingPathComponent(ready.local.uuidString)
        let attributes = try FileManager.default.attributesOfItem(atPath: original.path)
        guard (attributes[.size] as? NSNumber)?.uint64Value == UInt64(file.byte_length) else {
            throw NSFileProviderError(.serverUnreachable)
        }
        let item = try finderItem(ready.entry)
        let output = try manager.temporaryDirectoryURL().appendingPathComponent(UUID().uuidString)
        try FileManager.default.copyItem(at: original, to: output)
        return (output, item)
    }

    /// Load the ready record without treating any server text as a local path.
    private func loadReady() throws -> Ready {
        try JSONDecoder().decode(Ready.self, from: Data(contentsOf: storage.appendingPathComponent("ready.json")))
    }

    /// Translate transport and cancellation failures to Finder's protocol.
    private func fileProviderError(_ error: Error) -> Error {
        if error is CancellationError { return CocoaError(.userCancelled) }
        if (error as NSError).domain == NSFileProviderErrorDomain { return error }
        return NSError(
            domain: NSFileProviderErrorDomain,
            code: NSFileProviderError.Code.serverUnreachable.rawValue,
            userInfo: [NSLocalizedDescriptionKey: error.localizedDescription]
        )
    }

    /// Keep the File Provider domain name tied to its paired server.
    private static func domainIdentifier(for profile: MassProfile) -> String { "mass-" + profile.server_id }
}

/// The single explicitly approved file waiting for Finder to materialize it.
private struct Ready: Codable {
    let entry: MassEntry
    let local: UUID
}

/// A catalog entry projected into macOS metadata and read-only capabilities.
private final class FinderItem: NSObject, NSFileProviderItem {
    let entry: MassEntry
    let filename: String

    /// Retain the stable catalog entry and its persisted display name.
    init(entry: MassEntry, display: String) {
        self.entry = entry
        self.filename = display
    }

    var itemIdentifier: NSFileProviderItemIdentifier { NSFileProviderItemIdentifier(entry.id) }
    var parentItemIdentifier: NSFileProviderItemIdentifier {
        entry.parent.map { NSFileProviderItemIdentifier($0) } ?? .rootContainer
    }
    var contentType: UTType {
        if entry.file == nil { return .folder }
        return UTType(filenameExtension: (filename as NSString).pathExtension) ?? .data
    }
    var documentSize: NSNumber? {
        entry.file.flatMap { UInt64($0.byte_length) }.map(NSNumber.init(value:))
    }
    var creationDate: Date? {
        entry.created_at_ms.flatMap(Double.init).map { Date(timeIntervalSince1970: $0 / 1000) }
    }
    var contentModificationDate: Date? { creationDate }
    var capabilities: NSFileProviderItemCapabilities {
        entry.file == nil ? [.allowsContentEnumerating] : [.allowsReading]
    }
    var itemVersion: NSFileProviderItemVersion {
        let metadata = Data([entry.id, entry.parent ?? "", filename].joined(separator: "\0").utf8)
        return NSFileProviderItemVersion(
            contentVersion: Data((entry.file?.version_id ?? entry.id).utf8),
            metadataVersion: Data(SHA256.hash(data: metadata))
        )
    }
}

/// Enumerates Finder folders by a server cursor with no synthetic local files.
private final class MassEnumerator: NSObject, NSFileProviderEnumerator {
    private weak var provider: MassProvider?
    private let parent: String?
    private let empty: Bool
    private static let anchor = NSFileProviderSyncAnchor(Data("mass-preview-v1".utf8))

    /// Bind one enumerator to a catalog folder or intentionally empty special container.
    init(provider: MassProvider, parent: String?, empty: Bool) {
        self.provider = provider
        self.parent = parent
        self.empty = empty
    }

    /// Clear the association when the system invalidates this enumerator.
    func invalidate() { provider = nil }

    /// Retrieve the requested page and pass through its next stable cursor.
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        if empty { observer.finishEnumerating(upTo: nil); return }
        guard let provider else { observer.finishEnumeratingWithError(CocoaError(.userCancelled)); return }
        let after: String?
        if page.rawValue == NSFileProviderPage.initialPageSortedByName as Data
            || page.rawValue == NSFileProviderPage.initialPageSortedByDate as Data {
            after = nil
        } else if let value = String(data: page.rawValue, encoding: .utf8), UUID(uuidString: value) != nil {
            after = value
        } else {
            observer.finishEnumeratingWithError(NSFileProviderError(.pageExpired))
            return
        }
        _ = provider.run({ progress in
            try provider.page(parent: self.parent, after: after, progress: progress)
        }, completion: { result, error in
            if let error { observer.finishEnumeratingWithError(error); return }
            guard let result else { observer.finishEnumeratingWithError(NSFileProviderError(.serverUnreachable)); return }
            observer.didEnumerate(result.items)
            observer.finishEnumerating(upTo: result.next.map { NSFileProviderPage(Data($0.utf8)) })
        })
    }

    /// Invalidate normal-folder snapshots until the app change feed is available.
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        if empty {
            observer.finishEnumeratingChanges(upTo: Self.anchor, moreComing: false)
        } else {
            observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
        }
    }

    /// Return the preview anchor used to request a fresh catalog enumeration.
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        completionHandler(Self.anchor)
    }
}

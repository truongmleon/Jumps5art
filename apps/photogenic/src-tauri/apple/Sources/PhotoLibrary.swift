// Reads only the authorized PhotoKit library. This adapter keeps grid
// thumbnails separate from original bytes and never changes Apple Photos.

import Foundation
import Photos
#if os(macOS)
import AppKit
#else
import UIKit
#endif

/// App-level access state used to distinguish a limited selection from full authorization.
enum LibraryAccess: String, Encodable {
    case request, limited, full, denied, restricted

    var canBrowse: Bool { self == .limited || self == .full }

    /// Map unknown system states to a safe refusal.
    init(_ status: PHAuthorizationStatus) {
        switch status {
        case .notDetermined: self = .request
        case .limited: self = .limited
        case .authorized: self = .full
        case .denied: self = .denied
        case .restricted: self = .restricted
        @unknown default: self = .restricted
        }
    }
}

/// The timeline filters, applied to the Photos query before pagination.
enum LibraryFilter: String, CaseIterable, Identifiable {
    case all = "All", photos = "Photos", videos = "Videos"
    var id: Self { self }

    var predicate: NSPredicate? {
        switch self {
        case .all: NSPredicate(format: "mediaType IN %@", [PHAssetMediaType.image.rawValue, PHAssetMediaType.video.rawValue])
        case .photos: NSPredicate(format: "mediaType = %d", PHAssetMediaType.image.rawValue)
        case .videos: NSPredicate(format: "mediaType = %d", PHAssetMediaType.video.rawValue)
        }
    }
}

/// A visible Photos asset and its media badge.
struct LibraryAsset: Identifiable {
    let asset: PHAsset
    var id: String { asset.localIdentifier }

    var kind: AssetKind {
        if asset.mediaType == .video { return .video }
        return asset.mediaSubtypes.contains(.photoLive) ? .livePhoto : .photo
    }

    /// Describe all resources PhotoKit exposes for this exact snapshot.
    func snapshot(resources: [PHAssetResource]) -> SourceAsset {
        SourceAsset(nativeID: id, kind: kind, createdAt: asset.creationDate,
                    modifiedAt: asset.modificationDate, resources: resources.map(\.source))
    }
}

private extension PHAssetResource {
    /// Preserve unknown native roles and actual file types for future inspection.
    var source: SourceResource {
        let role: ResourceRole
        switch type {
        case .photo: role = .originalStill
        case .video: role = .originalVideo
        case .pairedVideo: role = .pairedMotion
        case .alternatePhoto: role = .alternateStill
        case .audio: role = .audio
        case .fullSizePhoto, .fullSizeVideo: role = .editedRendition
        case .fullSizePairedVideo: role = .editedMotion
        case .adjustmentData: role = .adjustment
        case .adjustmentBasePhoto, .adjustmentBaseVideo, .adjustmentBasePairedVideo: role = .adjustmentBase
        case .photoProxy: role = .proxy
        @unknown default: role = .unknown
        }
        return SourceResource(role: role, nativeType: type.rawValue,
                              typeIdentifier: uniformTypeIdentifier, originalFilename: originalFilename)
    }
}

/// One entry in the timeline. Thumbnails are retrieved separately as cells come into view.
struct Tile: Encodable {
    let id: String
    let kind: AssetKind
    let date: Date?
}

/// A page of visible assets and the permission and staging information shown with it.
struct LibraryPage: Encodable {
    let access: LibraryAccess
    let assets: [Tile]
    let total: Int
    let queueCount: Int
    let queueError: String?
}

/// The exact resource declarations presented before staging a selected asset.
struct AssetDetails: Encodable {
    let kind: AssetKind
    let resources: [SourceResource]
    let staged: Bool
    let sourceChanged: Bool
}

/// A queue row intentionally excludes native IDs and file names.
struct QueueRow: Encodable {
    let id: UUID
    let kind: AssetKind
    let count: Int
    let date: Date?
}

/// The selected asset's local staging result.
struct StageResult: Encodable {
    let status: String
    let message: String?
}

/// Reads authorized Photos metadata and serializes modifications to the local queue.
@MainActor
final class PhotoLibrary {
    private let queue = OriginalStaging.applicationQueue
    private var staged: [PreparedOriginals] = []
    private var active = false
    private var queueError: String?
    private let images = PHCachingImageManager()

    /// Reopen previously published local packages once per app process.
    init() {
        do {
            staged = try queue.recover()
        } catch {
            queueError = "The local staging queue could not be opened. \(error.localizedDescription)"
        }
    }

    /// Ask for Photos access only in response to a user action.
    func requestAccess() async {
        _ = await PHPhotoLibrary.requestAuthorization(for: .readWrite)
    }

    /// Return up to 96 metadata records from the current permission-bounded timeline.
    func page(_ filter: LibraryFilter, offset: Int) -> LibraryPage {
        let access = currentAccess()
        guard access.canBrowse else {
            return LibraryPage(access: access, assets: [], total: 0, queueCount: staged.count, queueError: queueError)
        }
        let options = PHFetchOptions()
        options.includeHiddenAssets = false
        options.sortDescriptors = [NSSortDescriptor(key: "creationDate", ascending: false)]
        options.predicate = filter.predicate
        let results = PHAsset.fetchAssets(with: options)
        let start = min(max(offset, 0), results.count)
        let end = start + min(96, results.count - start)
        let tiles = (start..<end).map { index in
            let item = LibraryAsset(asset: results.object(at: index))
            return Tile(id: item.id, kind: item.kind, date: item.asset.creationDate)
        }
        return LibraryPage(access: access, assets: tiles, total: results.count, queueCount: staged.count,
                           queueError: queueError)
    }

    /// Return only a local, small PhotoKit thumbnail, in JPEG form for the WebView.
    func thumbnail(_ id: String) async throws -> String? {
        guard let entry = find(id) else { throw StageFailure.inaccessible }
        let options = PHImageRequestOptions()
        options.isNetworkAccessAllowed = false
        options.deliveryMode = .fastFormat
        options.resizeMode = .fast
        return await withCheckedContinuation { continuation in
            images.requestImage(for: entry.asset, targetSize: CGSize(width: 290, height: 290),
                                contentMode: .aspectFill, options: options) { image, _ in
                #if os(macOS)
                let jpeg = image?.tiffRepresentation.flatMap(NSBitmapImageRep.init(data:))?
                    .representation(using: .jpeg, properties: [.compressionFactor: 0.75])
                #else
                let jpeg = image?.jpegData(compressionQuality: 0.75)
                #endif
                continuation.resume(returning: jpeg?.base64EncodedString())
            }
        }
    }

    /// Describe the native original resources for a currently authorized selection.
    func details(_ id: String) throws -> AssetDetails {
        guard let entry = find(id) else { throw StageFailure.inaccessible }
        let source = entry.snapshot(resources: PHAssetResource.assetResources(for: entry.asset))
        let copy = staged.first { $0.source.nativeID == id }
        return AssetDetails(kind: entry.kind, resources: source.resources, staged: copy != nil,
                            sourceChanged: copy.map { $0.source != source } ?? false)
    }

    /// Return the removable local queue even if Photos permission was later changed.
    func queueRows() -> [QueueRow] {
        staged.map { QueueRow(id: $0.id, kind: $0.source.kind, count: $0.files.count, date: $0.source.createdAt) }
    }

    /// Stage all original and companion bytes using only resources already on this device.
    func prepare(_ id: String) async -> StageResult {
        if let queueError { return StageResult(status: "attention", message: queueError) }
        guard !active else {
            return StageResult(status: "attention", message: "Wait for the current preparation to finish.")
        }
        guard !staged.contains(where: { $0.source.nativeID == id }) else {
            return StageResult(status: "attention", message: "Remove the previous copy from the local queue before staging again.")
        }
        active = true
        defer { active = false }
        do {
            guard let entry = find(id) else { throw StageFailure.inaccessible }
            let resources = PHAssetResource.assetResources(for: entry.asset)
            let source = entry.snapshot(resources: resources)
            let package = try await queue.prepare(source, writeResource: { index, url in
                let options = PHAssetResourceRequestOptions()
                options.isNetworkAccessAllowed = false
                do {
                    try await PHAssetResourceManager.default().writeData(for: resources[index], toFile: url, options: options)
                } catch let error as NSError where error.domain == PHPhotosErrorDomain
                    && error.code == PHPhotosError.Code.networkAccessRequired.rawValue {
                    throw StageFailure.waitingForSource
                }
            }, verifySource: {
                guard let current = self.find(id) else { throw StageFailure.inaccessible }
                let snapshot = current.snapshot(resources: PHAssetResource.assetResources(for: current.asset))
                guard source == snapshot else { throw StageFailure.changed }
            })
            staged.append(package)
            return StageResult(status: "staged", message: nil)
        } catch let error as StageFailure {
            return StageResult(status: error == .waitingForSource ? "waiting" : "attention",
                               message: error.localizedDescription)
        } catch {
            return StageResult(status: "attention", message: error.localizedDescription)
        }
    }

    /// Remove the selected local copy only, leaving Apple Photos unchanged.
    func remove(_ id: UUID) throws {
        guard !active, staged.contains(where: { $0.id == id }) else {
            throw StageFailure.inaccessible
        }
        try queue.remove(id)
        staged.removeAll { $0.id == id }
    }

    /// Open the platform's Settings route for adjusting Photos permission.
    func openSettings() {
        #if os(macOS)
        if let url = URL(string: "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_Photos") {
            NSWorkspace.shared.open(url)
        }
        #else
        if let url = URL(string: UIApplication.openSettingsURLString) {
            UIApplication.shared.open(url)
        }
        #endif
    }

    /// Refetch one authorized photo or video without including hidden items.
    private func find(_ id: String) -> LibraryAsset? {
        guard currentAccess().canBrowse else { return nil }
        let options = PHFetchOptions()
        options.includeHiddenAssets = false
        guard let asset = PHAsset.fetchAssets(withLocalIdentifiers: [id], options: options).firstObject,
              !asset.isHidden, asset.mediaType == .image || asset.mediaType == .video else { return nil }
        return LibraryAsset(asset: asset)
    }

    /// Recheck Photos permission before every operation, including after system Settings.
    private func currentAccess() -> LibraryAccess {
        LibraryAccess(PHPhotoLibrary.authorizationStatus(for: .readWrite))
    }
}

// Finder can compare names case-insensitively even when Mass stores distinct
// names. This map keeps a display name stable when another item later collides.

import Foundation

/// Durable per-parent filename allocation for a File Provider domain.
final class MountNames {
    private struct Entry: Codable {
        let canonical: String
        let display: String
    }

    private let lock = NSLock()
    private let url: URL

    /// Point the map at this domain's private application support directory.
    init(url: URL) { self.url = url }

    /// Resolve or reserve a safe Finder filename for the exact catalog identity.
    func displayName(parent: String?, id: String, canonical: String) throws -> String {
        lock.lock()
        defer { lock.unlock() }
        let parent = parent ?? "root"
        var map: [String: [String: Entry]] = [:]
        if FileManager.default.fileExists(atPath: url.path) {
            map = try JSONDecoder().decode([String: [String: Entry]].self, from: Data(contentsOf: url))
        }
        var siblings = map[parent] ?? [:]
        if let previous = siblings[id], previous.canonical == canonical { return previous.display }
        siblings.removeValue(forKey: id)
        let occupied = Set(siblings.values.map { Self.fold($0.display) })
        let cleaned = Self.clean(canonical)
        var display = Self.fit(cleaned, suffix: "")
        if occupied.contains(Self.fold(display)) {
            let short = String(id.replacingOccurrences(of: "-", with: "").suffix(8))
            display = Self.fit(cleaned, suffix: " (\(short))")
            if occupied.contains(Self.fold(display)) {
                display = Self.fit(cleaned, suffix: " (\(id))")
            }
            var number = 2
            while occupied.contains(Self.fold(display)) {
                display = Self.fit(cleaned, suffix: " (\(id)-\(number))")
                number += 1
            }
        }
        siblings[id] = Entry(canonical: canonical, display: display)
        map[parent] = siblings
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try JSONEncoder().encode(map).write(to: url, options: .atomic)
        return display
    }

    /// Compare names using a conservative form of Finder's normalization.
    private static func fold(_ name: String) -> String {
        name.decomposedStringWithCanonicalMapping.folding(
            options: [.caseInsensitive, .widthInsensitive], locale: Locale(identifier: "en_US_POSIX")
        )
    }

    /// Replace names Finder reserves while leaving ordinary Unicode intact.
    private static func clean(_ name: String) -> String {
        var result = ""
        for scalar in name.unicodeScalars {
            if scalar == ":" || scalar == "/" || CharacterSet.controlCharacters.contains(scalar) {
                result.append("_")
            } else {
                result.unicodeScalars.append(scalar)
            }
        }
        if result.isEmpty || result == "." || result == ".." { return "Untitled" }
        return result
    }

    /// Fit the display name and collision suffix into Finder's component budget.
    private static func fit(_ name: String, suffix: String) -> String {
        let original = name as NSString
        let ext = original.pathExtension
        let ending = ext.isEmpty || ext.decomposedStringWithCanonicalMapping.utf8.count > 40 ? "" : ".\(ext)"
        let base = ending.isEmpty ? name : original.deletingPathExtension
        let remainder = 255 - (suffix + ending).decomposedStringWithCanonicalMapping.utf8.count
        var trimmed = ""
        var remaining = remainder
        for character in base {
            let length = String(character).decomposedStringWithCanonicalMapping.utf8.count
            if remaining < length { break }
            trimmed.append(character)
            remaining -= length
        }
        return (trimmed.isEmpty ? "File" : trimmed) + suffix + ending
    }
}

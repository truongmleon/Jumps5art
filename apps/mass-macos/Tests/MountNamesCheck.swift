// Checks filename safety and stability across Finder extension restarts.

import Foundation

@main
struct MountNamesCheck {
    /// Verify case and Unicode collisions without registering a real Finder domain.
    static func main() throws {
        let scratch = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: scratch) }
        let url = scratch.appendingPathComponent("names.json")
        let names = MountNames(url: url)
        let original = try names.displayName(parent: nil, id: "aaaa0001", canonical: "Photo.jpg")
        let collision = try names.displayName(parent: nil, id: "bbbb0002", canonical: "photo.jpg")
        precondition(original == "Photo.jpg")
        precondition(collision == "photo (bbbb0002).jpg")
        let restarted = MountNames(url: url)
        let reopened = try restarted.displayName(parent: nil, id: "bbbb0002", canonical: "photo.jpg")
        precondition(reopened == collision)
        let separate = try restarted.displayName(parent: "folder", id: "cccc0003", canonical: "photo.jpg")
        precondition(separate == "photo.jpg")
        let composed = try restarted.displayName(parent: nil, id: "dddd0004", canonical: "Café")
        let decomposed = try restarted.displayName(parent: nil, id: "eeee0005", canonical: "Cafe\u{301}")
        precondition(composed != decomposed)
        let longName = try restarted.displayName(parent: nil, id: "ffff0006", canonical: String(repeating: "é", count: 128) + ".txt")
        precondition(longName.decomposedStringWithCanonicalMapping.utf8.count <= 255)
        let clean = try restarted.displayName(parent: nil, id: "ffff0007", canonical: "notes:today")
        precondition(clean == "notes_today")
    }
}

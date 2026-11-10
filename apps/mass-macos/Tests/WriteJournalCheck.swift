// Exercises the Finder write journal without registering a signed domain.

import Foundation

@main
struct WriteJournalCheck {
    /// Verify durable retries, generation changes, empty files and receipt cleanup.
    static func main() throws {
        let scratch = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: scratch) }
        let root = scratch.appendingPathComponent("PendingCreates")
        let source = scratch.appendingPathComponent("FinderSnapshot")
        let parent = UUID().uuidString
        let bytes = Data("first sealed Finder save".utf8)
        try bytes.write(to: source)
        let journal = WriteJournal(root: root)
        let first = try journal.seal(template: "stable Finder ID", parent: parent, name: "File.txt", source: source)
        let reopened = WriteJournal(root: root)
        let replayed = try reopened.seal(template: "stable Finder ID", parent: parent, name: "File.txt", source: source)
        precondition(first == replayed)
        let firstData = try Data(contentsOf: reopened.input(first)!)
        precondition(firstData == bytes)

        try Data("different local bytes".utf8).write(to: source)
        let changed = try reopened.seal(template: "stable Finder ID", parent: parent, name: "File.txt", source: source)
        precondition(changed.id != first.id)
        let retained = try Data(contentsOf: reopened.input(first)!)
        precondition(retained == bytes)
        try reopened.finish(first)
        try bytes.write(to: source)
        let afterReceipt = try reopened.seal(template: "stable Finder ID", parent: parent, name: "File.txt", source: source)
        precondition(afterReceipt == first)

        try Data().write(to: source)
        let empty = try reopened.seal(template: "new identifier", parent: parent, name: "Empty", source: source)
        let folder = try reopened.seal(template: "new identifier", parent: parent, name: "Empty", source: nil)
        precondition(empty.id != folder.id)
        precondition(reopened.input(folder) == nil)
        let emptyData = try Data(contentsOf: reopened.input(empty)!)
        precondition(emptyData.isEmpty)

        let symbolic = scratch.appendingPathComponent("SymbolicLink")
        try FileManager.default.createSymbolicLink(at: symbolic, withDestinationURL: source)
        do {
            _ = try reopened.seal(template: "symlink", parent: parent, name: "Link", source: symbolic)
            preconditionFailure("accepted a symbolic link")
        } catch is WriteFailure {}
    }
}

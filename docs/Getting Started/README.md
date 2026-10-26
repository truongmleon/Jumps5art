# Getting started

Jumpstart is your own library. One Linux computer stores everything. Your phone, tablet, and computers open it through separate apps. There is one person and no shared accounts.

These guides describe the finished apps. They are not available to install yet.

| App | Open it when you want |
|---|---|
| [Muse](../Muse/README.md) | Music |
| [Photogenic](../Photogenic/README.md) | Photos and short videos |
| [Mass](../Mass/README.md) | Files, remote downloads, and backups |
| [Bato](../Bato/README.md) | Books and manga |
| [Flex](../Flex/README.md) | Movies, shows, and anime |
| [Worlds](../Worlds/README.md) | A Fabric Minecraft server, from the Linux computer |

## Pair a device

The first device is approved on the server itself.

1. On the server, start a pairing window.
2. On the new phone or computer, scan the code or enter it. You must be on the same home network.
3. Compare the confirmation phrase on both sides.
4. Approve that exact device.

Later devices are approved from a device you already trust, or again from the server. A matching name is not enough. Each device also shows its platform and when it was last seen.

Remove a device when you no longer want it connected. Its access stops immediately, including anything it was streaming. Music and files already downloaded on it stay on it. Changes that had not reached the server stay on it until you export or discard them.

## While you are away from home

- If the server is reachable, you can stream and sync.
- If it is not, open what you already downloaded.
- Anything you change offline waits as **Pending Sync**. It is sent when you reconnect, in the order you made the changes.
- If the library changed while you were offline, edits that can be combined are combined. If they cannot, Jumpstart keeps both and lets you apply or dismiss yours. Nothing is thrown away quietly.
- An offline delete becomes **Pending Deletion** on that device. After reconnect, the server file moves to Recently Deleted.

## Delete, restore, and undo

Deleting in Muse, Photogenic, Mass, Bato, or Flex moves the file to Recently Deleted for 30 days. You can still see it in the app that owned it.

- **Restore** puts the file back, including bookmarks, progress, and album membership that still exist.
- If the old name or place is taken, choose **Restore as Copy**, **Replace Existing**, or **Cancel**.
- **Delete permanently** removes the file and its app details.

On a desktop, Ctrl/Cmd+Z undoes your latest reversible action on that device, such as a rename, move, reorder, or delete. To undo something done on another device, open Activity History and choose it. History lasts seven days. Deleted file contents live in Recently Deleted, not in history.

Uploading a file with the same name as one already there asks you to **Replace**, **Keep Both**, or **Cancel**.

## Jobs

Uploads, downloads, backups, disc rips, and video conversion appear as jobs. You can pause, resume, cancel, and retry them when the source allows it. If it cannot continue, the job says it must start over and keeps what was already finished in a batch.

A finished rename or playlist edit is undone from history. Cancel applies to work that is still running.

## Backups and leaving Jumpstart

Mass backs up all six parts of Jumpstart to another drive. See [Mass](../Mass/README.md).

You can export the library or move it to a new server without the apps. Export uses the original file when Jumpstart still has it, or a common format when it does not, plus readable sidecar files for playlists, progress, bookmarks, and similar details. The original server is left alone until you confirm the copy.

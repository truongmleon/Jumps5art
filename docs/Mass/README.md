# Mass

Mass is the file library and the backup tool. On the phone it works like a personal Google Drive. On a computer it can also appear as a drive in the file manager.

[Getting started](../Getting%20Started/README.md) covers pairing, offline changes, and deletion.

## Files and folders

Upload and download files, and make folders. Open a large file from the middle without downloading the whole library first. If the server is unreachable and the file is not fully on the device, Mass shows it as unavailable. It does not pretend an unfinished download is the real file.

Muse, Photogenic, Bato, and Flex files show up here as read-only. Change those in their own app. **Copy** makes a separate Mass file. Later edits to the copy and the original stay separate.

If two files would look like the same name on your computer, such as `Photo.jpg` and `photo.jpg`, Mass keeps both and shows names you can tell apart.

A save made while the server is offline stays **Pending Sync** until it is stored, even after you restart the app or the server. If someone, including you on another device, changes or deletes the server file first, both versions are kept.

## Download from the internet

Mass can fetch files on the server from:

- a normal web link
- Mega
- a torrent, with seeding set to none after the download finishes

Choose where a finished download should go: leave it in Mass, or add it to Muse, Photogenic, Bato, or Flex. You can set a default. The other app checks the file the way it usually does. A video sent to Flex is converted before it can be played there. The file stays in Mass until that app accepts it. The download's history remains in Mass.

Turn on **Require VPN for BitTorrent** when torrent traffic must use a VPN. Metadata, peers, downloading, and uploading then wait for that VPN. If it drops, torrent traffic stops. It does not continue on your normal connection.

A download is not finished until the file checks out. A login page, quota page, or error page is a failure, not a file. If the link changed, Mass keeps the job and starts the file over rather than gluing the new data onto the old partial download. Passwords, keys, and renewed links are asked for when the source needs them.

## Search

Search file names, details, and the text of supported documents. The search runs on your server. Files that cannot be read, or are not indexed yet, are listed that way. File contents are not sent to an outside AI service.

## Back up the server

Pick another drive as the backup for every Jumpstart library, including Worlds. One backup drive can hold several library drives, each in its own folder.

The backup includes your files, their organization, and the saved Minecraft world. It skips the backup drive itself, temporary caches, chapters that were only prefetched, and half-finished output. You can see the last successful backup and the last check.

- A missing, full, stale, or unreadable backup drive is labeled as such.
- A file that disappears because a drive failed is not treated as a delete. The last good backup is kept, and that library drive is marked as needing repair.
- Restore one file, one folder, or a whole drive onto a replacement disk.
- Before a restore replaces what you have now, Mass shows what will change and keeps a recovery copy until the restored data checks out.
- Removing a library drive archives its backup for 30 days. You can purge that archive sooner after you confirm.

## The computer drive

On macOS, Windows, and Linux you can mount the server as a normal drive. Previews can appear without downloading every original. Search tools on the computer are not allowed to pull down the whole library by scanning that drive.

Saves and deletes through the drive finish completely or not at all.

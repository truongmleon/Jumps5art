# Photogenic

Photogenic is the photo library. It works like a personal Google Photos library: a timeline of pictures and clips stored on your server, with backup from your phone.

[Getting started](../Getting%20Started/README.md) covers pairing, offline changes, and deletion.

## Add photos

Upload from any device, or turn on camera-roll backup on a phone.

Backup follows the Wi-Fi, mobile-data, and power choices you set. A photo counts as **Backed Up** only after the server has the phone's complete original. If the phone cannot read it yet, it stays **Waiting for Source**. If the copy fails or the photo changes mid-transfer, it shows **Needs Attention**.

- Full photo access is required to back up the whole camera roll. If you grant only selected photos, Photogenic backs up those and says access is limited. It does not claim the whole roll is backed up.
- Hidden or locked items are included only when you turn that choice on and the phone allows it.
- Deleting a photo in the phone's Photos app does not delete the server copy.
- Saving a Photogenic photo back to the phone does not upload it again as a new item.
- After you delete and reinstall the app, or replace the phone, unchanged photos link to the copies already on the server. They are not uploaded again. Edited ones are kept as related versions.
- Deleting the server photo stops automatic backup of that same camera-roll photo. Choose **Back Up Again** if you want it stored once more. A later edit on the phone is still treated as a new version.

A Live Photo is stored as one item: the still and the movie together. Backup, download, move, restore, and delete apply to both. If the phone also has a RAW, JPEG, depth, or edited rendition, those stay linked to the same asset.

## Browse and organize

The library is a grid and a timeline. There is no Create tab, and Photogenic does not make collages, animations, or memory prompts.

Albums are groups you create. Taking a photo out of an album, or deleting the album, does not delete the photo. Duplicate makes another copy.

Capture date, time, time zone, location, frame rate, and camera are kept when the file has them. If the time zone is missing, it stays **Unknown** until you set it. Date, time, and location corrections can be undone.

## Duplicates

Photogenic groups photos so you can review them. It never deletes a duplicate for you.

| Group | What it means |
|---|---|
| Exact duplicates | The same file bytes |
| Equivalent copies | The same picture stored differently |
| Related edits | Versions of one photo |
| Burst or similar | A burst or lookalikes |

You choose what to delete. Deleting one copy of a shared photo leaves the others.

## Edit

Crop, rotate, and mark up a photo. Those edits can be reverted to the original.

Editing a Live Photo makes a still and warns you that the motion is left behind. Revert brings back the still and the movie.

For a video clip, change speed, crop, or trim. **Save as New Video** is the default. **Replace Original** asks you first and keeps the old video in Recently Deleted for 30 days.

## Search, people, and generated images

Face grouping, search by what is in a photo, and optional image generation run on your server. Photos are not sent to a cloud AI service.

- Name a person, merge two people, or remove a face from a group. Those corrections stick, and a face you separated is not grouped again unless you undo the correction.
- Generate or edit with SDXL when you turn it on. The result is a new item, marked as AI-generated, and the photo you started from stays as it was.

## If backup looks stuck

Check the network and power settings, then whether photo access is limited. A photo still only in iCloud or Google Photos storage waits until the phone can read the original. The server copy is unchanged by a phone-only delete.

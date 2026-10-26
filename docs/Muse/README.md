# Muse

Muse is the music app. It works like a personal Apple Music library: a server collection you can stream, plus downloads and playlists on each device.

[Getting started](../Getting%20Started/README.md) covers pairing, offline changes, and deletion.

## Add music

- Add audio files you already have.
- Paste a link from YouTube, YouTube Music, or Spotify. Spotify links are matched to a YouTube Music recording using the title, main artist, version (live, remix, remaster), and length. A clear match imports on its own. Several uncertain songs collect in one review list so you can choose or skip them together.
- Search from a connected Spotify or YouTube account inside Muse. Opening the result in that service's app is optional.

Imports are saved as `.m4a`. Artwork and artist tags come with the file when the source has them. Lyrics, including timed lyrics, are added from LRCLIB when a match exists. Missing lyrics are fine, and the song still plays. You can correct the title, artists, album, artwork, track details, and lyrics. Automatic updates do not overwrite those corrections. A suggested lyric change waits under **Review Update**.

Importing a playlist creates a playlist with the same name and the same order. The songs also appear in the library. Importing that playlist again updates it: songs you added by hand stay, songs already downloaded are not downloaded again, and removals are shown before they happen. **Import as New** makes a separate playlist instead.

A failed song stays as a placeholder in its spot. Retry fills that spot. It does not swap in a different recording.

If the file is an exact copy of one you already have, Muse reuses it. If it sounds like the same recording with different silence or encoding, Muse asks you to **Keep Both** or **Use Existing**. You can decide several at once.

Before importing, confirm you have the right to add the music.

## Play

Play, pause, resume, seek, and stop. On iPhone, iPad, and Android, the lock screen and notification show the song, progress, and artwork.

Each device has its own player. Your phone and computer can play different songs. **Handoff** moves the current song, place, queue, shuffle, repeat, and upcoming tracks to another device that has the song or can reach the server. The first device keeps playing until the other one actually starts. If handoff fails, the first device keeps going.

After a handoff, your other devices can control that player. A late command stays aimed at the player you sent it to.

## Playlists

Playlists are labeled **Server** or **This Device**.

- **Server** playlists sync. A downloaded copy on a device follows the server playlist.
- **This Device** playlists stay on that device.

Artists and Albums are automatic views of your songs. They are not playlists. A playlist's cover can be an image you upload or art from up to four songs.

Search and sort songs by name, last played, and artist. Search and sort playlists by name and last played.

Removing a song from a playlist, or deleting the playlist, leaves the audio in the library. Deleting the library song is a separate step and tells you which playlists used it.

## Offline

Download a song or playlist when you want it without the server. A server playlist updates its order right away, then fetches missing audio according to that device's network and storage settings. Songs already on the device keep playing.

In settings, **Keep local download after library deletion** keeps a downloaded song on that device after it is removed from the server library. Otherwise the download is removed when the deletion arrives.

## If a music service is down

YouTube, YouTube Music, Spotify, or LRCLIB can be unavailable or can ask you to sign in again. Songs and lyrics already in the library stay. New search and downloads wait, and unfinished playlist tracks stay unresolved until you retry them.

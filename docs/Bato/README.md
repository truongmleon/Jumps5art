# Bato

Bato is the reading app. It works like a personal Apple Books library for PDF, EPUB, and CBZ manga.

[Getting started](../Getting%20Started/README.md) covers pairing, offline changes, and deletion.

## Add books and chapters

Upload a PDF, EPUB, or CBZ. Bato checks the file before it enters the library. A damaged file is identified. A password-protected file asks for the password. Bato does not remove DRM or guess a password.

For manga, use the MangaDex downloader, or a similar source, when you want chapters fetched for you. Each downloaded chapter is its own `.cbz`, with its cover and details. Pages are ordered so page 2 comes before page 10. Another release of the same chapter, such as a different group or language, stays separate until you add it to your reading order.

A book is not marked fully available offline while part of it is still missing. The missing part is named.

## The library

The sidebar is **All**, **Want to Read**, **Finished**, **Collections**, and the collections you create. If a book has no cover, the title and author are shown instead.

You can mark a book or series **Finished** yourself. That stays until you change it. A book also finishes on its own when your saved place reaches the end through normal reading, or when you choose **Continue Reading from Here** at the end. A series finishes on its own only when every chapter in the selected order is done and no chapter is missing. A new required chapter puts an automatically finished series back in progress. A series you marked finished yourself stays finished.

Gaps and replaced chapters are flagged.

## Read

Zoom, scroll, bookmark, and annotate. PDF and CBZ progress is the current page out of the readable pages. EPUB progress follows your place in the book's reading order. EPUB places and notes use a stable anchor so they can survive reflow.

For manga you can turn on right-to-left pages, vertical webtoon scrolling, and automatic two-page spreads. Wide images fit, spread, or split based on the reading mode. You can set the reading direction, how pages pair, and whether the first page stands alone. Those choices are remembered per book.

Text in an EPUB or a text PDF can be selected. Image-only PDF and CBZ pages can be recognized so you can select and copy the text.

If a sync replaces the chapter you have open, you keep reading the open copy for that session. The new copy is used the next time you open it.

## Your place in the book

Bato keeps two positions:

- **Saved Reading Progress** is the place you return to.
- **Current Viewing Location** is wherever you are looking right now.

Ordinary page turns and scrolling update the saved place, including when you deliberately read backward. Search, the table of contents, a bookmark, a chapter list, an in-book link, or the page scrubber are temporary browsing. They do not move the saved place. **Return to [saved location]** takes you back. **Continue Reading from Here** makes the page you are viewing the saved place, including the end of the book.

Closing the book during a look-around keeps the saved place. A bookmark or highlight does not move it.

**Re-read** starts again from the beginning, clears the current Finished mark, and keeps the record that you finished before.

Saved progress syncs. If this device was offline and the saved place changed elsewhere, the server place stays, and Bato offers **Continue from this device's position**.

## Downloads and the next chapter

Download a book or chapter when you want it offline. Around 80% of the way through a chapter, Bato may fetch the next chapter in your selected order onto the device you are reading on. That prefetch is labeled differently from a download you asked for. Downloading a chapter that was already prefetched reuses that copy.

If space is low, a closed prefetched chapter can be removed when it can be fetched again. Your place, bookmarks, and notes stay. A download you asked for is not removed automatically. If the same chapter cannot be fetched again, the saved place is kept and marked unresolved rather than attached to a different release.

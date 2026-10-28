# Earlier types the new features build on

[Back to the type guide](README.md)

The job, retry, and storage types use a smaller set of shared IDs, checked numbers, and messages. This reference explains their purpose so reading a field's type does not require guessing from its name.

## IDs answer “which one?”

A file name or display title can change or repeat. Jumpstart therefore uses IDs for records and a separate fingerprint for bytes. For example, renaming a song keeps its `ItemId`; replacing its audio creates a distinguishable version with a fingerprint for the new contents.

| Type | What it represents | Why it has its own type |
|---|---|---|
| `ServerId` | The server authority the library belongs to. | A client must distinguish its server from a different installation even if paths or display names match. |
| `SyncEpoch` | The server's particular synchronization history. | Restoring older data can invalidate old change positions while retaining the server's identity. |
| `DeviceId` | A device identity, intended to be bound to an approved key. | Two devices can share a display name. An ID keeps their records distinct; the ID by itself does not authenticate a device. |
| `ClientInstanceId` | One app's durable local database. | Reinstalling an app can restart command numbering. A fresh instance ID separates the new commands from those of the lost database. |
| `ItemId` | One logical library record, such as a song. | Renaming or editing the song should preserve references to that library item. |
| `VersionId` | One immutable version of an item. | Replacing contents needs a distinguishable version while retaining the item's identity and earlier versions. |
| `ResourceId` | One component resource record, such as the original image or motion-video component of a photo. | A version can contain several components with distinct roles. A fingerprint alone does not describe those relationships. |
| `JobId` | One long-running user operation. | Worker runs can change within the same job. A terminal job's retry or retained-data resume uses a linked successor job. `JobLifecycle` does not allocate or store this ID. |

All eight types wrap UUIDs, a standard ID format, but they remain distinct Rust types. A function requiring an `ItemId` will not accept a `DeviceId` merely because both contain a UUID. This catches a category of mix-ups before the program runs.

Parsing checks the required UUID shape/version. It does not prove the corresponding database record exists, that a device was approved, or that a caller may access it.

Source: [generated identity types](../../crates/jumpstart-domain/src/identity.rs).

## Bytes, numbers, command identity, and ownership

| Type | What it is for | Why we need it |
|---|---|---|
| `BlobId` | A fingerprint calculated from exact file bytes. | Two names may contain the same bytes, or the same name may later contain different bytes. The fingerprint identifies expected contents independently of the name. It does not store the bytes or prove a usable copy is present. |
| `Counter` | An exact nonnegative whole number in the supported signed-64-bit range, such as a byte count or token counter. | Validation rejects unsupported values. JSON decimal strings preserve large integers exactly for consumers that cannot represent every 64-bit integer as a numeric value. It does not increment itself. |
| `PositiveCounter` | The corresponding checked number type with a minimum of one. | Command sequences and revisions that cannot validly be zero reject zero at their boundary. |
| `OperationId` | A device ID, app-database instance ID, and positive command sequence together. | Future receipt handling needs to recognize a retransmitted command. Device A's command 1 and device B's command 1 must remain different. The type identifies commands; duplicate-effect prevention requires storage and handler code. |
| `Owner` | Which app manages an item: Muse, Photogenic, Mass, Bato, or Flex. | Callers choose from the supported app names rather than interpret arbitrary labels. It represents catalog ownership, not a user's account or access-control decision. |
| `DomainError` | Why a proposed ID, counter, or fingerprint failed validation. | Callers can distinguish malformed text, a wrong UUID version, or an out-of-range number without interpreting an error sentence. |

“Blob” means an uninterpreted set of bytes here: a song, photo, PDF, video, or copied SQLite database can all have a `BlobId`. The library record, its version, its component resources, and their actual bytes have different identities because they answer different questions.

Source: [blob fingerprints](../../crates/jumpstart-domain/src/blob.rs), [counters](../../crates/jumpstart-domain/src/counter.rs), and [operations, owners, and validation errors](../../crates/jumpstart-domain/src/lib.rs).

## Messages describe an app/server conversation

An app offers communication rules in `Hello`; the server selects shared rules in `Welcome`. These types describe and validate that exchange. The current library does not open or authenticate a network connection.

| Type | What it is for | Why we need it |
|---|---|---|
| `Capability` | A checked name for a communication feature, such as `control-json.v1`. | Negotiation needs bounded, consistently formatted names it can compare. A capability name does not establish product health or permissions. |
| `Hello` | The app's offered version, local database identity, and supported/required features. | The fields must agree; for example, a required feature must also be listed as supported. Validating the complete message catches that inconsistency. |
| `ServerHandshake` | Local server identity, features, limits, and change-feed bounds used to prepare a reply. | The server's own facts must be supplied separately from client input. The caller is responsible for providing trustworthy local values. |
| `Welcome` | The server's proposed communication agreement. | The app must check the actual selection against the `Hello` it sent. A well-formed selection could still include something the app never offered. |
| `ProtocolLimits` | Message size, nesting, request, stream, page, and deadline budgets advertised to a peer. | Both sides need explicit budgets, and invalid values must be rejected. Defining a budget does not implement the future service that enforces it. |
| `HandshakeValidationError` | A malformed field or inconsistent message. | Invalid input needs to be distinguished from valid messages that cannot agree. |
| `NegotiationError` | Why peers cannot agree, such as mismatched major versions or a missing required feature. | The caller needs a specific compatibility outcome to explain or handle. |
| `ErrorCode` | A fixed category such as `waiting_for_storage` or `resync_required`. | Software needs stable categories for its response instead of guessing from human-readable wording. |
| `ProtocolError` | A failure message carrying a category, explanation, required state text, and retry guidance. | The human explanation and the machine-readable handling information need to travel together with consistent retry fields. |
| `CodecError` | A problem reading, checking, encoding, or writing a framed JSON message. | Message-format and byte-stream errors must remain distinguishable from a failure of the requested library action. |

Source: [handshake and error messages](../../crates/jumpstart-protocol/src/handshake.rs) and [message encoding/decoding errors](../../crates/jumpstart-protocol/src/codec.rs).

### What “codec,” “framing,” and “finite” mean here

**Codec** means message encoder/decoder in this crate. It converts app/server messages to and from JSON bytes. The video encoder planned for Flex is a separate component.

**Framing** marks where each message begins and ends in a sequence of bytes. For example, if an app sends `Hello` followed by another JSON message, a length prefix lets a reader know how many bytes belong to the first one. The functions accept a supplied reader/writer; tests use memory buffers. They do not open a connection themselves or transfer raw media files.

**Finite checking** examines outgoing Rust values before JSON encoding, including whether floating-point values are finite. It rejects NaN or infinity instead of letting them become a misleading JSON `null`. Incoming JSON is checked by the decoder.

## Private parsing helpers

These are implementation tools rather than new product concepts. They exist so public messages can be bounded and validated before the rest of the application sees them.

| Type | Purpose and reason |
|---|---|
| `HelloWire`, `LimitsWire`, `WelcomeWire`, `ErrorWire` | Temporary receiving shapes. Serde, the serialization library, first reads fields into these values; conversion then checks relationships between the fields before returning the public type. This keeps decoding from bypassing those checks. |
| `BoundedBuffer` | Collects encoded JSON while refusing to grow past the message limit. The complete body is checked before a frame is written. |
| `JsonSeed` | Carries the current nesting depth into recursive parsing steps, so each nested container contributes to the same depth limit. |
| `JsonVisitor` | Builds incoming JSON while checking depth and duplicate object keys, including keys inside fields a later typed decoder may ignore. Duplicate keys could otherwise give the same message ambiguous meanings. |
| `FiniteCheck` | Walks outgoing Rust values without producing encoded bytes. It checks nonfinite numbers and preliminary size/depth budgets before encoding starts. |
| `PreflightError` | Records why that preliminary walk stopped. The codec translates it into the public `CodecError` that its callers handle. |

Source: [wire receiving shapes](../../crates/jumpstart-protocol/src/handshake.rs), [bounded parsing](../../crates/jumpstart-protocol/src/codec.rs), and [outgoing-value checking](../../crates/jumpstart-protocol/src/finite.rs).

## Types that only support tests

`EntryState` in the snapshot tests records a filesystem entry's contents and metadata so the test can compare before and after capture. The earlier metadata tests use `FileState` for a similar comparison. They establish that existing data stayed unchanged; they are not part of the library's stored schema.

The framing tests define small temporary types such as `ShortReader`, `HeaderOnly`, `Duplicates`, `Many`, and `Broken` to simulate short reads, forbidden reads, duplicate keys, excessive output, and failed writes. `Nested` supplies a serializable nested value. Keeping these simulations in tests makes the relevant failure reproducible without a real network peer.

Source: [snapshot tests](../../crates/jumpstart-storage/tests/snapshots.rs), [metadata tests](../../crates/jumpstart-storage/tests/metadata.rs), and [framing tests](../../crates/jumpstart-protocol/tests/framing.rs).

## Supporting types from libraries

Names such as `Result`, `Option`, `Vec`, `Path`, `PathBuf`, and `Duration` come from Rust's libraries. They are not additional Jumpstart product concepts. `Result<T, E>` holds either a successful `T` or an error `E`; `Option<T>` holds a `T` or an explicit absence. For example, the snapshot's `Option<Counter>` makes an unavailable change position distinguishable from an actual number.

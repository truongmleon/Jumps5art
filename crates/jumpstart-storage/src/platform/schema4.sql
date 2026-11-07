-- The shared server catalog holds app metadata and references to immutable bytes.
-- For example, a Photogenic still and motion clip share one version, while a
-- Mass copy can keep separate ownership and reuse the same verified blob.

CREATE TABLE volumes (
    volume_id BLOB PRIMARY KEY CHECK (length(volume_id) = 16),
    role TEXT NOT NULL CHECK (role IN ('primary', 'backup')),
    primary_id BLOB UNIQUE CHECK (primary_id IS NULL OR length(primary_id) = 16),
    filesystem_identity TEXT NOT NULL CHECK (length(filesystem_identity) > 0),
    health TEXT NOT NULL CHECK (health IN ('unknown', 'healthy', 'degraded')),
    reserve_bytes INTEGER CHECK (reserve_bytes >= 0),
    CHECK ((role = 'primary') = (primary_id IS NOT NULL))
) STRICT;

CREATE TABLE primary_policies (
    owner TEXT PRIMARY KEY CHECK (owner IN ('muse', 'photogenic', 'mass', 'bato', 'flex')),
    primary_id BLOB NOT NULL REFERENCES volumes(primary_id)
) STRICT;

CREATE INDEX primary_policies_primary ON primary_policies(primary_id);

CREATE TABLE items (
    item_id BLOB PRIMARY KEY CHECK (length(item_id) = 16),
    incarnation BLOB NOT NULL CHECK (length(incarnation) = 16),
    owner TEXT NOT NULL CHECK (owner IN ('muse', 'photogenic', 'mass', 'bato', 'flex')),
    kind TEXT NOT NULL CHECK (length(kind) > 0),
    home_primary_id BLOB NOT NULL REFERENCES volumes(primary_id),
    parent_id BLOB,
    name TEXT NOT NULL CHECK (length(name) > 0),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('live', 'recently_deleted')),
    deleted_at_ms INTEGER,
    current_version_id BLOB,
    revision INTEGER NOT NULL CHECK (revision > 0),
    UNIQUE (item_id, owner),
    CHECK ((lifecycle = 'recently_deleted') = (deleted_at_ms IS NOT NULL)),
    CHECK (parent_id IS NULL OR parent_id != item_id),
    CHECK (owner != 'mass' OR (
        instr(name, char(0)) = 0 AND instr(name, '/') = 0
        AND name NOT IN ('.', '..') AND length(CAST(name AS BLOB)) <= 255
    )),
    FOREIGN KEY (parent_id, owner) REFERENCES items(item_id, owner) DEFERRABLE INITIALLY DEFERRED,
    FOREIGN KEY (item_id, current_version_id) REFERENCES versions(item_id, version_id) DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE INDEX items_home_primary ON items(home_primary_id);

CREATE INDEX items_parent_owner ON items(parent_id, owner);

CREATE INDEX items_owner_lifecycle_parent_name ON items(owner, lifecycle, parent_id, name);

CREATE UNIQUE INDEX mass_live_sibling_names ON items(coalesce(parent_id, X''), name)
    WHERE owner = 'mass' AND lifecycle = 'live';

CREATE TABLE versions (
    version_id BLOB PRIMARY KEY CHECK (length(version_id) = 16),
    item_id BLOB NOT NULL REFERENCES items(item_id),
    created_seq INTEGER NOT NULL CHECK (created_seq > 0),
    manifest_version INTEGER NOT NULL CHECK (manifest_version > 0),
    manifest_algorithm TEXT NOT NULL CHECK (manifest_algorithm = 'b3'),
    manifest_digest BLOB NOT NULL CHECK (length(manifest_digest) = 32),
    UNIQUE (item_id, version_id)
) STRICT;

CREATE INDEX versions_item_sequence ON versions(item_id, created_seq);

CREATE TABLE blobs (
    algorithm TEXT NOT NULL CHECK (algorithm = 'b3'),
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    PRIMARY KEY (algorithm, digest)
) STRICT;

CREATE TABLE version_resources (
    version_id BLOB NOT NULL REFERENCES versions(version_id),
    resource_id BLOB NOT NULL CHECK (length(resource_id) = 16),
    role TEXT NOT NULL CHECK (length(role) > 0),
    original_filename TEXT NOT NULL CHECK (length(original_filename) > 0),
    media_type TEXT,
    algorithm TEXT NOT NULL,
    digest BLOB NOT NULL,
    required INTEGER NOT NULL CHECK (required IN (0, 1)),
    PRIMARY KEY (version_id, resource_id),
    FOREIGN KEY (algorithm, digest) REFERENCES blobs(algorithm, digest)
) STRICT;

CREATE INDEX version_resources_blob ON version_resources(algorithm, digest);

CREATE TABLE blob_placements (
    algorithm TEXT NOT NULL,
    digest BLOB NOT NULL,
    primary_id BLOB NOT NULL REFERENCES volumes(primary_id),
    state TEXT NOT NULL CHECK (state IN ('verified', 'quarantined', 'missing')),
    verified_at_ms INTEGER,
    PRIMARY KEY (algorithm, digest, primary_id),
    FOREIGN KEY (algorithm, digest) REFERENCES blobs(algorithm, digest),
    CHECK (state != 'verified' OR verified_at_ms IS NOT NULL)
) STRICT;

CREATE INDEX blob_placements_primary_state ON blob_placements(primary_id, state);

CREATE TABLE collections (
    collection_id BLOB PRIMARY KEY CHECK (length(collection_id) = 16),
    owner TEXT NOT NULL CHECK (owner IN ('muse', 'photogenic', 'mass', 'bato', 'flex')),
    name TEXT NOT NULL CHECK (length(name) > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    UNIQUE (collection_id, owner)
) STRICT;

CREATE TABLE collection_entries (
    entry_id BLOB PRIMARY KEY CHECK (length(entry_id) = 16),
    collection_id BLOB NOT NULL,
    owner TEXT NOT NULL,
    item_id BLOB,
    position INTEGER NOT NULL CHECK (position >= 0),
    UNIQUE (collection_id, position),
    FOREIGN KEY (collection_id, owner) REFERENCES collections(collection_id, owner),
    FOREIGN KEY (item_id, owner) REFERENCES items(item_id, owner)
) STRICT;

CREATE INDEX collection_entries_collection_owner ON collection_entries(collection_id, owner);

CREATE INDEX collection_entries_item_owner ON collection_entries(item_id, owner);

CREATE TABLE entity_fields (
    item_id BLOB NOT NULL REFERENCES items(item_id),
    field TEXT NOT NULL CHECK (length(field) > 0),
    value_schema_version INTEGER NOT NULL CHECK (value_schema_version > 0),
    value_json TEXT NOT NULL CHECK (length(CAST(value_json AS BLOB)) <= 1048576 AND json_valid(value_json)),
    provenance TEXT NOT NULL CHECK (length(provenance) > 0),
    provider_revision TEXT,
    manual_lock INTEGER NOT NULL CHECK (manual_lock IN (0, 1)),
    revision INTEGER NOT NULL CHECK (revision > 0),
    PRIMARY KEY (item_id, field)
) STRICT;

CREATE TABLE progress (
    item_id BLOB PRIMARY KEY,
    version_id BLOB NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    anchor_schema_version INTEGER NOT NULL CHECK (anchor_schema_version > 0),
    anchor_json TEXT NOT NULL CHECK (length(CAST(anchor_json AS BLOB)) <= 1048576 AND json_valid(anchor_json)),
    FOREIGN KEY (item_id, version_id) REFERENCES versions(item_id, version_id)
) STRICT;

CREATE TABLE completion_events (
    event_id BLOB PRIMARY KEY CHECK (length(event_id) = 16),
    item_id BLOB NOT NULL,
    version_id BLOB NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    origin TEXT NOT NULL CHECK (origin IN ('manual', 'automatic')),
    completed INTEGER NOT NULL CHECK (completed IN (0, 1)),
    revision INTEGER NOT NULL CHECK (revision > 0),
    recorded_at_ms INTEGER NOT NULL,
    FOREIGN KEY (item_id, version_id) REFERENCES versions(item_id, version_id)
) STRICT;

CREATE INDEX completion_events_item_version ON completion_events(item_id, version_id);

CREATE TABLE annotations (
    annotation_id BLOB PRIMARY KEY CHECK (length(annotation_id) = 16),
    item_id BLOB NOT NULL,
    version_id BLOB NOT NULL,
    anchor_schema_version INTEGER NOT NULL CHECK (anchor_schema_version > 0),
    anchor_json TEXT NOT NULL CHECK (length(CAST(anchor_json AS BLOB)) <= 1048576 AND json_valid(anchor_json)),
    body TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    FOREIGN KEY (item_id, version_id) REFERENCES versions(item_id, version_id)
) STRICT;

CREATE INDEX annotations_item_version ON annotations(item_id, version_id);

CREATE TABLE bookmarks (
    bookmark_id BLOB PRIMARY KEY CHECK (length(bookmark_id) = 16),
    item_id BLOB NOT NULL,
    version_id BLOB NOT NULL,
    anchor_schema_version INTEGER NOT NULL CHECK (anchor_schema_version > 0),
    anchor_json TEXT NOT NULL CHECK (length(CAST(anchor_json AS BLOB)) <= 1048576 AND json_valid(anchor_json)),
    revision INTEGER NOT NULL CHECK (revision > 0),
    FOREIGN KEY (item_id, version_id) REFERENCES versions(item_id, version_id)
) STRICT;

CREATE INDEX bookmarks_item_version ON bookmarks(item_id, version_id);

CREATE TABLE music_track (
    item_id BLOB PRIMARY KEY,
    owner TEXT NOT NULL CHECK (owner = 'muse'),
    version_id BLOB NOT NULL,
    duration_ms INTEGER NOT NULL CHECK (duration_ms > 0),
    codec TEXT NOT NULL CHECK (length(codec) > 0),
    metadata_revision INTEGER NOT NULL CHECK (metadata_revision > 0),
    source_algorithm TEXT NOT NULL CHECK (source_algorithm = 'b3'),
    source_digest BLOB NOT NULL CHECK (length(source_digest) = 32),
    FOREIGN KEY (item_id, owner) REFERENCES items(item_id, owner),
    FOREIGN KEY (item_id, version_id) REFERENCES versions(item_id, version_id)
) STRICT;

CREATE TABLE chapter_releases (
    release_id BLOB PRIMARY KEY CHECK (length(release_id) = 16),
    item_id BLOB NOT NULL,
    owner TEXT NOT NULL CHECK (owner = 'bato'),
    version_id BLOB NOT NULL,
    provider TEXT NOT NULL CHECK (length(provider) > 0),
    provider_series_id TEXT NOT NULL CHECK (length(provider_series_id) > 0),
    provider_release_id TEXT NOT NULL CHECK (length(provider_release_id) > 0),
    language TEXT NOT NULL CHECK (length(language) > 0),
    edition TEXT,
    volume_label TEXT,
    chapter_label TEXT NOT NULL CHECK (length(chapter_label) > 0),
    UNIQUE (provider, provider_release_id),
    FOREIGN KEY (item_id, owner) REFERENCES items(item_id, owner),
    FOREIGN KEY (item_id, version_id) REFERENCES versions(item_id, version_id)
) STRICT;

CREATE INDEX chapter_releases_item_version ON chapter_releases(item_id, version_id);

CREATE TABLE media_marker (
    item_id BLOB NOT NULL,
    owner TEXT NOT NULL CHECK (owner = 'flex'),
    version_id BLOB NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('opening', 'ending')),
    start_ms INTEGER NOT NULL CHECK (start_ms >= 0),
    end_ms INTEGER NOT NULL CHECK (end_ms > start_ms),
    provenance TEXT NOT NULL CHECK (length(provenance) > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    disabled INTEGER NOT NULL CHECK (disabled IN (0, 1)),
    dismissed INTEGER NOT NULL CHECK (dismissed IN (0, 1)),
    PRIMARY KEY (item_id, version_id, kind),
    FOREIGN KEY (item_id, owner) REFERENCES items(item_id, owner),
    FOREIGN KEY (item_id, version_id) REFERENCES versions(item_id, version_id)
) STRICT;

CREATE TABLE worlds_backup_catalog (
    point_id BLOB PRIMARY KEY CHECK (length(point_id) = 16),
    volume_id BLOB NOT NULL REFERENCES volumes(volume_id),
    manifest_version INTEGER NOT NULL CHECK (manifest_version > 0),
    manifest_algorithm TEXT NOT NULL CHECK (manifest_algorithm = 'b3'),
    manifest_digest BLOB NOT NULL CHECK (length(manifest_digest) = 32),
    captured_at_ms INTEGER NOT NULL,
    verified_at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX worlds_backup_catalog_volume ON worlds_backup_catalog(volume_id);

CREATE TABLE deletion_tombstones (
    item_id BLOB NOT NULL CHECK (length(item_id) = 16),
    delete_token BLOB NOT NULL CHECK (length(delete_token) = 16),
    delete_seq INTEGER NOT NULL CHECK (delete_seq > 0),
    PRIMARY KEY (item_id, delete_token)
) STRICT;

CREATE TABLE camera_suppression (
    manifest_version INTEGER NOT NULL CHECK (manifest_version > 0),
    manifest_algorithm TEXT NOT NULL CHECK (manifest_algorithm = 'b3'),
    manifest_digest BLOB NOT NULL CHECK (length(manifest_digest) = 32),
    item_id BLOB NOT NULL,
    delete_token BLOB NOT NULL,
    PRIMARY KEY (manifest_version, manifest_algorithm, manifest_digest, item_id, delete_token),
    FOREIGN KEY (item_id, delete_token) REFERENCES deletion_tombstones(item_id, delete_token)
) STRICT;

CREATE INDEX camera_suppression_delete ON camera_suppression(item_id, delete_token);

CREATE TABLE camera_suppression_resources (
    manifest_version INTEGER NOT NULL,
    manifest_algorithm TEXT NOT NULL,
    manifest_digest BLOB NOT NULL,
    item_id BLOB NOT NULL,
    delete_token BLOB NOT NULL,
    role TEXT NOT NULL CHECK (length(role) > 0),
    resource_algorithm TEXT NOT NULL CHECK (resource_algorithm = 'b3'),
    resource_digest BLOB NOT NULL CHECK (length(resource_digest) = 32),
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    multiplicity INTEGER NOT NULL CHECK (multiplicity > 0),
    PRIMARY KEY (manifest_version, manifest_algorithm, manifest_digest, item_id, delete_token, role, resource_algorithm, resource_digest, byte_length),
    FOREIGN KEY (manifest_version, manifest_algorithm, manifest_digest, item_id, delete_token)
        REFERENCES camera_suppression(manifest_version, manifest_algorithm, manifest_digest, item_id, delete_token)
) STRICT;

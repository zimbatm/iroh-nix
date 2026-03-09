-- Narinfo metadata table for D1
-- Mirrors the nix-store HashIndex schema for the smart protocol
CREATE TABLE IF NOT EXISTS narinfo (
    store_hash TEXT PRIMARY KEY,
    store_path TEXT UNIQUE NOT NULL,
    nar_hash_hex TEXT NOT NULL,
    nar_size INTEGER NOT NULL,
    references_basenames TEXT DEFAULT '',
    deriver TEXT,
    signatures TEXT DEFAULT '',
    blake3_hex TEXT
);

CREATE INDEX IF NOT EXISTS idx_store_path ON narinfo(store_path);

-- Pending upload sessions for atomic narinfo commits
-- Narinfos are staged here during upload, then atomically moved to narinfo table
CREATE TABLE IF NOT EXISTS pending_upload (
    session_id TEXT NOT NULL,
    store_hash TEXT NOT NULL,
    store_path TEXT NOT NULL,
    nar_hash_hex TEXT NOT NULL,
    nar_size INTEGER NOT NULL,
    references_basenames TEXT DEFAULT '',
    deriver TEXT,
    signatures TEXT DEFAULT '',
    blake3_hex TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (session_id, store_hash)
);

-- Clean up stale pending uploads (older than 1 hour)
-- Run periodically via Cron Trigger or before each commit
CREATE INDEX IF NOT EXISTS idx_pending_created ON pending_upload(created_at);

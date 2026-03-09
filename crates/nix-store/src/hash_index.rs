//! HashIndex: Bidirectional mapping between BLAKE3 and SHA256 hashes
//!
//! Nix uses SHA256 for content addressing, while iroh uses BLAKE3.
//! This module provides hash types (always available) and a SQLite-backed
//! index for translating between them (requires "sqlite" feature).

use crate::{Error, Result};

/// A 32-byte BLAKE3 hash
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Blake3Hash(pub [u8; 32]);

impl Blake3Hash {
    /// Create from a slice (must be exactly 32 bytes)
    pub fn from_slice(slice: &[u8]) -> Result<Self> {
        let arr: [u8; 32] = slice
            .try_into()
            .map_err(|_| Error::Protocol("invalid blake3 hash length".into()))?;
        Ok(Self(arr))
    }

    /// Get as bytes
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to hex string
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse from hex string
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s).map_err(|e| Error::Protocol(format!("invalid hex: {}", e)))?;
        Self::from_slice(&bytes)
    }
}

impl From<blake3::Hash> for Blake3Hash {
    fn from(h: blake3::Hash) -> Self {
        Self(*h.as_bytes())
    }
}

impl std::fmt::Display for Blake3Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

/// A 32-byte SHA256 hash
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Hash(pub [u8; 32]);

impl Sha256Hash {
    /// Create from a slice (must be exactly 32 bytes)
    pub fn from_slice(slice: &[u8]) -> Result<Self> {
        let arr: [u8; 32] = slice
            .try_into()
            .map_err(|_| Error::Protocol("invalid sha256 hash length".into()))?;
        Ok(Self(arr))
    }

    /// Get as bytes
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to hex string
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parse from hex string
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s).map_err(|e| Error::Protocol(format!("invalid hex: {}", e)))?;
        Self::from_slice(&bytes)
    }

    /// Convert to Nix base32 format (used in store paths)
    pub fn to_nix_base32(&self) -> String {
        crate::encoding::encode_nix_base32(&self.0)
    }
}

impl std::fmt::Display for Sha256Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

// ============================================================================
// SQLite-backed index (requires "sqlite" feature)
// ============================================================================

#[cfg(feature = "sqlite")]
use rusqlite::{params, Connection, OptionalExtension};
#[cfg(feature = "sqlite")]
use std::path::Path;

/// Entry in the hash index
#[cfg(feature = "sqlite")]
#[derive(Debug, Clone)]
pub struct HashEntry {
    /// BLAKE3 hash of the NAR content
    pub blake3: Blake3Hash,
    /// SHA256 hash of the NAR content (used by Nix)
    pub sha256: Sha256Hash,
    /// Full store path (e.g., /nix/store/abc123...-name)
    pub store_path: String,
    /// Size of the NAR in bytes
    pub nar_size: u64,
    /// Runtime dependencies (full store paths)
    pub references: Vec<String>,
    /// Deriver path (optional, full store path to .drv)
    pub deriver: Option<String>,
}

/// SQLite-backed index for hash translation
#[cfg(feature = "sqlite")]
pub struct HashIndex {
    conn: Connection,
}

#[cfg(feature = "sqlite")]
impl HashIndex {
    /// Open or create a hash index at the given path
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        let index = Self { conn };
        index.init_schema()?;
        Ok(index)
    }

    /// Create an in-memory hash index (for testing)
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let index = Self { conn };
        index.init_schema()?;
        Ok(index)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS hash_index (
                blake3 BLOB PRIMARY KEY,
                sha256 BLOB UNIQUE NOT NULL,
                store_path TEXT UNIQUE NOT NULL,
                nar_size INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_sha256 ON hash_index(sha256);
            CREATE INDEX IF NOT EXISTS idx_store_path ON hash_index(store_path);
            "#,
        )?;
        self.migrate()?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        // Add store_hash column if it doesn't exist (migration from older schema)
        let has_store_hash: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('hash_index') WHERE name = 'store_hash'",
            [],
            |row| row.get(0),
        )?;

        if !has_store_hash {
            self.conn.execute_batch(
                r#"
                ALTER TABLE hash_index ADD COLUMN store_hash TEXT;
                CREATE INDEX IF NOT EXISTS idx_store_hash ON hash_index(store_hash);
                "#,
            )?;

            // Backfill store_hash from existing store_path values
            // Extract hash from /nix/store/<hash>-name
            let mut stmt = self
                .conn
                .prepare("SELECT rowid, store_path FROM hash_index WHERE store_hash IS NULL")?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect();

            let mut update_stmt = self
                .conn
                .prepare("UPDATE hash_index SET store_hash = ?1 WHERE rowid = ?2")?;
            for (rowid, store_path) in rows {
                if let Some(hash) = extract_store_hash(&store_path) {
                    update_stmt.execute(params![hash, rowid])?;
                }
            }
        }

        // Add references column (space-separated store path basenames)
        let has_references: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('hash_index') WHERE name = 'references'",
            [],
            |row| row.get(0),
        )?;

        if !has_references {
            self.conn.execute_batch(
                r#"
                ALTER TABLE hash_index ADD COLUMN "references" TEXT DEFAULT '';
                ALTER TABLE hash_index ADD COLUMN deriver TEXT;
                "#,
            )?;
        }

        Ok(())
    }

    /// Insert a new entry into the index
    pub fn insert(&self, entry: &HashEntry) -> Result<()> {
        let store_hash = extract_store_hash(&entry.store_path);
        // Store references as space-separated basenames (matching narinfo format)
        let references_str = references_to_basenames(&entry.references).join(" ");
        let deriver_str = entry.deriver.as_deref().and_then(store_path_basename);
        self.conn.execute(
            "INSERT OR REPLACE INTO hash_index (blake3, sha256, store_path, nar_size, store_hash, \"references\", deriver) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.blake3.as_bytes().as_slice(),
                entry.sha256.as_bytes().as_slice(),
                &entry.store_path,
                entry.nar_size as i64,
                store_hash,
                references_str,
                deriver_str,
            ],
        )?;
        Ok(())
    }

    /// Look up an entry by BLAKE3 hash
    pub fn get_by_blake3(&self, blake3: &Blake3Hash) -> Result<Option<HashEntry>> {
        let result = self
            .conn
            .query_row(
                "SELECT blake3, sha256, store_path, nar_size, \"references\", deriver FROM hash_index WHERE blake3 = ?1",
                params![blake3.as_bytes().as_slice()],
                row_to_entry_tuple,
            )
            .optional()?;

        optional_tuple_to_entry(result)
    }

    /// Look up an entry by SHA256 hash
    pub fn get_by_sha256(&self, sha256: &Sha256Hash) -> Result<Option<HashEntry>> {
        let result = self
            .conn
            .query_row(
                "SELECT blake3, sha256, store_path, nar_size, \"references\", deriver FROM hash_index WHERE sha256 = ?1",
                params![sha256.as_bytes().as_slice()],
                row_to_entry_tuple,
            )
            .optional()?;

        optional_tuple_to_entry(result)
    }

    /// Look up an entry by store path
    pub fn get_by_store_path(&self, store_path: &str) -> Result<Option<HashEntry>> {
        let result = self
            .conn
            .query_row(
                "SELECT blake3, sha256, store_path, nar_size, \"references\", deriver FROM hash_index WHERE store_path = ?1",
                params![store_path],
                row_to_entry_tuple,
            )
            .optional()?;

        optional_tuple_to_entry(result)
    }

    /// Look up an entry by nix store hash (the hash prefix from /nix/store/<hash>-name)
    pub fn get_by_store_hash(&self, store_hash: &str) -> Result<Option<HashEntry>> {
        let result = self
            .conn
            .query_row(
                "SELECT blake3, sha256, store_path, nar_size, \"references\", deriver FROM hash_index WHERE store_hash = ?1",
                params![store_hash],
                row_to_entry_tuple,
            )
            .optional()?;

        optional_tuple_to_entry(result)
    }

    /// Delete an entry by BLAKE3 hash
    pub fn delete_by_blake3(&self, blake3: &Blake3Hash) -> Result<bool> {
        let rows = self.conn.execute(
            "DELETE FROM hash_index WHERE blake3 = ?1",
            params![blake3.as_bytes().as_slice()],
        )?;
        Ok(rows > 0)
    }

    /// List all entries (for debugging/iteration)
    pub fn list_all(&self) -> Result<Vec<HashEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT blake3, sha256, store_path, nar_size, \"references\", deriver FROM hash_index",
        )?;
        let rows = stmt.query_map([], row_to_entry_tuple)?;

        let mut entries = Vec::new();
        for row in rows {
            let tuple = row?;
            entries.push(tuple_to_entry(tuple)?);
        }
        Ok(entries)
    }

    /// Batch lookup by store hashes.
    /// Returns results in the same order as the input hashes.
    pub fn get_batch_by_store_hash(&self, hashes: &[&str]) -> Result<Vec<Option<HashEntry>>> {
        let mut results = Vec::with_capacity(hashes.len());
        for hash in hashes {
            results.push(self.get_by_store_hash(hash)?);
        }
        Ok(results)
    }

    /// Get the number of entries in the index
    pub fn count(&self) -> Result<u64> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM hash_index", [], |row| row.get(0))?;
        Ok(count as u64)
    }
}

#[cfg(feature = "sqlite")]
type EntryTuple = (Vec<u8>, Vec<u8>, String, i64, String, Option<String>);

/// Extract columns from a SQLite row into a tuple
#[cfg(feature = "sqlite")]
fn row_to_entry_tuple(row: &rusqlite::Row<'_>) -> rusqlite::Result<EntryTuple> {
    let blake3_bytes: Vec<u8> = row.get(0)?;
    let sha256_bytes: Vec<u8> = row.get(1)?;
    let store_path: String = row.get(2)?;
    let nar_size: i64 = row.get(3)?;
    let references_str: String = row.get(4)?;
    let deriver: Option<String> = row.get(5)?;
    Ok((
        blake3_bytes,
        sha256_bytes,
        store_path,
        nar_size,
        references_str,
        deriver,
    ))
}

/// Convert a tuple to a HashEntry
#[cfg(feature = "sqlite")]
fn tuple_to_entry(
    (blake3_bytes, sha256_bytes, store_path, nar_size, references_str, deriver): EntryTuple,
) -> Result<HashEntry> {
    let references = basenames_to_references(&references_str);
    let deriver = deriver.map(|d| format!("/nix/store/{}", d));
    Ok(HashEntry {
        blake3: Blake3Hash::from_slice(&blake3_bytes)?,
        sha256: Sha256Hash::from_slice(&sha256_bytes)?,
        store_path,
        nar_size: nar_size as u64,
        references,
        deriver,
    })
}

/// Convert an optional tuple to an optional HashEntry
#[cfg(feature = "sqlite")]
fn optional_tuple_to_entry(result: Option<EntryTuple>) -> Result<Option<HashEntry>> {
    match result {
        Some(tuple) => Ok(Some(tuple_to_entry(tuple)?)),
        None => Ok(None),
    }
}

/// Extract the basename from a store path (strip /nix/store/ prefix)
pub fn store_path_basename(path: &str) -> Option<String> {
    path.strip_prefix("/nix/store/").map(|s| s.to_string())
}

/// Convert a list of full store path references to basenames
#[cfg(feature = "sqlite")]
fn references_to_basenames(references: &[String]) -> Vec<String> {
    references
        .iter()
        .filter_map(|r| store_path_basename(r))
        .collect()
}

/// Convert space-separated basenames back to full store paths
#[cfg(feature = "sqlite")]
fn basenames_to_references(basenames_str: &str) -> Vec<String> {
    basenames_str
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .map(|s| format!("/nix/store/{}", s))
        .collect()
}

/// Extract the nix store hash from a store path.
///
/// Given "/nix/store/abc123-name", returns Some("abc123").
pub fn extract_store_hash(store_path: &str) -> Option<String> {
    store_path
        .strip_prefix("/nix/store/")
        .and_then(|s| s.split('-').next())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_hash_index_basic() -> Result<()> {
        let index = HashIndex::in_memory()?;

        let entry = HashEntry {
            blake3: Blake3Hash([1u8; 32]),
            sha256: Sha256Hash([2u8; 32]),
            store_path: "/nix/store/abc123-test".to_string(),
            nar_size: 1024,
            references: vec!["/nix/store/dep1-foo".to_string()],
            deriver: Some("/nix/store/xyz-test.drv".to_string()),
        };

        index.insert(&entry)?;

        // Look up by BLAKE3
        let found = index.get_by_blake3(&entry.blake3)?.unwrap();
        assert_eq!(found.store_path, entry.store_path);
        assert_eq!(found.references, entry.references);
        assert_eq!(found.deriver, entry.deriver);

        // Look up by SHA256
        let found = index.get_by_sha256(&entry.sha256)?.unwrap();
        assert_eq!(found.store_path, entry.store_path);

        // Look up by store path
        let found = index.get_by_store_path(&entry.store_path)?.unwrap();
        assert_eq!(found.blake3, entry.blake3);

        assert_eq!(index.count()?, 1);

        Ok(())
    }

    #[test]
    fn test_extract_store_hash() {
        assert_eq!(
            extract_store_hash("/nix/store/abc123-test"),
            Some("abc123".to_string())
        );
        assert_eq!(
            extract_store_hash("/nix/store/gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03-iroh-nix-0.1.0"),
            Some("gpnkbwdgfjvi04rcl7ybkgqsn2l4ma03".to_string())
        );
        assert_eq!(extract_store_hash("/tmp/something"), None);
        assert_eq!(extract_store_hash(""), None);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_get_by_store_hash() -> Result<()> {
        let index = HashIndex::in_memory()?;

        let entry = HashEntry {
            blake3: Blake3Hash([1u8; 32]),
            sha256: Sha256Hash([2u8; 32]),
            store_path: "/nix/store/abc123-test".to_string(),
            nar_size: 1024,
            references: vec![],
            deriver: None,
        };
        index.insert(&entry)?;

        let found = index.get_by_store_hash("abc123")?.unwrap();
        assert_eq!(found.store_path, entry.store_path);
        assert_eq!(found.blake3, entry.blake3);

        assert!(index.get_by_store_hash("nonexistent")?.is_none());

        Ok(())
    }

    #[test]
    fn test_nix_base32() {
        // Test vector from Nix
        let hash = Sha256Hash([0u8; 32]);
        let base32 = hash.to_nix_base32();
        assert_eq!(base32.len(), 52);
        assert_eq!(
            base32,
            "0000000000000000000000000000000000000000000000000000"
        );
    }
}

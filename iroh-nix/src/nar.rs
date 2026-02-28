//! NAR (Nix ARchive) serialization with streaming hash computation
//!
//! This module provides streaming NAR generation that computes both
//! SHA256 (for Nix) and BLAKE3 (for iroh) hashes on-the-fly.

use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::hash_index::{Blake3Hash, Sha256Hash};
use crate::{Error, Result};

/// NAR magic header
const NAR_MAGIC: &[u8] = b"nix-archive-1";

/// Result of NAR serialization with computed hashes
#[derive(Debug, Clone)]
pub struct NarInfo {
    /// BLAKE3 hash of the NAR content
    pub blake3: Blake3Hash,
    /// SHA256 hash of the NAR content
    pub sha256: Sha256Hash,
    /// Size of the NAR in bytes
    pub nar_size: u64,
}

/// A writer that computes both SHA256 and BLAKE3 hashes while writing
pub struct HashingWriter<W> {
    inner: W,
    sha256: Sha256,
    blake3: blake3::Hasher,
    bytes_written: u64,
}

impl<W: Write> HashingWriter<W> {
    /// Create a new hashing writer
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            sha256: Sha256::new(),
            blake3: blake3::Hasher::new(),
            bytes_written: 0,
        }
    }

    /// Finish writing and return the hashes
    pub fn finish(self) -> (W, NarInfo) {
        let sha256_result = self.sha256.finalize();
        let blake3_result = self.blake3.finalize();

        let info = NarInfo {
            sha256: Sha256Hash(sha256_result.into()),
            blake3: blake3_result.into(),
            nar_size: self.bytes_written,
        };

        (self.inner, info)
    }

    /// Get bytes written so far
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.sha256.update(&buf[..n]);
        self.blake3.update(&buf[..n]);
        self.bytes_written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// NAR serializer that writes to a generic writer
pub struct NarWriter<W> {
    writer: W,
}

impl<W: Write> NarWriter<W> {
    /// Create a new NAR writer
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Serialize a path to NAR format
    pub fn write_path(&mut self, path: &Path) -> Result<()> {
        // Write NAR header
        self.write_string(NAR_MAGIC)?;
        self.write_entry(path)?;
        Ok(())
    }

    /// Get the inner writer
    pub fn into_inner(self) -> W {
        self.writer
    }

    fn write_entry(&mut self, path: &Path) -> Result<()> {
        let metadata = path.symlink_metadata().map_err(|e| {
            Error::Nar(format!(
                "failed to read metadata for {}: {}",
                path.display(),
                e
            ))
        })?;

        self.write_string(b"(")?;

        if metadata.is_symlink() {
            self.write_symlink(path)?;
        } else if metadata.is_dir() {
            self.write_directory(path)?;
        } else if metadata.is_file() {
            self.write_file(path, &metadata)?;
        } else {
            return Err(Error::Nar(format!(
                "unsupported file type: {}",
                path.display()
            )));
        }

        self.write_string(b")")?;
        Ok(())
    }

    fn write_symlink(&mut self, path: &Path) -> Result<()> {
        let target = std::fs::read_link(path)
            .map_err(|e| Error::Nar(format!("failed to read symlink {}: {}", path.display(), e)))?;

        self.write_string(b"type")?;
        self.write_string(b"symlink")?;
        self.write_string(b"target")?;
        self.write_string(
            target
                .to_str()
                .ok_or_else(|| Error::Nar("symlink target is not valid UTF-8".into()))?
                .as_bytes(),
        )?;

        Ok(())
    }

    fn write_directory(&mut self, path: &Path) -> Result<()> {
        self.write_string(b"type")?;
        self.write_string(b"directory")?;

        // Read and sort directory entries (Nix requires sorted order)
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .map_err(|e| {
                Error::Nar(format!(
                    "failed to read directory {}: {}",
                    path.display(),
                    e
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Nar(format!("failed to read directory entry: {}", e)))?;

        entries.sort_by_key(|a| a.file_name());

        for entry in entries {
            let name = entry.file_name();
            let name_str = name
                .to_str()
                .ok_or_else(|| Error::Nar("filename is not valid UTF-8".into()))?;

            self.write_string(b"entry")?;
            self.write_string(b"(")?;
            self.write_string(b"name")?;
            self.write_string(name_str.as_bytes())?;
            self.write_string(b"node")?;
            self.write_entry(&entry.path())?;
            self.write_string(b")")?;
        }

        Ok(())
    }

    fn write_file(&mut self, path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
        self.write_string(b"type")?;
        self.write_string(b"regular")?;

        // Check if executable
        if metadata.permissions().mode() & 0o111 != 0 {
            self.write_string(b"executable")?;
            self.write_string(b"")?;
        }

        self.write_string(b"contents")?;

        let size = metadata.size();
        self.write_u64(size)?;

        // Stream file contents
        let mut file = std::fs::File::open(path)
            .map_err(|e| Error::Nar(format!("failed to open file {}: {}", path.display(), e)))?;

        let mut buf = [0u8; 65536];
        let mut remaining = size;
        while remaining > 0 {
            let to_read = std::cmp::min(remaining, buf.len() as u64) as usize;
            let n = file.read(&mut buf[..to_read]).map_err(|e| {
                Error::Nar(format!("failed to read file {}: {}", path.display(), e))
            })?;
            if n == 0 {
                return Err(Error::Nar(format!(
                    "unexpected EOF reading {}",
                    path.display()
                )));
            }
            self.writer
                .write_all(&buf[..n])
                .map_err(|e| Error::Nar(format!("failed to write NAR data: {}", e)))?;
            remaining -= n as u64;
        }

        // Pad to 8-byte boundary
        let padding = (8 - (size % 8)) % 8;
        if padding > 0 {
            let zeros = [0u8; 8];
            self.writer
                .write_all(&zeros[..padding as usize])
                .map_err(|e| Error::Nar(format!("failed to write padding: {}", e)))?;
        }

        Ok(())
    }

    fn write_string(&mut self, s: &[u8]) -> Result<()> {
        self.write_u64(s.len() as u64)?;
        self.writer
            .write_all(s)
            .map_err(|e| Error::Nar(format!("failed to write string: {}", e)))?;

        // Pad to 8-byte boundary
        let padding = (8 - (s.len() % 8)) % 8;
        if padding > 0 {
            let zeros = [0u8; 8];
            self.writer
                .write_all(&zeros[..padding])
                .map_err(|e| Error::Nar(format!("failed to write padding: {}", e)))?;
        }
        Ok(())
    }

    fn write_u64(&mut self, n: u64) -> Result<()> {
        self.writer
            .write_all(&n.to_le_bytes())
            .map_err(|e| Error::Nar(format!("failed to write u64: {}", e)))
    }
}

/// Serialize a path to NAR format and compute hashes
pub fn serialize_path(path: &Path) -> Result<(Vec<u8>, NarInfo)> {
    let mut buffer = Vec::new();
    let hashing_writer = HashingWriter::new(&mut buffer);
    let mut nar_writer = NarWriter::new(hashing_writer);
    nar_writer.write_path(path)?;
    let (_, info) = nar_writer.into_inner().finish();
    Ok((buffer, info))
}

/// Serialize a path to NAR format, writing to a writer and computing hashes
pub fn serialize_path_to_writer<W: Write>(path: &Path, writer: W) -> Result<NarInfo> {
    let hashing_writer = HashingWriter::new(writer);
    let mut nar_writer = NarWriter::new(hashing_writer);
    nar_writer.write_path(path)?;
    let (_, info) = nar_writer.into_inner().finish();
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_serialize_regular_file() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("test.txt");
        fs::write(&file_path, b"hello world").unwrap();

        let (nar, info) = serialize_path(&file_path)?;

        // NAR should start with magic header length
        assert!(nar.len() > 8);
        assert!(info.nar_size > 0);
        assert_eq!(info.nar_size, nar.len() as u64);

        Ok(())
    }

    #[test]
    fn test_serialize_directory() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let dir_path = tmp.path().join("testdir");
        fs::create_dir(&dir_path).unwrap();
        fs::write(dir_path.join("a.txt"), b"aaa").unwrap();
        fs::write(dir_path.join("b.txt"), b"bbb").unwrap();

        let (nar, info) = serialize_path(&dir_path)?;

        assert!(nar.len() > 8);
        assert!(info.nar_size > 0);

        Ok(())
    }

    #[test]
    fn test_serialize_symlink() -> Result<()> {
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("target.txt");
        let link_path = tmp.path().join("link");
        fs::write(&file_path, b"target content").unwrap();
        std::os::unix::fs::symlink("target.txt", &link_path).unwrap();

        let (nar, info) = serialize_path(&link_path)?;

        assert!(nar.len() > 8);
        assert!(info.nar_size > 0);

        Ok(())
    }

    #[test]
    fn test_hashing_writer() {
        let mut buffer = Vec::new();
        let mut writer = HashingWriter::new(&mut buffer);
        writer.write_all(b"test data").unwrap();
        let (_, info) = writer.finish();

        assert_eq!(info.nar_size, 9);
        // Hashes should be non-zero
        assert_ne!(info.blake3.0, [0u8; 32]);
        assert_ne!(info.sha256.0, [0u8; 32]);
    }
}

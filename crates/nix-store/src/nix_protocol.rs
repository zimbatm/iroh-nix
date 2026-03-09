//! Nix CommonProto binary format helpers
//!
//! Implements the serialization format used by the Nix build-hook protocol
//! for communication between nix-daemon and build hooks via file descriptors.
//!
//! Wire format:
//! - Integers: little-endian
//! - Strings: u64 length + bytes + padding to 8-byte alignment
//! - String sets: u64 count + count strings
//! - Store path sets: same as string sets

use std::io::{self, Read, Write};

use crate::{Error, Result};

/// Read a u32 from a reader (little-endian)
pub fn read_u32<R: Read>(reader: &mut R) -> Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::Protocol("unexpected EOF reading u32".into())
        } else {
            Error::Io(e)
        }
    })?;
    Ok(u32::from_le_bytes(buf))
}

/// Read a u64 from a reader (little-endian)
pub fn read_u64<R: Read>(reader: &mut R) -> Result<u64> {
    let mut buf = [0u8; 8];
    reader.read_exact(&mut buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Error::Protocol("unexpected EOF reading u64".into())
        } else {
            Error::Io(e)
        }
    })?;
    Ok(u64::from_le_bytes(buf))
}

/// Read a string from a reader (Nix CommonProto format)
///
/// Format: u64 length + bytes + padding to 8-byte alignment
pub fn read_string<R: Read>(reader: &mut R) -> Result<String> {
    let len = read_u64(reader)? as usize;

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;

    // Skip padding to 8-byte alignment
    let padding = (8 - (len % 8)) % 8;
    if padding > 0 {
        let mut pad_buf = vec![0u8; padding];
        reader.read_exact(&mut pad_buf)?;
    }

    String::from_utf8(buf).map_err(|e| Error::Protocol(format!("invalid UTF-8 in string: {}", e)))
}

/// Read a set of strings (Nix CommonProto format)
///
/// Format: u64 count + count strings
pub fn read_string_set<R: Read>(reader: &mut R) -> Result<Vec<String>> {
    let count = read_u64(reader)? as usize;
    let mut strings = Vec::with_capacity(count);
    for _ in 0..count {
        strings.push(read_string(reader)?);
    }
    Ok(strings)
}

/// Write a u32 to a writer (little-endian)
pub fn write_u32<W: Write>(writer: &mut W, value: u32) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

/// Write a u64 to a writer (little-endian)
pub fn write_u64<W: Write>(writer: &mut W, value: u64) -> Result<()> {
    writer.write_all(&value.to_le_bytes())?;
    Ok(())
}

/// Write a string to a writer (Nix CommonProto format)
///
/// Format: u64 length + bytes + padding to 8-byte alignment
pub fn write_string<W: Write>(writer: &mut W, s: &str) -> Result<()> {
    let bytes = s.as_bytes();
    write_u64(writer, bytes.len() as u64)?;
    writer.write_all(bytes)?;

    // Write padding to 8-byte alignment
    let padding = (8 - (bytes.len() % 8)) % 8;
    if padding > 0 {
        writer.write_all(&vec![0u8; padding])?;
    }

    Ok(())
}

/// Write a set of strings (Nix CommonProto format)
///
/// Format: u64 count + count strings
pub fn write_string_set<W: Write>(writer: &mut W, strings: &[String]) -> Result<()> {
    write_u64(writer, strings.len() as u64)?;
    for s in strings {
        write_string(writer, s)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_u32_roundtrip() {
        let mut buf = Vec::new();
        write_u32(&mut buf, 42).unwrap();
        assert_eq!(buf.len(), 4);

        let mut cursor = Cursor::new(&buf);
        assert_eq!(read_u32(&mut cursor).unwrap(), 42);
    }

    #[test]
    fn test_u64_roundtrip() {
        let mut buf = Vec::new();
        write_u64(&mut buf, 0xDEAD_BEEF_CAFE_BABE).unwrap();
        assert_eq!(buf.len(), 8);

        let mut cursor = Cursor::new(&buf);
        assert_eq!(read_u64(&mut cursor).unwrap(), 0xDEAD_BEEF_CAFE_BABE);
    }

    #[test]
    fn test_string_roundtrip_aligned() {
        let mut buf = Vec::new();
        write_string(&mut buf, "try").unwrap();
        assert_eq!(buf.len(), 16);

        let mut cursor = Cursor::new(&buf);
        assert_eq!(read_string(&mut cursor).unwrap(), "try");
    }

    #[test]
    fn test_string_roundtrip_exact_alignment() {
        let mut buf = Vec::new();
        write_string(&mut buf, "x86_64-l").unwrap();
        assert_eq!(buf.len(), 16);

        let mut cursor = Cursor::new(&buf);
        assert_eq!(read_string(&mut cursor).unwrap(), "x86_64-l");
    }

    #[test]
    fn test_string_roundtrip_empty() {
        let mut buf = Vec::new();
        write_string(&mut buf, "").unwrap();
        assert_eq!(buf.len(), 8);

        let mut cursor = Cursor::new(&buf);
        assert_eq!(read_string(&mut cursor).unwrap(), "");
    }

    #[test]
    fn test_string_set_roundtrip() {
        let strings: Vec<String> = vec![
            "x86_64-linux".to_string(),
            "kvm".to_string(),
            "big-parallel".to_string(),
        ];

        let mut buf = Vec::new();
        write_string_set(&mut buf, &strings).unwrap();

        let mut cursor = Cursor::new(&buf);
        let result = read_string_set(&mut cursor).unwrap();
        assert_eq!(result, strings);
    }
}

//! Nix base32 encoding and decoding
//!
//! Nix uses a custom base32 alphabet: `0123456789abcdfghijklmnpqrsvwxyz`
//! (note: no 'e', 'o', 't', 'u' to avoid confusion).
//!
//! This module provides the canonical implementation used throughout
//! the workspace.

use crate::{Error, Result};

/// The Nix base32 alphabet (32 chars, no e/o/t/u).
const NIX_BASE32_CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

/// Decode a Nix base32 string to bytes.
///
/// Returns a `Vec<u8>` of length `(input_len * 5) / 8`.
pub fn decode_nix_base32(s: &str) -> Result<Vec<u8>> {
    let mut lookup = [255u8; 128];
    for (i, &c) in NIX_BASE32_CHARS.iter().enumerate() {
        lookup[c as usize] = i as u8;
    }

    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let out_len = (len * 5) / 8;
    let mut out = vec![0u8; out_len];

    for n in (0..len).rev() {
        let c = chars[len - 1 - n];
        if c as u32 >= 128 {
            return Err(Error::Protocol(format!("invalid base32 char: {}", c)));
        }
        let digit = lookup[c as usize];
        if digit == 255 {
            return Err(Error::Protocol(format!("invalid base32 char: {}", c)));
        }

        let b = n * 5;
        let byte_idx = b / 8;
        let bit_idx = b % 8;

        if byte_idx < out_len {
            out[byte_idx] |= digit << bit_idx;
        }
        if bit_idx > 3 && byte_idx + 1 < out_len {
            out[byte_idx + 1] |= digit >> (8 - bit_idx);
        }
    }

    Ok(out)
}

/// Decode a Nix base32 string that is expected to be exactly 52 chars (SHA-256).
///
/// Returns a fixed-size `[u8; 32]`.
pub fn decode_nix_base32_fixed(s: &str) -> Result<[u8; 32]> {
    if s.len() != 52 {
        return Err(Error::Protocol(format!(
            "nix32 hash should be 52 chars, got {}",
            s.len()
        )));
    }
    let vec = decode_nix_base32(s)?;
    vec.try_into()
        .map_err(|_| Error::Protocol("decoded nix32 is not 32 bytes".into()))
}

/// Encode bytes to Nix base32 format.
///
/// The output length is `ceil(input_len * 8 / 5)` characters.
pub fn encode_nix_base32(bytes: &[u8]) -> String {
    let hash_bits = bytes.len() * 8;
    let out_len = hash_bits.div_ceil(5);
    let mut out = String::with_capacity(out_len);

    for n in (0..out_len).rev() {
        let b = n * 5;
        let byte_idx = b / 8;
        let bit_idx = b % 8;

        let mut c = (bytes[byte_idx] >> bit_idx) & 0x1f;
        if bit_idx > 3 && byte_idx + 1 < bytes.len() {
            c |= (bytes[byte_idx + 1] << (8 - bit_idx)) & 0x1f;
        }
        out.push(NIX_BASE32_CHARS[c as usize] as char);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_zeros() {
        let nix32 = "0000000000000000000000000000000000000000000000000000";
        let result = decode_nix_base32_fixed(nix32).unwrap();
        assert_eq!(result, [0u8; 32]);
    }

    #[test]
    fn test_decode_invalid_length() {
        let result = decode_nix_base32_fixed("abc");
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = [42u8; 32];
        let encoded = encode_nix_base32(&original);
        assert_eq!(encoded.len(), 52);
        let decoded = decode_nix_base32_fixed(&encoded).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn test_decode_dynamic_length() {
        // 10-char nix base32 -> (10 * 5) / 8 = 6 bytes
        let s = "0000000000";
        let result = decode_nix_base32(s).unwrap();
        assert_eq!(result.len(), 6);
    }
}

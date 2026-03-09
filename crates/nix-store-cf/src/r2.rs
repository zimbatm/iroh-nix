//! R2 (Cloudflare object storage) operations for NAR content

use worker::*;

/// NAR key format in R2: "nar/<blake3-hex>.nar"
fn nar_key(blake3_hex: &str) -> String {
    format!("nar/{}.nar", blake3_hex)
}

/// Stream a NAR from R2 to the HTTP response
pub async fn stream_nar(bucket: &Bucket, blake3_hex: &str) -> Result<Response> {
    let key = nar_key(blake3_hex);

    match bucket.get(&key).execute().await? {
        Some(object) => {
            let body = object
                .body()
                .ok_or_else(|| Error::RustError("empty body".into()))?;
            let bytes = body.bytes().await?;

            let mut headers = Headers::new();
            headers.set("Content-Type", "application/x-nix-nar")?;
            headers.set("Content-Length", &bytes.len().to_string())?;

            Ok(Response::from_bytes(bytes)?.with_headers(headers))
        }
        None => Response::error("NAR not found", 404),
    }
}

/// Upload a NAR to R2
pub async fn put_nar(bucket: &Bucket, blake3_hex: &str, data: Vec<u8>) -> Result<()> {
    let key = nar_key(blake3_hex);
    bucket.put(&key, data).execute().await?;
    Ok(())
}

/// Check if a NAR exists in R2
pub async fn nar_exists(bucket: &Bucket, blake3_hex: &str) -> Result<bool> {
    let key = nar_key(blake3_hex);
    let obj = bucket.head(&key).await?;
    Ok(obj.is_some())
}

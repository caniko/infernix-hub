//! Content-addressed asset storage on a local volume.
//!
//! `put` writes a temporary file, fsyncs it, verifies the checksum and renames
//! it into place, so a key is only ever returned for a finalized object. An
//! S3-compatible store implements the same `AssetStore` port.

use std::path::PathBuf;

use async_trait::async_trait;
use domain::ports::AssetStore;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct LocalAssetStore {
    root: PathBuf,
}

impl LocalAssetStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path(&self, key: &str) -> std::io::Result<PathBuf> {
        // Keys are `sha256/<2>/<64 hex>`; refuse anything else (no traversal).
        let ok =
            key.len() == 74 && key.starts_with("sha256/") && key[7..9] == key[10..12] && key[10..].bytes().all(|b| b.is_ascii_hexdigit());
        if !ok || key.as_bytes()[9] != b'/' {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid asset key"));
        }
        Ok(self.root.join(key))
    }
}

#[async_trait]
impl AssetStore for LocalAssetStore {
    async fn put(&self, sha256: &str, bytes: &[u8]) -> std::io::Result<String> {
        let actual = hex::encode(Sha256::digest(bytes));
        if actual != sha256 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "checksum mismatch before store"));
        }
        let key = format!("sha256/{}/{}", &sha256[..2], sha256);
        let dest = self.path(&key)?;
        if let Ok(existing) = tokio::fs::read(&dest).await
            && hex::encode(Sha256::digest(&existing)) == sha256
        {
            return Ok(key);
        }
        let dir = dest.parent().expect("key has a parent");
        tokio::fs::create_dir_all(dir).await?;
        let tmp_dir = self.root.join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await?;
        let tmp = tmp_dir.join(uuid::Uuid::new_v4().to_string());
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(bytes).await?;
        f.sync_all().await?;
        drop(f);
        let written = tokio::fs::read(&tmp).await?;
        if hex::encode(Sha256::digest(&written)) != sha256 {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "checksum mismatch after write"));
        }
        tokio::fs::rename(&tmp, &dest).await?;
        tokio::fs::File::open(dir).await?.sync_all().await?;
        Ok(key)
    }

    async fn get(&self, key: &str) -> std::io::Result<Vec<u8>> {
        tokio::fs::read(self.path(key)?).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn put_is_content_addressed_and_rejects_bad_keys() {
        let dir = std::env::temp_dir().join(format!("ihub-store-{}", uuid::Uuid::new_v4()));
        let s = LocalAssetStore::new(&dir);
        let sha = hex::encode(Sha256::digest(b"hello"));
        let key = s.put(&sha, b"hello").await.unwrap();
        assert_eq!(s.put(&sha, b"hello").await.unwrap(), key);
        assert_eq!(s.get(&key).await.unwrap(), b"hello");
        assert!(s.put(&sha, b"other").await.is_err());
        assert!(s.get("../../etc/passwd").await.is_err());
        let _ = std::fs::remove_dir_all(dir);
    }
}

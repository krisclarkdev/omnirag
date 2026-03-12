use sha2::{Digest, Sha256};
use std::path::Path;

/// Generate the Redis key for a file: `<sha256_of_absolute_path>_<filename>`.
pub fn generate_redis_key(path: &Path) -> String {
    let abs = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string();

    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let mut hasher = Sha256::new();
    hasher.update(abs.as_bytes());
    let hash = hex::encode(hasher.finalize());

    format!("{}_{}", hash, filename)
}

/// Compute the SHA-256 hash of a file's raw contents.
pub fn hash_file_contents(path: &Path) -> Result<String, std::io::Error> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

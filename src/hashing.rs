use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Generate the Redis key for a file: `<sha256_of_absolute_path>_<filename>`.
/// Takes an optional pre-canonicalized path to avoid redundant syscalls.
pub fn generate_redis_key_from(abs_path: &str, filename: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(abs_path.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("{}_{}", hash, filename)
}

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

    generate_redis_key_from(&abs, &filename)
}

/// Compute the SHA-256 hash of a file's raw contents using streaming I/O.
/// Reads in 8KB chunks to avoid loading the entire file into memory.
pub fn hash_file_contents(path: &Path) -> Result<String, std::io::Error> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::with_capacity(8192, file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(hex::encode(hasher.finalize()))
}

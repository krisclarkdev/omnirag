#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    // ─────────────────── SHA-256 Hashing Tests ───────────────────

    /// Replicates the logic from hashing.rs for testing
    fn generate_redis_key(path: &Path) -> String {
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

    fn hash_file_contents(path: &Path) -> String {
        let bytes = fs::read(path).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hex::encode(hasher.finalize())
    }

    #[test]
    fn test_identical_contents_produce_identical_hashes() {
        let dir = TempDir::new().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");

        fs::write(&file_a, "Hello, World!").unwrap();
        fs::write(&file_b, "Hello, World!").unwrap();

        let hash_a = hash_file_contents(&file_a);
        let hash_b = hash_file_contents(&file_b);

        assert_eq!(hash_a, hash_b, "Identical contents must produce identical hashes");
    }

    #[test]
    fn test_different_contents_produce_different_hashes() {
        let dir = TempDir::new().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");

        fs::write(&file_a, "Hello, World!").unwrap();
        fs::write(&file_b, "Goodbye, World!").unwrap();

        let hash_a = hash_file_contents(&file_a);
        let hash_b = hash_file_contents(&file_b);

        assert_ne!(hash_a, hash_b, "Different contents must produce different hashes");
    }

    #[test]
    fn test_hash_is_64_char_hex() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "test content").unwrap();

        let hash = hash_file_contents(&file);
        assert_eq!(hash.len(), 64, "SHA-256 hex should be exactly 64 characters");
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should only contain hex characters"
        );
    }

    #[test]
    fn test_empty_file_has_known_hash() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("empty.txt");
        fs::write(&file, "").unwrap();

        let hash = hash_file_contents(&file);
        // SHA-256 of empty string = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    // ─────────────────── Redis Key Generation Tests ───────────────────

    #[test]
    fn test_redis_key_contains_filename() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("myfile.md");
        fs::write(&file, "content").unwrap();

        let key = generate_redis_key(&file);
        assert!(
            key.ends_with("_myfile.md"),
            "Key '{}' should end with '_myfile.md'",
            key
        );
    }

    #[test]
    fn test_redis_key_has_sha256_prefix() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        fs::write(&file, "data").unwrap();

        let key = generate_redis_key(&file);
        let parts: Vec<&str> = key.rsplitn(2, '_').collect();
        assert_eq!(parts.len(), 2);
        let hash_part = parts[1];
        assert_eq!(
            hash_part.len(),
            64,
            "Hash prefix should be 64 chars, got {}",
            hash_part.len()
        );
    }

    #[test]
    fn test_same_path_produces_same_key() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("stable.md");
        fs::write(&file, "content").unwrap();

        let key1 = generate_redis_key(&file);
        let key2 = generate_redis_key(&file);
        assert_eq!(key1, key2, "Same path should always produce the same key");
    }

    #[test]
    fn test_different_paths_produce_different_keys() {
        let dir = TempDir::new().unwrap();
        let file_a = dir.path().join("a.md");
        let file_b = dir.path().join("b.md");
        fs::write(&file_a, "same").unwrap();
        fs::write(&file_b, "same").unwrap();

        let key_a = generate_redis_key(&file_a);
        let key_b = generate_redis_key(&file_b);
        assert_ne!(
            key_a, key_b,
            "Different file paths should produce different keys"
        );
    }

    // ─────────────────── Config Serialization Tests ───────────────────

    #[test]
    fn test_config_default() {
        #[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
        struct AppConfig {
            #[serde(default)]
            target_directory: String,
            openwebui_url: String,
            openwebui_token: String,
            openwebui_knowledge_id: String,
            #[serde(default)]
            redis_url: String,
            #[serde(default)]
            context_header_label: String,
            #[serde(default)]
            max_concurrent_uploads: u32,
            #[serde(default)]
            convert_to_markdown: bool,
        }

        let config = AppConfig::default();
        assert!(config.target_directory.is_empty());
        assert!(config.openwebui_url.is_empty());
        assert!(config.openwebui_token.is_empty());
        assert!(config.redis_url.is_empty());
        assert!(config.context_header_label.is_empty());
        assert_eq!(config.max_concurrent_uploads, 0);
        assert!(!config.convert_to_markdown);
    }

    #[test]
    fn test_config_deserialization_with_optional_fields() {
        #[derive(Debug, serde::Serialize, serde::Deserialize)]
        struct AppConfig {
            #[serde(default)]
            target_directory: String,
            openwebui_url: String,
            openwebui_token: String,
            openwebui_knowledge_id: String,
            #[serde(default)]
            redis_url: String,
            #[serde(default)]
            context_header_label: String,
            #[serde(default)]
            max_concurrent_uploads: u32,
            #[serde(default)]
            convert_to_markdown: bool,
        }

        // Simulate form submission without optional fields
        let form_data = "openwebui_url=https://example.com&openwebui_token=sk-test&openwebui_knowledge_id=kb-123";
        let config: AppConfig = serde_urlencoded::from_str(form_data).unwrap();

        assert_eq!(config.openwebui_url, "https://example.com");
        assert_eq!(config.openwebui_token, "sk-test");
        assert_eq!(config.openwebui_knowledge_id, "kb-123");
        assert!(config.target_directory.is_empty(), "target_directory should default to empty");
        assert!(config.redis_url.is_empty(), "redis_url should default to empty");
        assert!(config.context_header_label.is_empty(), "context_header_label should default to empty");
        assert_eq!(config.max_concurrent_uploads, 0, "max_concurrent_uploads should default to 0");
        assert!(!config.convert_to_markdown, "convert_to_markdown should default to false");
    }

    #[test]
    fn test_config_deserialization_with_new_fields() {
        #[derive(Debug, serde::Serialize, serde::Deserialize)]
        struct AppConfig {
            #[serde(default)]
            target_directory: String,
            openwebui_url: String,
            openwebui_token: String,
            openwebui_knowledge_id: String,
            #[serde(default)]
            redis_url: String,
            #[serde(default)]
            context_header_label: String,
            #[serde(default)]
            max_concurrent_uploads: u32,
            #[serde(default)]
            convert_to_markdown: bool,
        }

        let form_data = "openwebui_url=https://example.com&openwebui_token=sk-test&openwebui_knowledge_id=kb-123&context_header_label=Project+Lore&max_concurrent_uploads=10&convert_to_markdown=true";
        let config: AppConfig = serde_urlencoded::from_str(form_data).unwrap();

        assert_eq!(config.context_header_label, "Project Lore");
        assert_eq!(config.max_concurrent_uploads, 10);
        assert!(config.convert_to_markdown);
    }

    #[test]
    fn test_config_json_roundtrip() {
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
        struct AppConfig {
            #[serde(default)]
            target_directory: String,
            openwebui_url: String,
            openwebui_token: String,
            openwebui_knowledge_id: String,
            #[serde(default)]
            redis_url: String,
            #[serde(default)]
            context_header_label: String,
            #[serde(default)]
            max_concurrent_uploads: u32,
            #[serde(default)]
            convert_to_markdown: bool,
        }

        let config = AppConfig {
            target_directory: "/rag".to_string(),
            openwebui_url: "https://example.com".to_string(),
            openwebui_token: "sk-abc123".to_string(),
            openwebui_knowledge_id: "d8f441b0-240f-4bdf-8056-70a42c24a022".to_string(),
            redis_url: "redis://127.0.0.1:6379/0".to_string(),
            context_header_label: "Solutions Architect Context".to_string(),
            max_concurrent_uploads: 5,
            convert_to_markdown: true,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: AppConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config, deserialized);
    }
}

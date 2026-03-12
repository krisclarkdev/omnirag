use crate::api::OpenWebUiClient;
use crate::config::AppConfig;
use crate::hashing::{generate_redis_key, generate_redis_key_from, hash_file_contents};
use crate::redis_client;

use bytes::Bytes;
use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};
use walkdir::WalkDir;

/// Supported file extensions for ingestion.
const ALLOWED_EXTENSIONS: &[&str] = &[
    "md", "txt", "pdf", "csv", "json", "yaml", "yml", "toml",
    "xml", "html", "htm", "rst", "log", "cfg", "ini", "conf",
    "py", "rs", "go", "js", "ts", "sh", "bat", "ps1",
];

/// OS/hidden files to always skip.
const IGNORED_FILES: &[&str] = &[
    ".DS_Store", "Thumbs.db", "desktop.ini", ".gitkeep",
];

/// Text-based extensions safe for context injection.
/// Binary formats (e.g., pdf) are excluded to prevent corruption.
const TEXT_EXTENSIONS: &[&str] = &[
    "md", "txt", "csv", "json", "yaml", "yml", "toml",
    "xml", "html", "htm", "rst", "log", "cfg", "ini", "conf",
    "py", "rs", "go", "js", "ts", "sh", "bat", "ps1",
];

/// Run the full sync: Phase 0 (Reconciliation) + Phase 1 (Ingestion) + Phase 2 (Orphan Cleanup).
pub async fn run_sync(
    con: &mut redis::aio::MultiplexedConnection,
    config: &AppConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let api = OpenWebUiClient::new(&config.openwebui_url, &config.openwebui_token);

    info!("=== Phase 0: Reconciliation ===");
    if let Err(e) = phase0_reconciliation(con, config, &api).await {
        warn!("Reconciliation failed (continuing anyway): {}", e);
    }

    info!("=== Phase 1: Ingestion ===");
    phase1_ingestion(con, config, &api).await?;

    info!("=== Phase 2: Orphan Cleanup ===");
    let target_dir = Path::new(&config.target_directory);
    phase2_cleanup(con, &api, target_dir).await?;

    info!("Sync complete.");
    Ok(())
}

// ─────────────────── .ragignore ───────────────────

/// Load ignore patterns from a `.ragignore` file (one glob pattern per line).
pub fn load_ragignore(target_dir: &Path) -> Vec<String> {
    let ragignore_path = target_dir.join(".ragignore");
    if !ragignore_path.exists() {
        return Vec::new();
    }

    match std::fs::File::open(&ragignore_path) {
        Ok(file) => std::io::BufReader::new(file)
            .lines()
            .filter_map(|line| {
                let line = line.ok()?;
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    None
                } else {
                    Some(trimmed)
                }
            })
            .collect(),
        Err(e) => {
            warn!("Failed to read .ragignore: {}", e);
            Vec::new()
        }
    }
}

// ─────────────────── Filtering helpers ───────────────────

/// Check if a file matches any ragignore pattern.
pub fn is_ragignored(path: &Path, target_dir: &Path, patterns: &[String]) -> bool {
    let relative = match path.strip_prefix(target_dir) {
        Ok(r) => r,
        Err(_) => return false,
    };
    let relative_str = relative.to_string_lossy();

    for pattern in patterns {
        // Simple glob: check filename or path component match
        if relative_str.contains(pattern.as_str()) {
            return true;
        }
        // Check path components (e.g., "subdir" matches "subdir/file.txt")
        for component in relative.components() {
            if component.as_os_str().to_string_lossy() == *pattern {
                return true;
            }
        }
    }
    false
}

/// Check if a file has an allowed extension.
pub fn has_allowed_extension(path: &Path) -> bool {
    match path.extension() {
        Some(ext) => {
            let ext_lower = ext.to_string_lossy().to_lowercase();
            ALLOWED_EXTENSIONS.contains(&ext_lower.as_str())
        }
        None => false, // No extension → skip
    }
}

/// Check if a filename is in the OS-file ignore list.
pub fn is_os_ignored(path: &Path) -> bool {
    match path.file_name() {
        Some(name) => {
            let name_str = name.to_string_lossy();
            IGNORED_FILES.contains(&name_str.as_ref())
        }
        None => false,
    }
}

/// Standard filter for dot-directories, used in both Phase 0 and Phase 1.
fn is_visible_entry(e: &walkdir::DirEntry) -> bool {
    let name = e.file_name().to_string_lossy();
    !name.starts_with('.')
}

// ─────────────────── Phase 0: Reconciliation ───────────────────

/// Query Open WebUI for the current KB file list and heal Redis state
/// to prevent duplicate uploads after crashes.
async fn phase0_reconciliation(
    con: &mut redis::aio::MultiplexedConnection,
    config: &AppConfig,
    api: &OpenWebUiClient,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if config.openwebui_knowledge_id.is_empty() {
        info!("No knowledge base ID configured, skipping reconciliation");
        return Ok(());
    }

    let remote_files = api.list_knowledge_files(&config.openwebui_knowledge_id).await?;
    if remote_files.is_empty() {
        info!("Knowledge base is empty, nothing to reconcile");
        return Ok(());
    }

    let target_dir = Path::new(&config.target_directory);

    // Optimization: Build a filename→paths HashMap in a single pass instead of
    // walking the entire tree per remote file (O(K*N) → O(N+K))
    let file_index = build_file_index(target_dir);
    let mut healed = 0u32;

    for remote_file in &remote_files {
        // O(1) lookup instead of full directory walk
        let local_paths = file_index.get(&remote_file.filename);
        let local_path = local_paths.and_then(|paths| paths.first());

        if let Some(local_path) = local_path {
            if !local_path.exists() {
                info!("[GHOST] {} exists in Open WebUI but not on disk — skipping heal (Phase 2 will clean up)", remote_file.filename);
                continue;
            }

            let key = generate_redis_key(local_path);

            // Check if Redis already has this file tracked
            let exists = redis_client::fcall_check_file_exists(con, &key).await?;
            if exists {
                // Verify the stored file_id matches
                let stored_id: Option<String> = redis::cmd("HGET")
                    .arg(&key)
                    .arg("openwebui_file_id")
                    .query_async(con)
                    .await?;

                if stored_id.as_deref() == Some(&remote_file.id) {
                    continue; // Already in sync
                }
            }

            // Heal: update Redis with the correct remote file_id
            let abs_path = local_path.canonicalize()
                .unwrap_or_else(|_| local_path.clone())
                .to_string_lossy()
                .to_string();
            let content_hash = hash_file_contents(local_path).unwrap_or_default();

            redis_client::fcall_upsert_sync_state(
                con, &key, &abs_path, &content_hash, &remote_file.id,
            )
            .await?;

            info!("[HEAL] {} → file_id={}", remote_file.filename, remote_file.id);
            healed += 1;
        }
    }

    if healed > 0 {
        info!("Healed {} file(s) during reconciliation", healed);
    } else {
        info!("Reconciliation complete — no healing needed");
    }

    Ok(())
}

/// Build a HashMap<filename, Vec<PathBuf>> from a single directory walk.
/// Used by Phase 0 for O(1) lookups instead of repeated full walks.
fn build_file_index(target_dir: &Path) -> HashMap<String, Vec<PathBuf>> {
    let mut index: HashMap<String, Vec<PathBuf>> = HashMap::new();

    for entry in WalkDir::new(target_dir)
        .into_iter()
        .filter_entry(is_visible_entry)
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            if let Some(name) = entry.file_name().to_str() {
                index
                    .entry(name.to_string())
                    .or_default()
                    .push(entry.into_path());
            }
        }
    }

    index
}

// ─────────────────── Phase 1: Ingestion ───────────────────

/// Phase 1: Walk directory, detect new/updated/unchanged files, upload as needed.
async fn phase1_ingestion(
    con: &mut redis::aio::MultiplexedConnection,
    config: &AppConfig,
    api: &OpenWebUiClient,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let target_dir = Path::new(&config.target_directory);
    if !target_dir.exists() {
        return Err(format!("Target directory does not exist: {}", config.target_directory).into());
    }

    let ragignore_patterns = load_ragignore(target_dir);

    // Collect filtered files
    let mut files = Vec::new();
    let mut skipped_ext = 0u32;
    let mut skipped_ignore = 0u32;

    for entry in WalkDir::new(target_dir)
        .into_iter()
        .filter_entry(is_visible_entry)
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.into_path();

        // Filter: OS files
        if is_os_ignored(&path) {
            continue;
        }

        // Filter: .ragignore
        if is_ragignored(&path, target_dir, &ragignore_patterns) {
            skipped_ignore += 1;
            continue;
        }

        // Filter: extension whitelist
        if !has_allowed_extension(&path) {
            skipped_ext += 1;
            continue;
        }

        files.push(path);
    }

    info!(
        "Found {} files to process (skipped: {} unsupported ext, {} ragignored)",
        files.len(), skipped_ext, skipped_ignore
    );

    let max = if config.max_concurrent_uploads == 0 {
        usize::MAX
    } else {
        config.max_concurrent_uploads as usize
    };
    let semaphore = Arc::new(Semaphore::new(max));
    let mut handles = Vec::new();

    for file_path in files {
        let sem = semaphore.clone();
        let mut con = con.clone();
        let api = api.clone();
        let knowledge_id = config.openwebui_knowledge_id.clone();
        let convert_md = config.convert_to_markdown;

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();

            if let Err(e) = process_file(&mut con, &api, &knowledge_id, &file_path, convert_md).await {
                // Non-fatal: log and skip, don't crash the sync
                warn!("Failed to process '{}': {} (will retry on next sync)", file_path.display(), e);
            }
        }));
    }

    for handle in handles {
        if let Err(e) = handle.await {
            error!("Task join error: {}", e);
        }
    }

    Ok(())
}

/// Process a single file: check existence, verify hash, upload if new/changed.
/// Optimizations:
/// - Single canonicalize() call, reused for Redis key and state
/// - Async file I/O via spawn_blocking to avoid blocking the Tokio runtime
/// - Decoupled poll+attach pipeline: upload returns immediately, poll+attach runs in background
async fn process_file(
    con: &mut redis::aio::MultiplexedConnection,
    api: &OpenWebUiClient,
    knowledge_id: &str,
    file_path: &Path,
    convert_to_markdown: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Single canonicalize — reused throughout
    let abs_path = file_path
        .canonicalize()?
        .to_string_lossy()
        .to_string();
    let original_filename = file_path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let key = generate_redis_key_from(&abs_path, &original_filename);

    // Async hash via spawn_blocking to avoid blocking the Tokio runtime
    let hash_path = file_path.to_path_buf();
    let content_hash = tokio::task::spawn_blocking(move || {
        hash_file_contents(&hash_path)
    }).await??;

    // Determine upload filename (may change extension to .md)
    let filename = if convert_to_markdown && is_text_file(file_path) && !is_markdown_file(file_path) {
        let stem = file_path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!("{}.md", stem)
    } else {
        original_filename.clone()
    };

    // Step 1: Check if file exists in Redis
    let exists = redis_client::fcall_check_file_exists(con, &key).await?;

    if !exists {
        // Condition A: New file
        info!("[NEW] {}", original_filename);
        let payload = build_upload_payload(con, &key, file_path, convert_to_markdown).await?;
        let file_id = api.upload_file(&filename, payload).await?;

        // Decoupled pipeline: spawn poll+attach in background
        let api_bg = api.clone();
        let kid = knowledge_id.to_string();
        let fid = file_id.clone();
        tokio::spawn(async move {
            if let Err(e) = api_bg.poll_process_status(&fid).await {
                warn!("Background poll failed for file_id={}: {}", fid, e);
                return;
            }
            if let Err(e) = api_bg.add_to_knowledge(&kid, &fid).await {
                warn!("Background attach failed for file_id={}: {}", fid, e);
            }
        });

        redis_client::fcall_upsert_sync_state(con, &key, &abs_path, &content_hash, &file_id)
            .await?;
    } else {
        // File exists — check if contents changed
        let hash_matches = redis_client::fcall_verify_file_hash(con, &key, &content_hash).await?;

        if hash_matches {
            // Condition B: Unchanged — skip
            info!("[SKIP] {}", original_filename);
        } else {
            // Condition C: Updated file
            info!("[UPDATE] {}", original_filename);

            // Get the old file ID to delete
            let old_file_id: Option<String> = redis::cmd("HGET")
                .arg(&key)
                .arg("openwebui_file_id")
                .query_async(con)
                .await?;

            if let Some(old_id) = old_file_id {
                if let Err(e) = api.delete_file(&old_id).await {
                    warn!("Failed to delete old file {}: {}", old_id, e);
                }
            }

            let payload = build_upload_payload(con, &key, file_path, convert_to_markdown).await?;
            let file_id = api.upload_file(&filename, payload).await?;

            // Decoupled pipeline: spawn poll+attach in background
            let api_bg = api.clone();
            let kid = knowledge_id.to_string();
            let fid = file_id.clone();
            tokio::spawn(async move {
                if let Err(e) = api_bg.poll_process_status(&fid).await {
                    warn!("Background poll failed for file_id={}: {}", fid, e);
                    return;
                }
                if let Err(e) = api_bg.add_to_knowledge(&kid, &fid).await {
                    warn!("Background attach failed for file_id={}: {}", fid, e);
                }
            });

            redis_client::fcall_upsert_sync_state(con, &key, &abs_path, &content_hash, &file_id)
                .await?;
        }
    }

    Ok(())
}

/// Check if file is already a markdown file.
fn is_markdown_file(path: &Path) -> bool {
    match path.extension() {
        Some(ext) => {
            let e = ext.to_string_lossy().to_lowercase();
            e == "md" || e == "txt"
        }
        None => false,
    }
}

/// Map a file extension to a markdown code fence language identifier.
fn ext_to_lang(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => "python",
        Some("rs") => "rust",
        Some("go") => "go",
        Some("js") => "javascript",
        Some("ts") => "typescript",
        Some("sh" | "bat" | "ps1") => "shell",
        Some("json") => "json",
        Some("yaml" | "yml") => "yaml",
        Some("toml") => "toml",
        Some("xml" | "html" | "htm") => "html",
        Some("csv") => "csv",
        Some("rst") => "rst",
        Some("cfg" | "ini" | "conf") => "ini",
        Some("log") => "log",
        _ => "",
    }
}

/// Check if a file extension is a text-based format safe for context injection.
fn is_text_file(path: &Path) -> bool {
    match path.extension() {
        Some(ext) => {
            let ext_lower = ext.to_string_lossy().to_lowercase();
            TEXT_EXTENSIONS.contains(&ext_lower.as_str())
        }
        None => false,
    }
}

/// Build the upload payload.
/// For text files: prepend context header + raw contents.
/// For binary files (e.g., PDF): raw contents only to prevent corruption.
/// If convert_to_markdown is true, wraps non-markdown text content in code fences.
/// Uses async file I/O via spawn_blocking.
async fn build_upload_payload(
    con: &mut redis::aio::MultiplexedConnection,
    key: &str,
    file_path: &Path,
    convert_to_markdown: bool,
) -> Result<Bytes, Box<dyn std::error::Error + Send + Sync>> {
    // Async file read via spawn_blocking
    let path_owned = file_path.to_path_buf();
    let raw_contents = tokio::task::spawn_blocking(move || {
        std::fs::read(&path_owned)
    }).await??;

    if is_text_file(file_path) {
        let context_header = redis_client::fcall_get_formatted_context(con, key).await?;

        // Optionally wrap content in a code fence for markdown conversion
        let content = if convert_to_markdown && !is_markdown_file(file_path) {
            let lang = ext_to_lang(file_path);
            let text = String::from_utf8_lossy(&raw_contents);
            format!("```{}\n{}\n```\n", lang, text).into_bytes()
        } else {
            raw_contents
        };

        if context_header.is_empty() {
            return Ok(Bytes::from(content));
        }
        let mut payload = Vec::with_capacity(context_header.len() + content.len());
        payload.extend_from_slice(context_header.as_bytes());
        payload.extend_from_slice(&content);
        Ok(Bytes::from(payload))
    } else {
        info!("Binary file detected, skipping context injection for '{}'", file_path.display());
        Ok(Bytes::from(raw_contents))
    }
}

// ─────────────────── Phase 2: Orphan Cleanup ───────────────────

/// Phase 2: Iterate Redis keys, remove orphans whose files no longer exist on disk
/// OR whose files now match a filter (ragignore, OS ignore, disallowed extension).
async fn phase2_cleanup(
    con: &mut redis::aio::MultiplexedConnection,
    api: &OpenWebUiClient,
    target_dir: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ragignore_patterns = load_ragignore(target_dir);
    let mut cursor = "0".to_string();

    loop {
        let (new_cursor, items) =
            redis_client::fcall_get_cleanup_batch(con, &cursor).await?;

        for (key, path, file_id) in &items {
            let file_path = Path::new(path);
            let missing = !file_path.exists();
            let ignored = is_ragignored(file_path, target_dir, &ragignore_patterns)
                || is_os_ignored(file_path)
                || !has_allowed_extension(file_path);

            if missing || ignored {
                let reason = if missing { "missing" } else { "filtered" };
                info!("[ORPHAN:{}] {} → deleting", reason, path);

                // Delete from Open WebUI if we have a file ID
                if !file_id.is_empty() {
                    if let Err(e) = api.delete_file(file_id).await {
                        warn!("Failed to delete orphan file_id={}: {}", file_id, e);
                    }
                }

                // Delete key from Redis
                let _: () = redis::cmd("DEL")
                    .arg(key)
                    .query_async(con)
                    .await?;
            }
        }

        if new_cursor == "0" {
            break;
        }
        cursor = new_cursor;
    }

    Ok(())
}

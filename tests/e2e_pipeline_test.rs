//! End-to-end integration test for the OmniRAG pipeline.
//!
//! This test validates the full lifecycle: Phase 0 (reconciliation), Phase 1 (ingestion),
//! Phase 2 (orphan cleanup), the `context_dirty` flag, binary-safe context injection,
//! and `.ragignore` orphan trap.
//!
//! **Requires:** A local Redis server at `redis://127.0.0.1:6379/0`.

#[cfg(test)]
mod tests {
    use omnirag::config::AppConfig;
    use omnirag::hashing::generate_redis_key;
    use omnirag::redis_client;
    use omnirag::sync;

    use std::io::Write;

    /// Helper: try to connect to Redis; return None if unavailable.
    async fn try_bootstrap_redis() -> Option<redis::aio::MultiplexedConnection> {
        let client = redis::Client::open("redis://127.0.0.1:6379/0").ok()?;
        let mut con = client.get_multiplexed_async_connection().await.ok()?;

        // Isolate: flush the entire DB
        let _: () = redis::cmd("FLUSHDB")
            .query_async(&mut con)
            .await
            .ok()?;

        // Load Lua functions
        redis_client::load_functions(&mut con).await.ok()?;

        Some(con)
    }

    /// Helper: count Redis keys that are NOT `config:global`.
    async fn count_tracked_keys(con: &mut redis::aio::MultiplexedConnection) -> usize {
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg("*")
            .query_async(con)
            .await
            .unwrap_or_default();
        keys.iter().filter(|k| k.as_str() != "config:global").count()
    }

    /// Helper: read a field from a Redis hash.
    async fn hget(
        con: &mut redis::aio::MultiplexedConnection,
        key: &str,
        field: &str,
    ) -> Option<String> {
        redis::cmd("HGET")
            .arg(key)
            .arg(field)
            .query_async(con)
            .await
            .ok()
    }

    #[tokio::test]
    async fn test_e2e_full_lifecycle() {
        // Initialize tracing for test output
        let _ = tracing_subscriber::fmt()
            .with_env_filter("info,omnirag=debug")
            .with_test_writer()
            .try_init();

        // ── Setup ──────────────────────────────────────────────────
        let mut con = match try_bootstrap_redis().await {
            Some(c) => c,
            None => {
                eprintln!("⚠ Skipping E2E test: Redis not available at 127.0.0.1:6379");
                return;
            }
        };

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let target = temp_dir.path();

        // Start wiremock server
        let mock_server = wiremock::MockServer::start().await;
        let kb_id = "test-kb-00000000-0000-0000-0000-000000000000";

        // --- Mock: GET /api/v1/knowledge/{kb} (Phase 0 reconciliation) ---
        // Return empty KB so reconciliation is a no-op.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!("/api/v1/knowledge/{}", kb_id)))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"files": []})),
            )
            .expect(1..)
            .named("Phase 0: list KB files")
            .mount(&mock_server)
            .await;


        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v1/files/"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                // Each request gets a unique ID by using a closure via respond_with
                serde_json::json!({"id": "mock-file-id-placeholder"}),
            ))
            .named("Upload file")
            .mount(&mock_server)
            .await;

        // --- Mock: GET /api/v1/files/{id}/process/status ---
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(r"^/api/v1/files/.+/process/status$"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"status": "completed"})),
            )
            .named("Poll process status")
            .mount(&mock_server)
            .await;

        // --- Mock: POST /api/v1/knowledge/{kb}/file/add ---
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(format!(
                "/api/v1/knowledge/{}/file/add",
                kb_id
            )))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"status": "ok"}),
            ))
            .named("Add to KB")
            .mount(&mock_server)
            .await;

        // --- Mock: DELETE /api/v1/files/{id} ---
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path_regex(r"^/api/v1/files/.+$"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .named("Delete file")
            .mount(&mock_server)
            .await;

        // Build config pointing at temp dir + mock server
        let config = AppConfig {
            target_directory: target.to_string_lossy().to_string(),
            openwebui_url: mock_server.uri(),
            openwebui_token: "test-token".to_string(),
            openwebui_knowledge_id: kb_id.to_string(),
            redis_url: "redis://127.0.0.1:6379/0".to_string(),
            context_header_label: "File Context".to_string(),
            max_concurrent_uploads: 5,
            convert_to_markdown: false,
        };

        // Save config to Redis so Phase 0 reconciliation can read it
        config.save_to_redis(&mut con).await.expect("Failed to save config");

        // ══════════════════════════════════════════════════════════
        // STEP 1: Initial Ingestion
        // ══════════════════════════════════════════════════════════

        // Create two files: one text (doc.md), one binary (diagram.pdf)
        let doc_path = target.join("doc.md");
        let pdf_path = target.join("diagram.pdf");

        std::fs::write(&doc_path, "# Architecture\nThis is a test document.\n")
            .expect("Write doc.md");
        std::fs::write(&pdf_path, b"%PDF-1.4 binary content here")
            .expect("Write diagram.pdf");

        // Run sync
        sync::run_sync(&mut con, &config)
            .await
            .expect("Step 1: run_sync failed");

        // Assert: Redis tracks both files
        assert_eq!(count_tracked_keys(&mut con).await, 2, "Step 1: should track 2 files");

        // Assert: context_dirty is "false" for both
        let doc_key = generate_redis_key(&doc_path);
        let pdf_key = generate_redis_key(&pdf_path);
        assert_eq!(
            hget(&mut con, &doc_key, "context_dirty").await.as_deref(),
            Some("false"),
            "Step 1: doc.md context_dirty should be 'false'"
        );
        assert_eq!(
            hget(&mut con, &pdf_key, "context_dirty").await.as_deref(),
            Some("false"),
            "Step 1: diagram.pdf context_dirty should be 'false'"
        );

        // Assert: Wiremock received upload requests
        let step1_uploads: Vec<_> = mock_server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == wiremock::http::Method::POST && r.url.path() == "/api/v1/files/")
            .cloned()
            .collect();
        assert_eq!(step1_uploads.len(), 2, "Step 1: should have 2 upload requests");

        // Record the initial file IDs from Redis for later comparison
        let _doc_file_id_v1 = hget(&mut con, &doc_key, "openwebui_file_id")
            .await
            .expect("Step 1: doc.md should have openwebui_file_id");
        let pdf_file_id_v1 = hget(&mut con, &pdf_key, "openwebui_file_id")
            .await
            .expect("Step 1: diagram.pdf should have openwebui_file_id");

        // Reset wiremock request journal for clean counting
        mock_server.reset().await;

        // Re-mount all mocks after reset
        remount_mocks(&mock_server, kb_id).await;

        // ══════════════════════════════════════════════════════════
        // STEP 2: Context Dirty Flag (re-upload without file change)
        // ══════════════════════════════════════════════════════════

        // Simulate Web UI: set context and mark dirty
        redis_client::update_context_text(&mut con, &doc_key, "Test Context")
            .await
            .expect("Step 2: update_context_text failed");

        // Verify dirty flag was set
        assert_eq!(
            hget(&mut con, &doc_key, "context_dirty").await.as_deref(),
            Some("true"),
            "Step 2: doc.md should be marked dirty"
        );

        // Verify verify_file_hash returns 0 (mismatch) due to dirty flag
        let content_hash = omnirag::hashing::hash_file_contents(&doc_path)
            .expect("hash doc.md");
        let hash_matches = redis_client::fcall_verify_file_hash(&mut con, &doc_key, &content_hash)
            .await
            .expect("fcall_verify_file_hash");
        assert!(
            !hash_matches,
            "Step 2: verify_file_hash should return false when context_dirty='true'"
        );

        // Run sync again
        sync::run_sync(&mut con, &config)
            .await
            .expect("Step 2: run_sync failed");

        // Assert: doc.md was re-uploaded (DELETE old + POST new)
        let step2_requests = mock_server.received_requests().await.unwrap();
        let step2_deletes: Vec<_> = step2_requests
            .iter()
            .filter(|r| r.method == wiremock::http::Method::DELETE)
            .collect();
        let step2_uploads: Vec<_> = step2_requests
            .iter()
            .filter(|r| r.method == wiremock::http::Method::POST && r.url.path() == "/api/v1/files/")
            .collect();

        assert!(
            step2_deletes.len() >= 1,
            "Step 2: should have at least 1 DELETE (old doc.md)"
        );

        // Assert: Only doc.md was uploaded (PDF should be skipped)
        assert_eq!(
            step2_uploads.len(),
            1,
            "Step 2: should have exactly 1 upload (doc.md only, PDF skipped)"
        );

        // Assert: The upload payload contains the context string
        let upload_body = String::from_utf8_lossy(&step2_uploads[0].body);
        assert!(
            upload_body.contains("File Context"),
            "Step 2: upload payload should contain the configurable context header label"
        );
        assert!(
            upload_body.contains("Test Context"),
            "Step 2: upload payload should contain the user-set context string"
        );

        // Assert: context_dirty was reset to "false" after sync
        assert_eq!(
            hget(&mut con, &doc_key, "context_dirty").await.as_deref(),
            Some("false"),
            "Step 2: doc.md context_dirty should be reset to 'false' after sync"
        );

        // Assert: PDF was not touched (still tracks same file_id)
        assert_eq!(
            hget(&mut con, &pdf_key, "openwebui_file_id").await.as_deref(),
            Some(pdf_file_id_v1.as_str()),
            "Step 2: diagram.pdf file_id should be unchanged"
        );

        // Reset journal again
        mock_server.reset().await;
        remount_mocks(&mock_server, kb_id).await;

        // ══════════════════════════════════════════════════════════
        // STEP 3: File Modification (binary file hash change)
        // ══════════════════════════════════════════════════════════

        // Append to PDF to change its hash
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&pdf_path)
                .expect("Open PDF for append");
            file.write_all(b"\n%% MODIFIED CONTENT %%\n")
                .expect("Append to PDF");
        }

        sync::run_sync(&mut con, &config)
            .await
            .expect("Step 3: run_sync failed");

        let step3_requests = mock_server.received_requests().await.unwrap();
        let step3_deletes: Vec<_> = step3_requests
            .iter()
            .filter(|r| r.method == wiremock::http::Method::DELETE)
            .collect();
        let step3_uploads: Vec<_> = step3_requests
            .iter()
            .filter(|r| r.method == wiremock::http::Method::POST && r.url.path() == "/api/v1/files/")
            .collect();

        // Assert: PDF was re-uploaded (DELETE old + POST new)
        assert!(
            step3_deletes.len() >= 1,
            "Step 3: should DELETE old PDF"
        );
        assert_eq!(
            step3_uploads.len(),
            1,
            "Step 3: should upload exactly 1 file (diagram.pdf)"
        );

        // Assert: doc.md was not touched
        let step3_doc_uploads: Vec<_> = step3_requests
            .iter()
            .filter(|r| {
                r.method == wiremock::http::Method::POST
                    && r.url.path() == "/api/v1/files/"
                    && String::from_utf8_lossy(&r.body).contains("doc.md")
            })
            .collect();
        assert_eq!(
            step3_doc_uploads.len(),
            0,
            "Step 3: doc.md should NOT be re-uploaded"
        );

        // Reset journal
        mock_server.reset().await;
        remount_mocks(&mock_server, kb_id).await;

        // ══════════════════════════════════════════════════════════
        // STEP 4: Orphan Cleanup & .ragignore Trap
        // ══════════════════════════════════════════════════════════

        // Delete diagram.pdf from disk
        std::fs::remove_file(&pdf_path).expect("Delete diagram.pdf");

        // Create .ragignore that ignores doc.md
        std::fs::write(target.join(".ragignore"), "doc.md\n").expect("Write .ragignore");

        sync::run_sync(&mut con, &config)
            .await
            .expect("Step 4: run_sync failed");

        let step4_requests = mock_server.received_requests().await.unwrap();
        let step4_deletes: Vec<_> = step4_requests
            .iter()
            .filter(|r| r.method == wiremock::http::Method::DELETE)
            .collect();

        // Assert: Both files were deleted from Open WebUI
        // diagram.pdf: orphan (missing from disk)
        // doc.md: orphan (filtered by .ragignore)
        assert_eq!(
            step4_deletes.len(),
            2,
            "Step 4: should DELETE both orphaned files (missing + ragignored)"
        );

        // Assert: No uploads (nothing valid to ingest)
        let step4_uploads: Vec<_> = step4_requests
            .iter()
            .filter(|r| r.method == wiremock::http::Method::POST && r.url.path() == "/api/v1/files/")
            .collect();
        assert_eq!(
            step4_uploads.len(),
            0,
            "Step 4: should have zero uploads"
        );

        // Assert: Redis is empty of tracking keys
        assert_eq!(
            count_tracked_keys(&mut con).await,
            0,
            "Step 4: Redis should have zero file tracking keys"
        );

        // Cleanup
        let _: () = redis::cmd("FLUSHDB")
            .query_async(&mut con)
            .await
            .unwrap_or(());
    }

    /// Re-mount all wiremock mocks after a reset (reset clears both journal AND mocks).
    async fn remount_mocks(mock_server: &wiremock::MockServer, kb_id: &str) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(format!(
                "/api/v1/knowledge/{}",
                kb_id
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"files": []})),
            )
            .mount(mock_server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v1/files/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "mock-file-id-placeholder"})),
            )
            .mount(mock_server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path_regex(
                r"^/api/v1/files/.+/process/status$",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"status": "completed"})),
            )
            .mount(mock_server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(format!(
                "/api/v1/knowledge/{}/file/add",
                kb_id
            )))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"status": "ok"})),
            )
            .mount(mock_server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path_regex(r"^/api/v1/files/.+$"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(mock_server)
            .await;
    }
}

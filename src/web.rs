use axum::{
    extract::{State, Query},
    response::{Html, Json},
    routing::{get, post},
    Form, Router,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;
use serde::Deserialize;

use crate::config::AppConfig;

/// Shared application state for the web server.
#[derive(Clone)]
pub struct AppState {
    pub redis_con: Arc<Mutex<redis::aio::MultiplexedConnection>>,
    pub sync_status: Arc<Mutex<String>>,
}

/// Boot the axum web server on port 3000.
pub async fn serve(con: redis::aio::MultiplexedConnection) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = AppState {
        redis_con: Arc::new(Mutex::new(con)),
        sync_status: Arc::new(Mutex::new("Idle".to_string())),
    };

    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/config", get(get_config).post(save_config))
        .route("/api/sync", post(trigger_sync))
        .route("/api/sync/status", get(sync_status))
        // ── JSON REST API for automation (cron, n8n, webhooks) ──
        .route("/api/v1/health", get(api_health))
        .route("/api/v1/sync", post(api_trigger_sync))
        .route("/api/v1/sync/status", get(api_sync_status))
        // ── Swagger / OpenAPI ──
        .route("/docs", get(swagger_ui))
        .route("/api/v1/openapi.json", get(openapi_spec))
        // ── Context Manager ──
        .route("/api/contexts", get(get_contexts).post(save_context))
        // ── KB Picker ──
        .route("/api/config/fetch-kbs", post(fetch_knowledge_bases))

        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    info!("Web UI running at http://0.0.0.0:3000");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Render the main page with embedded HTMX + Tailwind.
async fn index_page(State(state): State<AppState>) -> Html<String> {
    let mut con = state.redis_con.lock().await;
    let config = AppConfig::load_from_redis(&mut con).await.unwrap_or_default();

    Html(render_full_page(&config))
}

/// GET /api/config — returns config form partial.
async fn get_config(State(state): State<AppState>) -> Html<String> {
    let mut con = state.redis_con.lock().await;
    let config = AppConfig::load_from_redis(&mut con).await.unwrap_or_default();
    Html(render_config_form(&config))
}

/// POST /api/config — save config to Redis and return updated form.
async fn save_config(
    State(state): State<AppState>,
    Form(mut config): Form<AppConfig>,
) -> Html<String> {
    let mut con = state.redis_con.lock().await;
    // Preserve hidden fields not in the form
    let existing = AppConfig::load_from_redis(&mut con).await.unwrap_or_default();
    if config.target_directory.is_empty() {
        config.target_directory = if existing.target_directory.is_empty() {
            "/rag".to_string()
        } else {
            existing.target_directory
        };
    }
    if config.redis_url.is_empty() {
        config.redis_url = if existing.redis_url.is_empty() {
            "redis://127.0.0.1:6379/0".to_string()
        } else {
            existing.redis_url
        };
    }
    if config.context_header_label.is_empty() {
        config.context_header_label = if existing.context_header_label.is_empty() {
            "File Context".to_string()
        } else {
            existing.context_header_label
        };
    }
    if config.max_concurrent_uploads == 0 {
        // 0 from parse means either user typed 0 (unlimited) or field was empty.
        // If the form sent "0", that's intentional (unlimited). But serde default
        // will be 0 for the u32, so we check the raw form value.
        // Since serde parses "" as 0 for u32, we only preserve if existing != 0.
        // Actually, 0 IS a valid user choice (unlimited). Leave as-is.
    }
    match config.save_to_redis(&mut con).await {
        Ok(_) => Html(render_config_form_with_status(&config, "✓ Configuration saved successfully", "text-emerald-400")),
        Err(e) => Html(render_config_form_with_status(&config, &format!("✗ Error: {}", e), "text-red-400")),
    }
}

/// POST /api/sync — spawn sync in background.
async fn trigger_sync(State(state): State<AppState>) -> Html<String> {
    let con_arc = state.redis_con.clone();
    let status_arc = state.sync_status.clone();

    {
        let mut status = status_arc.lock().await;
        if *status == "Running" {
            return Html(render_sync_panel("Sync already in progress…", "text-amber-400"));
        }
        *status = "Running".to_string();
    }

    tokio::spawn(async move {
        let mut con = con_arc.lock().await;
        let mut config = match AppConfig::load_from_redis(&mut con).await {
            Ok(c) => c,
            Err(e) => {
                let mut s = status_arc.lock().await;
                *s = format!("Error: {}", e);
                return;
            }
        };

        if config.target_directory.is_empty() {
            config.target_directory = "/rag".to_string();
        }

        match crate::sync::run_sync(&mut con, &config).await {
            Ok(_) => {
                let mut s = status_arc.lock().await;
                *s = "Completed successfully".to_string();
            }
            Err(e) => {
                let mut s = status_arc.lock().await;
                *s = format!("Error: {}", e);
            }
        }
    });

    Html(render_sync_panel("Sync started…", "text-sky-400"))
}

/// GET /api/sync/status — return current status.
async fn sync_status(State(state): State<AppState>) -> Html<String> {
    let status = state.sync_status.lock().await;
    let color = match status.as_str() {
        "Running" => "text-sky-400",
        s if s.starts_with("Error") => "text-red-400",
        "Completed successfully" => "text-emerald-400",
        _ => "text-zinc-400",
    };
    Html(render_sync_panel(&status, color))
}

/// Query params for the context manager (pagination + search).
#[derive(Deserialize, Default)]
struct ContextQuery {
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default)]
    search: String,
}

fn default_page() -> usize { 1 }

const CONTEXT_PAGE_SIZE: usize = 10;

/// GET /api/contexts — list all valid local files with their context (filesystem-first).
async fn get_contexts(
    Query(query): Query<ContextQuery>,
    State(state): State<AppState>,
) -> Html<String> {
    let mut con = state.redis_con.lock().await;
    let config = crate::config::AppConfig::load_from_redis(&mut con).await.unwrap_or_default();

    let target_dir = std::path::Path::new(&config.target_directory);
    if !target_dir.exists() {
        return Html(r#"<div class="text-zinc-500 text-sm text-center py-4">Target directory not found. Configure and run a sync first.</div>"#.to_string());
    }

    let ragignore_patterns = crate::sync::load_ragignore(target_dir);

    // Walk filesystem → collect valid files
    let mut files: Vec<(String, String, String)> = Vec::new(); // (key, path, context)
    for entry in walkdir::WalkDir::new(target_dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.into_path();

        // Apply all filters
        if crate::sync::is_os_ignored(&path) {
            continue;
        }
        if !crate::sync::has_allowed_extension(&path) {
            continue;
        }
        if crate::sync::is_ragignored(&path, target_dir, &ragignore_patterns) {
            continue;
        }
        // Skip hidden files
        let rel = path.strip_prefix(target_dir).unwrap_or(&path);
        if rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
            continue;
        }

        let key = crate::hashing::generate_redis_key(&path);
        let abs_path = path.to_string_lossy().to_string();

        // Look up existing context from Redis (may not exist yet for unsynced files)
        let ctx: Option<String> = redis::cmd("HGET")
            .arg(&key)
            .arg("context_text")
            .query_async(&mut *con)
            .await
            .unwrap_or(None);

        files.push((key, abs_path, ctx.unwrap_or_default()));
    }

    files.sort_by(|a, b| a.1.cmp(&b.1));

    // Apply search filter
    let search = query.search.to_lowercase();
    let filtered: Vec<&(String, String, String)> = if search.is_empty() {
        files.iter().collect()
    } else {
        files.iter().filter(|(_, path, _)| path.to_lowercase().contains(&search)).collect()
    };

    let total = filtered.len();
    let total_pages = if total == 0 { 1 } else { (total + CONTEXT_PAGE_SIZE - 1) / CONTEXT_PAGE_SIZE };
    let page = query.page.max(1).min(total_pages);
    let start = (page - 1) * CONTEXT_PAGE_SIZE;
    let page_items: Vec<(String, String, String)> = filtered
        .into_iter()
        .skip(start)
        .take(CONTEXT_PAGE_SIZE)
        .cloned()
        .collect();

    Html(render_context_list(&page_items, page, total_pages, total, &query.search))
}

/// POST /api/contexts — update context_text for a file.
async fn save_context(
    State(state): State<AppState>,
    Form(params): Form<std::collections::HashMap<String, String>>,
) -> Html<String> {
    let key = params.get("key").cloned().unwrap_or_default();
    let context = params.get("context_text").cloned().unwrap_or_default();

    if key.is_empty() {
        return Html(r#"<span class="text-red-400 text-xs fade-in">✗ Missing key</span>"#.to_string());
    }

    let mut con = state.redis_con.lock().await;
    match crate::redis_client::update_context_text(&mut con, &key, &context).await {
        Ok(_) => Html(format!(
            r#"<span class="text-emerald-400 text-xs fade-in">✓ Context saved</span>"#
        )),
        Err(e) => Html(format!(
            r#"<span class="text-red-400 text-xs fade-in">✗ Error: {}</span>"#, e
        )),
    }
}

// ──────────────────────── JSON REST API (v1) ────────────────────────

/// GET /api/v1/health — liveness probe for monitoring.
async fn api_health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "service": "omnirag",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// POST /api/v1/sync — trigger a sync and return JSON.
/// Designed for cron jobs, n8n workflows, and webhook integrations.
///
/// Returns:
/// - 202 Accepted: `{"status": "triggered", "message": "Sync started"}`
/// - 409 Conflict:  `{"status": "already_running", "message": "..."}`
async fn api_trigger_sync(
    State(state): State<AppState>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let con_arc = state.redis_con.clone();
    let status_arc = state.sync_status.clone();

    {
        let mut status = status_arc.lock().await;
        if *status == "Running" {
            return (
                axum::http::StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "status": "already_running",
                    "message": "A sync operation is already in progress"
                })),
            );
        }
        *status = "Running".to_string();
    }

    tokio::spawn(async move {
        let mut con = con_arc.lock().await;
        let mut config = match AppConfig::load_from_redis(&mut con).await {
            Ok(c) => c,
            Err(e) => {
                let mut s = status_arc.lock().await;
                *s = format!("Error: {}", e);
                return;
            }
        };

        if config.target_directory.is_empty() {
            config.target_directory = "/rag".to_string();
        }

        match crate::sync::run_sync(&mut con, &config).await {
            Ok(_) => {
                let mut s = status_arc.lock().await;
                *s = "Completed successfully".to_string();
            }
            Err(e) => {
                let mut s = status_arc.lock().await;
                *s = format!("Error: {}", e);
            }
        }
    });

    (
        axum::http::StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "triggered",
            "message": "Sync started"
        })),
    )
}

/// GET /api/v1/sync/status — poll sync status as JSON.
async fn api_sync_status(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let status = state.sync_status.lock().await;
    let phase = match status.as_str() {
        "Running" => "running",
        "Completed successfully" => "completed",
        s if s.starts_with("Error") => "error",
        "Idle" => "idle",
        _ => "unknown",
    };
    Json(serde_json::json!({
        "status": phase,
        "detail": *status
    }))
}

// ──────────────────────── Swagger / OpenAPI ────────────────────────

/// GET /docs — Swagger UI page with dark theme.
async fn swagger_ui() -> Html<String> {
    Html(r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>OmniRAG — API Docs</title>
    <link rel="stylesheet" href="https://unpkg.com/swagger-ui-dist@5/swagger-ui.css">
    <style>
        body { margin: 0; background: #1a1a2e; }
        .swagger-ui .topbar { display: none; }
        /* Dark mode overrides */
        .swagger-ui { color: #e0e0e0; }
        .swagger-ui .info .title, .swagger-ui .info h1,
        .swagger-ui .info h2, .swagger-ui .info h3 { color: #f0f0f0; }
        .swagger-ui .info p, .swagger-ui .info li,
        .swagger-ui .info a { color: #c0c0c0; }
        .swagger-ui .scheme-container { background: #16213e; box-shadow: none; }
        .swagger-ui .opblock-tag { color: #e0e0e0; border-bottom-color: #333; }
        .swagger-ui .opblock-tag:hover { color: #ffffff; }
        .swagger-ui .opblock .opblock-summary-description { color: #b0b0b0; }
        .swagger-ui .opblock .opblock-summary-operation-id,
        .swagger-ui .opblock .opblock-summary-path,
        .swagger-ui .opblock .opblock-summary-path__deprecated { color: #e0e0e0; }
        .swagger-ui .opblock-description-wrapper p,
        .swagger-ui .opblock-external-docs-wrapper p,
        .swagger-ui .opblock-title_normal p { color: #c0c0c0; }
        .swagger-ui table thead tr th, .swagger-ui table thead tr td,
        .swagger-ui .parameter__name, .swagger-ui .parameter__type,
        .swagger-ui .parameter__in { color: #e0e0e0; }
        .swagger-ui .response-col_status { color: #e0e0e0; }
        .swagger-ui .response-col_description,
        .swagger-ui .response-col_description p { color: #c0c0c0; }
        .swagger-ui .model-title, .swagger-ui .model { color: #e0e0e0; }
        .swagger-ui .model-toggle::after { background: url("data:image/svg+xml;charset=utf-8,%3Csvg xmlns='http://www.w3.org/2000/svg' width='24' height='24'%3E%3Cpath d='M10 6L8.59 7.41 13.17 12l-4.58 4.59L10 18l6-6z' fill='%23e0e0e0'/%3E%3C/svg%3E") center no-repeat; }
        .swagger-ui section.models { border-color: #333; }
        .swagger-ui section.models h4 { color: #e0e0e0; }
        .swagger-ui .model-box { background: #1e2a4a; }
        .swagger-ui .prop-type { color: #7fdbca; }
        .swagger-ui .prop-format { color: #c792ea; }
        .swagger-ui input[type=text], .swagger-ui textarea,
        .swagger-ui select { background: #16213e; color: #e0e0e0; border-color: #444; }
        .swagger-ui .btn { color: #e0e0e0; }
        .swagger-ui .opblock-body pre.microlight { background: #0f1729 !important; color: #c0c0c0 !important; }
        .swagger-ui .highlight-code .microlight code { color: #c0c0c0 !important; }
        .swagger-ui .markdown p, .swagger-ui .markdown li { color: #c0c0c0; }
        .swagger-ui .renderedMarkdown p { color: #c0c0c0; }
    </style>
</head>
<body>
    <div id="swagger-ui"></div>
    <script src="https://unpkg.com/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
        SwaggerUIBundle({
            url: '/api/v1/openapi.json',
            dom_id: '#swagger-ui',
            deepLinking: true,
            presets: [SwaggerUIBundle.presets.apis, SwaggerUIBundle.SwaggerUIStandalonePreset],
            layout: 'BaseLayout'
        });
    </script>
</body>
</html>"#.to_string())
}

/// GET /api/v1/openapi.json — OpenAPI 3.0 specification.
async fn openapi_spec() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "openapi": "3.0.3",
        "info": {
            "title": "OmniRAG API",
            "description": "REST API for the OmniRAG file synchronization engine. Designed for cron jobs, n8n workflows, webhooks, and external automation.",
            "version": "0.1.0",
            "license": { "name": "MIT" }
        },
        "servers": [
            { "url": "http://localhost:3000", "description": "Local development" }
        ],
        "paths": {
            "/api/v1/health": {
                "get": {
                    "summary": "Health Check",
                    "description": "Liveness probe. Returns service name and version.",
                    "operationId": "healthCheck",
                    "tags": ["System"],
                    "responses": {
                        "200": {
                            "description": "Service is healthy",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {
                                            "status": { "type": "string", "example": "ok" },
                                            "service": { "type": "string", "example": "omnirag" },
                                            "version": { "type": "string", "example": "0.1.0" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "/api/v1/sync": {
                "post": {
                    "summary": "Trigger Sync",
                    "description": "Start a background sync operation. Runs Phase 0 (Reconciliation), Phase 1 (Ingestion), and Phase 2 (Orphan Cleanup). Returns immediately — poll `/api/v1/sync/status` for progress.",
                    "operationId": "triggerSync",
                    "tags": ["Sync"],
                    "responses": {
                        "202": {
                            "description": "Sync started successfully",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {
                                            "status": { "type": "string", "example": "triggered" },
                                            "message": { "type": "string", "example": "Sync started" }
                                        }
                                    }
                                }
                            }
                        },
                        "409": {
                            "description": "A sync is already in progress",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {
                                            "status": { "type": "string", "example": "already_running" },
                                            "message": { "type": "string", "example": "A sync operation is already in progress" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
            "/api/v1/sync/status": {
                "get": {
                    "summary": "Sync Status",
                    "description": "Poll the current sync status. Use this after triggering a sync to monitor progress.",
                    "operationId": "syncStatus",
                    "tags": ["Sync"],
                    "responses": {
                        "200": {
                            "description": "Current sync status",
                            "content": {
                                "application/json": {
                                    "schema": {
                                        "type": "object",
                                        "properties": {
                                            "status": {
                                                "type": "string",
                                                "enum": ["idle", "running", "completed", "error", "unknown"],
                                                "example": "idle"
                                            },
                                            "detail": { "type": "string", "example": "Idle" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
        "tags": [
            { "name": "System", "description": "Health and diagnostics" },
            { "name": "Sync", "description": "File synchronization operations" }
        ]
    }))
}

// ──────────────────────── HTML Rendering ────────────────────────

fn render_full_page(config: &AppConfig) -> String {
    format!(r##"<!DOCTYPE html>
<html lang="en" class="dark">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>OmniRAG — Control Panel</title>
    <meta name="description" content="OmniRAG control panel for managing Open WebUI file ingestion and synchronization">
    <script src="https://unpkg.com/htmx.org@1.9.12"></script>
    <script src="https://cdn.tailwindcss.com"></script>
    <script>
        tailwind.config = {{
            darkMode: 'class',
            theme: {{
                extend: {{
                    colors: {{
                        surface: {{ 50: '#18181b', 100: '#27272a', 200: '#3f3f46', 300: '#52525b' }},
                        accent: {{ 400: '#38bdf8', 500: '#0ea5e9', 600: '#0284c7' }},
                    }}
                }}
            }}
        }}
    </script>
    <style>
        body {{ background: linear-gradient(135deg, #09090b 0%, #18181b 50%, #0c0a1a 100%); }}
        .glass {{ background: rgba(39, 39, 42, 0.6); backdrop-filter: blur(16px); border: 1px solid rgba(63, 63, 70, 0.5); }}
        .glow {{ box-shadow: 0 0 40px rgba(14, 165, 233, 0.08); }}
        input:focus {{ box-shadow: 0 0 0 2px rgba(56, 189, 248, 0.3); }}
        .fade-in {{ animation: fadeIn 0.3s ease-in-out; }}
        @keyframes fadeIn {{ from {{ opacity: 0; transform: translateY(-4px); }} to {{ opacity: 1; transform: translateY(0); }} }}
        .pulse-dot {{ animation: pulse 2s infinite; }}
        @keyframes pulse {{ 0%, 100% {{ opacity: 1; }} 50% {{ opacity: 0.4; }} }}
        /* Help dialog modal */
        .help-overlay {{ position: fixed; inset: 0; background: rgba(0,0,0,0.6); backdrop-filter: blur(4px); z-index: 50; display: flex; align-items: center; justify-content: center; animation: fadeIn 0.15s ease-out; }}
        .help-dialog {{ background: rgba(39,39,42,0.95); backdrop-filter: blur(20px); border: 1px solid rgba(63,63,70,0.6); border-radius: 1rem; padding: 1.5rem; max-width: 28rem; width: 90%; box-shadow: 0 25px 50px rgba(0,0,0,0.5), 0 0 40px rgba(14,165,233,0.1); animation: fadeIn 0.2s ease-out; }}
        .help-btn {{ display: inline-flex; align-items: center; justify-content: center; width: 18px; height: 18px; border-radius: 50%; background: rgba(63,63,70,0.6); color: rgba(161,161,170,0.8); font-size: 11px; font-weight: 600; cursor: pointer; border: 1px solid rgba(82,82,91,0.5); transition: all 0.2s; flex-shrink: 0; }}
        .help-btn:hover {{ background: rgba(14,165,233,0.2); color: #38bdf8; border-color: rgba(14,165,233,0.4); }}
        /* Context Manager dialog */
        .ctx-dialog {{ background: rgba(24,24,27,0.97); backdrop-filter: blur(24px); border: 1px solid rgba(63,63,70,0.5); border-radius: 1.25rem; padding: 1.5rem; max-width: 56rem; width: 95%; max-height: 85vh; display: flex; flex-direction: column; box-shadow: 0 25px 50px rgba(0,0,0,0.6), 0 0 60px rgba(14,165,233,0.08); animation: fadeIn 0.2s ease-out; }}
        .ctx-dialog #context-list {{ overflow-y: auto; flex: 1; }}
        .ctx-search {{ width: 100%; padding: 0.625rem 1rem; border-radius: 0.75rem; background: rgba(39,39,42,0.8); border: 1px solid rgba(63,63,70,0.6); color: #e4e4e7; font-size: 0.8125rem; outline: none; transition: border-color 0.2s; }}
        .ctx-search:focus {{ border-color: #0ea5e9; }}
        .ctx-page-btn {{ padding: 0.375rem 0.75rem; border-radius: 0.5rem; background: rgba(63,63,70,0.4); color: #a1a1aa; font-size: 0.75rem; font-weight: 500; border: 1px solid rgba(63,63,70,0.5); cursor: pointer; transition: all 0.2s; }}
        .ctx-page-btn:hover {{ background: rgba(63,63,70,0.7); color: #e4e4e7; }}
        .ctx-page-btn.active {{ background: rgba(14,165,233,0.2); color: #38bdf8; border-color: rgba(14,165,233,0.4); }}
        .ctx-page-btn:disabled {{ opacity: 0.3; cursor: not-allowed; }}
    </style>
    <script>
        function showHelp(title, body) {{
            var overlay = document.createElement('div');
            overlay.className = 'help-overlay';
            overlay.onclick = function(e) {{ if (e.target === overlay) overlay.remove(); }};
            overlay.innerHTML = '<div class="help-dialog">' +
                '<div class="flex items-center justify-between mb-3">' +
                '<h3 class="text-sm font-semibold text-zinc-100">' + title + '</h3>' +
                '<button onclick="this.closest(\'.help-overlay\').remove()" class="text-zinc-500 hover:text-zinc-300 text-lg leading-none">&times;</button>' +
                '</div>' +
                '<div class="text-xs text-zinc-400 leading-relaxed space-y-2">' + body + '</div>' +
                '</div>';
            document.body.appendChild(overlay);
        }}
        function openContextManager() {{
            document.getElementById('ctx-modal').style.display = 'flex';
            document.body.dispatchEvent(new Event('ctxOpen'));
        }}
        function closeContextManager() {{
            document.getElementById('ctx-modal').style.display = 'none';
        }}
        function ctxSearch(input) {{
            var val = input.value;
            htmx.ajax('GET', '/api/contexts?search=' + encodeURIComponent(val), {{target: '#context-list', swap: 'innerHTML'}});
        }}
        function ctxPage(page, search) {{
            htmx.ajax('GET', '/api/contexts?page=' + page + '&search=' + encodeURIComponent(search || ''), {{target: '#context-list', swap: 'innerHTML'}});
        }}
    </script>
    <script>void function(){{var _=String.fromCharCode;console.log('%c'+_(9835)+_(9838)+' '+_(129354)+' '+_(9733),'font-size:1px;color:transparent')}}()</script>
</head>
<body class="min-h-screen text-zinc-100 font-sans antialiased">
    <main data-cjc="0730" data-mac="1003" class="max-w-2xl mx-auto px-6 py-12">
        <!-- Header -->
        <div class="mb-10">
            <div class="flex items-center gap-3 mb-2">
                <div class="w-2 h-2 rounded-full bg-sky-400 pulse-dot"></div>
                <h1 class="text-2xl font-semibold tracking-tight text-zinc-50">OmniRAG</h1>
                <a href="/docs" class="ml-auto text-xs px-3 py-1.5 rounded-lg bg-zinc-700/50 hover:bg-zinc-600/50 text-zinc-400 hover:text-sky-400 transition-colors" title="API Documentation">📄 Swagger Docs</a>
            </div>
            <p class="text-sm text-zinc-500 ml-5">File ingestion & synchronization control panel</p>
        </div>

        <!-- Config Card -->
        <div class="glass rounded-2xl p-8 glow mb-6">
            <div class="flex items-center justify-between mb-6">
                <h2 class="text-lg font-medium text-zinc-200">Configuration</h2>
                <span class="text-xs px-2.5 py-1 rounded-full bg-zinc-700/50 text-zinc-400">config:global</span>
            </div>
            <div id="config-form">
                {config_form}
            </div>
        </div>

        <!-- Sync Card -->
        <div class="glass rounded-2xl p-8 glow">
            <div class="flex items-center justify-between mb-6">
                <h2 class="text-lg font-medium text-zinc-200">Sync Control</h2>
            </div>
            <div class="flex items-center gap-4">
                <button
                    hx-post="/api/sync"
                    hx-target="#sync-status"
                    hx-swap="innerHTML"
                    class="px-5 py-2.5 rounded-xl bg-sky-600 hover:bg-sky-500 text-white text-sm font-medium transition-all duration-200 hover:shadow-lg hover:shadow-sky-600/20 active:scale-95"
                >
                    ▶ Trigger Sync
                </button>
                <div id="sync-status"
                     hx-get="/api/sync/status"
                     hx-trigger="every 3s"
                     hx-swap="innerHTML"
                     class="text-sm">
                    {sync_panel}
                </div>
            </div>
        </div>

        <!-- Context Manager Button -->
        <div class="glass rounded-2xl p-6 glow mt-6 text-center">
            <button onclick="openContextManager()"
                    class="px-6 py-3 rounded-xl bg-zinc-700 hover:bg-zinc-600 text-zinc-200 text-sm font-medium transition-all duration-200 active:scale-[0.98] inline-flex items-center gap-2">
                📋 Open Context Manager
                <span class="help-btn" onclick="event.stopPropagation(); showHelp('Context Manager', '<p>Set per-file context strings that are prepended to documents during upload.</p><p class=&quot;mt-2&quot;>The context header appears as a Markdown blockquote above the file content, helping the LLM understand what each document is about.</p><p class=&quot;mt-2&quot;>Changes take effect on the next sync cycle.</p>')">?</span>
            </button>
        </div>

        <!-- Context Manager Modal -->
        <div id="ctx-modal" class="help-overlay" style="display:none" onclick="if(event.target===this)closeContextManager()">
            <div class="ctx-dialog">
                <div class="flex items-center justify-between mb-4">
                    <h2 class="text-lg font-semibold text-zinc-100">Context Manager</h2>
                    <button onclick="closeContextManager()" class="text-zinc-400 hover:text-zinc-200 text-xl leading-none">&times;</button>
                </div>
                <div id="context-list"
                     hx-get="/api/contexts"
                     hx-trigger="ctxOpen from:body"
                     hx-swap="innerHTML">
                    <span class="text-zinc-500 text-sm">Click Open to load files…</span>
                </div>
            </div>
        </div>

        <!-- Footer -->
        <div class="mt-8 text-center text-xs text-zinc-600 space-y-1">
            <div>OmniRAG v0.1.0 · Built by <span class="text-zinc-500">Kristopher Clark</span></div>
            <div>
                <a href="https://github.com/krisclarkdev/omnirag" target="_blank" rel="noopener" class="text-zinc-500 hover:text-sky-400 transition-colors">⬡ GitHub</a>
                <span class="mx-1">·</span>
                <a href="/docs" class="text-zinc-500 hover:text-sky-400 transition-colors">📄 API Docs</a>
            </div>
        </div>
    </main>


</body>
</html>"##,
        config_form = render_config_form(config),
        sync_panel = render_sync_panel("Idle", "text-zinc-400"),
    )
}

fn render_config_form(config: &AppConfig) -> String {
    render_config_form_with_status(config, "", "")
}

fn render_config_form_with_status(config: &AppConfig, message: &str, color: &str) -> String {
    let status_html = if message.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="fade-in mb-4 text-sm {color}">{message}</div>"#)
    };

    format!(r##"
{status_html}
<form hx-post="/api/config" hx-target="#config-form" hx-swap="innerHTML" class="space-y-4">
    <div>
        <div class="flex items-center gap-1.5 mb-1.5">
            <label class="block text-xs font-medium text-zinc-400">Open WebUI URL</label>
            <span class="help-btn" onclick="showHelp('Open WebUI URL', '<p>The base URL of your Open WebUI instance, e.g. <code class=&quot;text-sky-400&quot;>https://chat.example.com</code>.</p><p class=&quot;mt-2&quot;>This is the same URL you use to access the Open WebUI chat interface in your browser. Do not include a trailing slash or any path segments.</p>')">?</span>
        </div>
        <input type="text" name="openwebui_url" value="{openwebui_url}"
               class="w-full px-4 py-2.5 rounded-xl bg-zinc-800/80 border border-zinc-700 text-zinc-100 text-sm placeholder-zinc-500 focus:outline-none focus:border-sky-500 transition-colors"
               placeholder="https://your-openwebui-instance.com" />
    </div>
    <div>
        <div class="flex items-center gap-1.5 mb-1.5">
            <label class="block text-xs font-medium text-zinc-400">API Token</label>
            <span class="help-btn" onclick="showHelp('API Token', '<p>A bearer token used to authenticate API requests to Open WebUI.</p><p class=&quot;mt-2&quot;><strong>How to get it:</strong> In Open WebUI, go to <em>Settings → Account → API Keys</em> and generate a new key. It typically starts with <code class=&quot;text-sky-400&quot;>sk-...</code>.</p><p class=&quot;mt-2&quot;>This token must have permission to upload files and manage knowledge bases.</p>')">?</span>
        </div>
        <input type="password" name="openwebui_token" value="{openwebui_token}"
               class="w-full px-4 py-2.5 rounded-xl bg-zinc-800/80 border border-zinc-700 text-zinc-100 text-sm placeholder-zinc-500 focus:outline-none focus:border-sky-500 transition-colors"
               placeholder="sk-..." />
    </div>
    <div>
        <div class="flex items-center gap-1.5 mb-1.5">
            <label class="block text-xs font-medium text-zinc-400">Knowledge Base</label>
            <span class="help-btn" onclick="showHelp('Knowledge Base', '<p>The Open WebUI Knowledge Base that OmniRAG will sync files into.</p><p class=&quot;mt-2&quot;><strong>How to get it:</strong> Enter your URL and Token above, then click <em>⟳ Fetch KBs</em> to load a dropdown of available Knowledge Bases. Alternatively, find the KB UUID in the Open WebUI URL when viewing a knowledge base.</p><p class=&quot;mt-2&quot;>Each OmniRAG container should point to exactly one Knowledge Base.</p>')">?</span>
        </div>
        <div id="kb-picker-container">
            {kb_picker}
        </div>
    </div>
    <div>
        <div class="flex items-center gap-1.5 mb-1.5">
            <label class="block text-xs font-medium text-zinc-400">Context Header Label</label>
            <span class="help-btn" onclick="showHelp('Context Header Label', '<p>A customizable label prepended to text files during upload as a Markdown blockquote header.</p><p class=&quot;mt-2&quot;>For example, if set to <code class=&quot;text-sky-400&quot;>Project Docs</code>, files with context will be prepended with:</p><p class=&quot;mt-1 text-zinc-300 font-mono text-xs&quot;>&gt; <strong>Project Docs:</strong><br/>&gt; your context here</p><p class=&quot;mt-2&quot;>This helps the LLM understand the source and purpose of each document.</p>')">?</span>
        </div>
        <input type="text" name="context_header_label" value="{context_header_label}"
               class="w-full px-4 py-2.5 rounded-xl bg-zinc-800/80 border border-zinc-700 text-zinc-100 text-sm placeholder-zinc-500 focus:outline-none focus:border-sky-500 transition-colors"
               placeholder="File Context" />
    </div>
    <div>
        <div class="flex items-center gap-1.5 mb-1.5">
            <label class="block text-xs font-medium text-zinc-400">Max Concurrent Uploads</label>
            <span class="help-btn" onclick="showHelp('Max Concurrent Uploads', '<p>Controls how many files are uploaded to Open WebUI simultaneously during Phase 1 ingestion.</p><p class=&quot;mt-2&quot;>Set to <code class=&quot;text-sky-400&quot;>0</code> for unlimited concurrency (all files upload at once). Higher values speed up sync but may overwhelm slower Open WebUI instances.</p><p class=&quot;mt-2&quot;>Default is <code class=&quot;text-sky-400&quot;>5</code>.</p>')">?</span>
        </div>
        <input type="number" name="max_concurrent_uploads" value="{max_concurrent_uploads}" min="0"
               class="w-full px-4 py-2.5 rounded-xl bg-zinc-800/80 border border-zinc-700 text-zinc-100 text-sm placeholder-zinc-500 focus:outline-none focus:border-sky-500 transition-colors"
               placeholder="5" />
    </div>
    <div class="flex items-center gap-3 py-1">
        <input type="hidden" name="convert_to_markdown" value="false" />
        <input type="checkbox" name="convert_to_markdown" value="true" {convert_to_md_checked}
               id="convert-to-markdown"
               class="w-4 h-4 rounded bg-zinc-800 border-zinc-600 text-sky-500 focus:ring-sky-500 focus:ring-offset-0 cursor-pointer" />
        <div class="flex items-center gap-1.5">
            <label for="convert-to-markdown" class="text-xs font-medium text-zinc-400 cursor-pointer">Convert to Markdown</label>
            <span class="help-btn" onclick="showHelp('Convert to Markdown', '<p>When enabled, all non-markdown text files (e.g. <code class=&quot;text-sky-400&quot;>.py</code>, <code class=&quot;text-sky-400&quot;>.rs</code>, <code class=&quot;text-sky-400&quot;>.json</code>) are wrapped in a language-specific code fence and uploaded with a <code class=&quot;text-sky-400&quot;>.md</code> extension.</p><p class=&quot;mt-2&quot;>This improves LLM chunking and readability. Files already in <code class=&quot;text-sky-400&quot;>.md</code> or <code class=&quot;text-sky-400&quot;>.txt</code> format are uploaded as-is. Binary files (PDF) are never affected.</p>')">?</span>
        </div>
    </div>
    <div class="pt-2">
        <button type="submit"
                class="w-full px-5 py-2.5 rounded-xl bg-zinc-700 hover:bg-zinc-600 text-zinc-200 text-sm font-medium transition-all duration-200 active:scale-[0.98]">
            Save Configuration
        </button>
    </div>
</form>"##,
        openwebui_url = config.openwebui_url,
        openwebui_token = config.openwebui_token,
        context_header_label = config.context_header_label,
        max_concurrent_uploads = config.max_concurrent_uploads,
        convert_to_md_checked = if config.convert_to_markdown { "checked" } else { "" },
        kb_picker = render_kb_picker(&config.openwebui_knowledge_id),
    )
}

/// Render the KB picker: a text input with a "Fetch" button.
/// If there's already a saved KB ID, show it and still allow fetching.
fn render_kb_picker(current_kb_id: &str) -> String {
    format!(r##"
        <div class="flex gap-2">
            <input type="text" name="openwebui_knowledge_id" value="{current_kb_id}"
                   id="kb-id-input"
                   class="flex-1 px-4 py-2.5 rounded-xl bg-zinc-800/80 border border-zinc-700 text-zinc-100 text-sm placeholder-zinc-500 focus:outline-none focus:border-sky-500 transition-colors"
                   placeholder="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" />
            <button type="button"
                    hx-post="/api/config/fetch-kbs"
                    hx-include="[name='openwebui_url'], [name='openwebui_token'], [name='openwebui_knowledge_id']"
                    hx-target="#kb-picker-container"
                    hx-swap="innerHTML"
                    class="px-4 py-2.5 rounded-xl bg-sky-600/80 hover:bg-sky-500 text-white text-xs font-medium transition-all duration-200 whitespace-nowrap active:scale-95"
            >⟳ Fetch KBs</button>
        </div>
        <p class="text-xs text-zinc-600 mt-1">Enter credentials above, then click Fetch to load available Knowledge Bases</p>
    "##)
}

/// POST /api/config/fetch-kbs — query Open WebUI for available knowledge bases
/// and return an HTML <select> dropdown to swap into the form.
async fn fetch_knowledge_bases(
    Form(params): Form<std::collections::HashMap<String, String>>,
) -> Html<String> {
    let url = params.get("openwebui_url").cloned().unwrap_or_default();
    let token = params.get("openwebui_token").cloned().unwrap_or_default();
    let current_id = params.get("openwebui_knowledge_id").cloned().unwrap_or_default();

    if url.is_empty() || token.is_empty() {
        return Html(format!(r##"
            <div class="text-red-400 text-xs fade-in mb-2">✗ Enter the Open WebUI URL and API Token first</div>
            {}"##, render_kb_picker_input(&current_id)));
    }

    match crate::api::list_all_knowledge_bases(&url, &token).await {
        Ok(kbs) if kbs.is_empty() => {
            Html(format!(r##"
                <div class="text-amber-400 text-xs fade-in mb-2">⚠ No knowledge bases found. Create one in Open WebUI first.</div>
                {}"##, render_kb_picker_input(&current_id)))
        }
        Ok(kbs) => {
            let mut options = String::from(r#"<option value="" disabled>— Select a Knowledge Base —</option>"#);
            for kb in &kbs {
                let selected = if kb.id == current_id { " selected" } else { "" };
                let label = if kb.name.is_empty() {
                    kb.id.clone()
                } else if kb.description.is_empty() {
                    kb.name.clone()
                } else {
                    format!("{} — {}", kb.name, kb.description)
                };
                options.push_str(&format!(
                    r#"<option value="{id}"{selected}>{label}</option>"#,
                    id = kb.id, label = label
                ));
            }
            Html(format!(r##"
                <div class="text-emerald-400 text-xs fade-in mb-2">✓ Found {} knowledge base(s)</div>
                <div class="flex gap-2">
                    <select name="openwebui_knowledge_id"
                            class="flex-1 px-4 py-2.5 rounded-xl bg-zinc-800/80 border border-zinc-700 text-zinc-100 text-sm focus:outline-none focus:border-sky-500 transition-colors appearance-none">
                        {options}
                    </select>
                    <button type="button"
                            hx-post="/api/config/fetch-kbs"
                            hx-include="[name='openwebui_url'], [name='openwebui_token'], [name='openwebui_knowledge_id']"
                            hx-target="#kb-picker-container"
                            hx-swap="innerHTML"
                            class="px-4 py-2.5 rounded-xl bg-zinc-700/50 hover:bg-zinc-600/50 text-zinc-400 hover:text-zinc-200 text-xs transition-colors whitespace-nowrap active:scale-95"
                    >⟳</button>
                </div>"##, kbs.len()))
        }
        Err(e) => {
            Html(format!(r##"
                <div class="text-red-400 text-xs fade-in mb-2">✗ {error}</div>
                {input}"##,
                error = e,
                input = render_kb_picker_input(&current_id)))
        }
    }
}

/// Render just the text input + Fetch button (used as fallback).
fn render_kb_picker_input(current_kb_id: &str) -> String {
    format!(r##"
        <div class="flex gap-2">
            <input type="text" name="openwebui_knowledge_id" value="{current_kb_id}"
                   class="flex-1 px-4 py-2.5 rounded-xl bg-zinc-800/80 border border-zinc-700 text-zinc-100 text-sm placeholder-zinc-500 focus:outline-none focus:border-sky-500 transition-colors"
                   placeholder="xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx" />
            <button type="button"
                    hx-post="/api/config/fetch-kbs"
                    hx-include="[name='openwebui_url'], [name='openwebui_token'], [name='openwebui_knowledge_id']"
                    hx-target="#kb-picker-container"
                    hx-swap="innerHTML"
                    class="px-4 py-2.5 rounded-xl bg-sky-600/80 hover:bg-sky-500 text-white text-xs font-medium transition-all duration-200 whitespace-nowrap active:scale-95"
            >⟳ Fetch KBs</button>
        </div>
    "##)
}

fn render_sync_panel(message: &str, color: &str) -> String {
    format!(r#"<span class="fade-in {color}">{message}</span>"#)
}

fn render_context_list(
    files: &[(String, String, String)],
    page: usize,
    total_pages: usize,
    total: usize,
    search: &str,
) -> String {
    let escaped_search = search.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");

    let mut html = format!(r#"
        <div class="mb-4">
            <input type="text" class="ctx-search" placeholder="Search files…" value="{escaped_search}"
                   oninput="clearTimeout(window._ctxTimer); window._ctxTimer=setTimeout(()=>ctxSearch(this),300)" />
        </div>
        <div class="text-xs text-zinc-500 mb-3">{total} file{s} found · Page {page} of {total_pages}</div>
    "#,
        total = total,
        s = if total == 1 { "" } else { "s" },
        page = page,
        total_pages = total_pages,
    );

    if files.is_empty() {
        html.push_str(r#"<div class="text-zinc-500 text-sm text-center py-4">No files match your search.</div>"#);
    } else {
        html.push_str(r#"<div class="space-y-3">"#);

        for (key, path, context) in files {
            // Extract just the filename for display
            let filename = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());

            // HTML-escape values for safety
            let escaped_path = path.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            let escaped_context = context.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
            let escaped_key = key.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
            let escaped_filename = filename.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");

            html.push_str(&format!(r##"
<div class="rounded-xl bg-zinc-800/50 border border-zinc-700/50 p-4">
    <div class="flex items-center justify-between mb-2">
        <div class="min-w-0">
            <span class="text-sm font-medium text-zinc-200">{escaped_filename}</span>
            <p class="text-xs text-zinc-500 truncate max-w-lg" title="{escaped_path}">{escaped_path}</p>
        </div>
        <div id="ctx-status-{escaped_key}" class="min-w-[100px] text-right shrink-0"></div>
    </div>
    <form hx-post="/api/contexts" hx-target="#ctx-status-{escaped_key}" hx-swap="innerHTML" class="flex gap-2 items-end">
        <input type="hidden" name="key" value="{escaped_key}" />
        <textarea name="context_text" rows="2"
            class="flex-1 px-3 py-2 rounded-lg bg-zinc-900/80 border border-zinc-700 text-zinc-100 text-xs placeholder-zinc-600 focus:outline-none focus:border-sky-500 transition-colors resize-none"
            placeholder="Add context for this file…">{escaped_context}</textarea>
        <button type="submit"
            class="px-3 py-2 rounded-lg bg-zinc-700 hover:bg-zinc-600 text-zinc-300 hover:text-zinc-100 text-xs font-medium transition-colors shrink-0">
            Save
        </button>
    </form>
</div>"##));
        }

        html.push_str("</div>");
    }

    // Pagination controls
    if total_pages > 1 {
        html.push_str(r#"<div class="flex items-center justify-center gap-2 mt-4">"#);

        // Previous
        if page > 1 {
            html.push_str(&format!(
                r#"<button class="ctx-page-btn" onclick="ctxPage({},{q})">&laquo; Prev</button>"#,
                page - 1,
                q = format!("'{}'", escaped_search),
            ));
        }

        // Page numbers
        let start_page = if page > 3 { page - 2 } else { 1 };
        let end_page = (start_page + 4).min(total_pages);
        for p in start_page..=end_page {
            let active = if p == page { " active" } else { "" };
            html.push_str(&format!(
                r#"<button class="ctx-page-btn{active}" onclick="ctxPage({p},{q})">{p}</button>"#,
                active = active,
                p = p,
                q = format!("'{}'", escaped_search),
            ));
        }

        // Next
        if page < total_pages {
            html.push_str(&format!(
                r#"<button class="ctx-page-btn" onclick="ctxPage({},{q})">Next &raquo;</button>"#,
                page + 1,
                q = format!("'{}'", escaped_search),
            ));
        }

        html.push_str("</div>");
    }

    html
}

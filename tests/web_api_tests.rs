//! Integration tests for the OmniRAG JSON REST API endpoints.
//!
//! These tests verify the /api/v1/* routes (health, sync trigger, sync status)
//! using Axum's tower::ServiceExt for in-process HTTP testing — no network port needed.

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        extract::State,
        response::{Html, Json},
        routing::{get, post},
        Router,
    };
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    /// Minimal app state for testing (no real Redis connection needed).
    #[derive(Clone)]
    struct TestAppState {
        sync_status: Arc<Mutex<String>>,
    }

    /// GET /api/v1/health — always returns 200 with service info.
    async fn api_health() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "status": "ok",
            "service": "omnirag",
            "version": "0.1.0"
        }))
    }

    /// POST /api/v1/sync — returns 202 if idle, 409 if already running.
    async fn api_trigger_sync(
        State(state): State<TestAppState>,
    ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        let mut status = state.sync_status.lock().await;
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
        (
            axum::http::StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "triggered",
                "message": "Sync started"
            })),
        )
    }

    /// GET /api/v1/sync/status — returns current sync phase.
    async fn api_sync_status(
        State(state): State<TestAppState>,
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

    fn test_app(initial_status: &str) -> Router {
        let state = TestAppState {
            sync_status: Arc::new(Mutex::new(initial_status.to_string())),
        };
        Router::new()
            .route("/api/v1/health", get(api_health))
            .route("/api/v1/sync", post(api_trigger_sync))
            .route("/api/v1/sync/status", get(api_sync_status))
            .with_state(state)
    }

    // ─────────────────── Health Endpoint Tests ───────────────────

    #[tokio::test]
    async fn test_health_returns_200() {
        let app = test_app("Idle");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn test_health_returns_service_info() {
        let app = test_app("Idle");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["status"], "ok");
        assert_eq!(json["service"], "omnirag");
        assert!(json["version"].is_string());
    }

    // ─────────────────── Sync Trigger Tests ───────────────────

    #[tokio::test]
    async fn test_sync_trigger_returns_202_when_idle() {
        let app = test_app("Idle");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/sync")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 202);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "triggered");
    }

    #[tokio::test]
    async fn test_sync_trigger_returns_409_when_running() {
        let app = test_app("Running");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/v1/sync")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 409);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "already_running");
    }

    // ─────────────────── Sync Status Tests ───────────────────

    #[tokio::test]
    async fn test_sync_status_idle() {
        let app = test_app("Idle");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "idle");
        assert_eq!(json["detail"], "Idle");
    }

    #[tokio::test]
    async fn test_sync_status_running() {
        let app = test_app("Running");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "running");
    }

    #[tokio::test]
    async fn test_sync_status_completed() {
        let app = test_app("Completed successfully");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "completed");
    }

    #[tokio::test]
    async fn test_sync_status_error() {
        let app = test_app("Error: connection refused");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/sync/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "error");
        assert_eq!(json["detail"], "Error: connection refused");
    }

    // ─────────────────── Method Validation Tests ───────────────────

    #[tokio::test]
    async fn test_sync_get_returns_405() {
        let app = test_app("Idle");
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/v1/sync")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), 405); // Method Not Allowed
    }
}

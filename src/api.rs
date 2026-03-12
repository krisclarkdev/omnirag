use reqwest::multipart;
use serde::Deserialize;
use tracing::{info, warn};

const MAX_RETRIES: u32 = 3;
const INITIAL_BACKOFF_MS: u64 = 500;

/// Response from the Open WebUI file upload endpoint.
#[derive(Debug, Deserialize)]
pub struct UploadResponse {
    pub id: String,
}

/// Response from the process status endpoint.
#[derive(Debug, Deserialize)]
pub struct ProcessStatus {
    pub status: Option<String>,
}

/// A file entry returned from the knowledge base file list.
#[derive(Debug, Deserialize)]
pub struct KnowledgeFileEntry {
    pub id: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default, alias = "meta")]
    pub name: String,
}

/// Response from the knowledge base GET endpoint.
#[derive(Debug, Deserialize)]
pub struct KnowledgeResponse {
    #[serde(default)]
    pub files: Vec<KnowledgeFile>,
}

/// A file object within a knowledge base.
#[derive(Debug, Deserialize)]
pub struct KnowledgeFile {
    pub id: String,
    #[serde(default)]
    pub filename: String,
}

/// Client wrapping `reqwest` for Open WebUI API interactions.
#[derive(Clone)]
pub struct OpenWebUiClient {
    client: reqwest::Client,
    base_url: String,
    token: String,
}

/// Helper: sleep with exponential backoff.
async fn backoff(attempt: u32) {
    let ms = INITIAL_BACKOFF_MS * 2u64.pow(attempt);
    warn!("Retrying in {}ms (attempt {}/{})", ms, attempt + 1, MAX_RETRIES);
    tokio::time::sleep(tokio::time::Duration::from_millis(ms)).await;
}

impl OpenWebUiClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// Upload a file via multipart/form-data with retry. Returns the file ID.
    pub async fn upload_file(
        &self,
        filename: &str,
        bytes: Vec<u8>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let mut last_err = String::new();

        for attempt in 0..MAX_RETRIES {
            let part = multipart::Part::bytes(bytes.clone())
                .file_name(filename.to_string())
                .mime_str("application/octet-stream")?;

            let form = multipart::Form::new().part("file", part);

            match self
                .client
                .post(format!("{}/api/v1/files/", self.base_url))
                .bearer_auth(&self.token)
                .multipart(form)
                .send()
                .await
            {
                Ok(resp) => {
                    if resp.status().is_success() {
                        let upload: UploadResponse = resp.json().await?;
                        info!("Uploaded '{}' → file_id={}", filename, upload.id);
                        return Ok(upload.id);
                    }
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    last_err = format!("Upload failed ({}): {}", status, body);
                    // Don't retry on 4xx client errors (except 429)
                    if status.is_client_error() && status.as_u16() != 429 {
                        return Err(last_err.into());
                    }
                }
                Err(e) => {
                    last_err = format!("Upload network error: {}", e);
                }
            }

            if attempt < MAX_RETRIES - 1 {
                backoff(attempt).await;
            }
        }

        Err(last_err.into())
    }

    /// Poll `/api/v1/files/{id}/process/status` until completed, with retry.
    pub async fn poll_process_status(
        &self,
        file_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let url = format!(
            "{}/api/v1/files/{}/process/status",
            self.base_url, file_id
        );

        let mut consecutive_errors = 0u32;

        loop {
            match self.client.get(&url).bearer_auth(&self.token).send().await {
                Ok(resp) => {
                    if !resp.status().is_success() {
                        consecutive_errors += 1;
                        if consecutive_errors >= MAX_RETRIES {
                            let body = resp.text().await.unwrap_or_default();
                            return Err(format!("Poll status failed after retries: {}", body).into());
                        }
                        backoff(consecutive_errors - 1).await;
                        continue;
                    }
                    consecutive_errors = 0;

                    let status: ProcessStatus = resp.json().await?;
                    match status.status.as_deref() {
                        Some("completed") => {
                            info!("Processing complete for file_id={}", file_id);
                            return Ok(());
                        }
                        Some("failed") => {
                            return Err(format!("Processing failed for file_id={}", file_id).into());
                        }
                        _ => {
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        }
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_RETRIES {
                        return Err(format!("Poll network error after retries: {}", e).into());
                    }
                    backoff(consecutive_errors - 1).await;
                }
            }
        }
    }

    /// Attach a file to a knowledge base, with retry.
    pub async fn add_to_knowledge(
        &self,
        knowledge_id: &str,
        file_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut last_err = String::new();

        for attempt in 0..MAX_RETRIES {
            match self
                .client
                .post(format!(
                    "{}/api/v1/knowledge/{}/file/add",
                    self.base_url, knowledge_id
                ))
                .bearer_auth(&self.token)
                .json(&serde_json::json!({ "file_id": file_id }))
                .send()
                .await
            {
                Ok(resp) => {
                    if resp.status().is_success() {
                        info!("Attached file_id={} to knowledge={}", file_id, knowledge_id);
                        return Ok(());
                    }
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    last_err = format!("Add to KB failed ({}): {}", status, body);
                    if status.is_client_error() && status.as_u16() != 429 {
                        return Err(last_err.into());
                    }
                }
                Err(e) => {
                    last_err = format!("Add to KB network error: {}", e);
                }
            }

            if attempt < MAX_RETRIES - 1 {
                backoff(attempt).await;
            }
        }

        Err(last_err.into())
    }

    /// Delete a file from Open WebUI, with retry.
    pub async fn delete_file(
        &self,
        file_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut last_err = String::new();

        for attempt in 0..MAX_RETRIES {
            match self
                .client
                .delete(format!("{}/api/v1/files/{}", self.base_url, file_id))
                .bearer_auth(&self.token)
                .send()
                .await
            {
                Ok(resp) => {
                    if resp.status().is_success() {
                        info!("Deleted file_id={} from Open WebUI", file_id);
                        return Ok(());
                    }
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    last_err = format!("Delete failed ({}): {}", status, body);
                    if status.is_client_error() && status.as_u16() != 429 {
                        return Err(last_err.into());
                    }
                }
                Err(e) => {
                    last_err = format!("Delete network error: {}", e);
                }
            }

            if attempt < MAX_RETRIES - 1 {
                backoff(attempt).await;
            }
        }

        Err(last_err.into())
    }

    /// List all files currently in a knowledge base.
    /// Used for pre-sync reconciliation to prevent duplicates.
    pub async fn list_knowledge_files(
        &self,
        knowledge_id: &str,
    ) -> Result<Vec<KnowledgeFile>, Box<dyn std::error::Error + Send + Sync>> {
        let mut last_err = String::new();

        for attempt in 0..MAX_RETRIES {
            match self
                .client
                .get(format!(
                    "{}/api/v1/knowledge/{}", 
                    self.base_url, knowledge_id
                ))
                .bearer_auth(&self.token)
                .send()
                .await
            {
                Ok(resp) => {
                    if resp.status().is_success() {
                        let kb: KnowledgeResponse = resp.json().await?;
                        info!("Found {} files in knowledge base {}", kb.files.len(), knowledge_id);
                        return Ok(kb.files);
                    }
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    last_err = format!("List KB files failed ({}): {}", status, body);
                    if status.is_client_error() && status.as_u16() != 429 {
                        return Err(last_err.into());
                    }
                }
                Err(e) => {
                    last_err = format!("List KB files network error: {}", e);
                }
            }

            if attempt < MAX_RETRIES - 1 {
                backoff(attempt).await;
            }
        }

        Err(last_err.into())
    }
}

/// Summary of a knowledge base returned from the listing endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct KnowledgeBaseSummary {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// Standalone function: list ALL knowledge bases from an Open WebUI instance.
/// Takes raw URL + token (used from the config form before a client exists).
pub async fn list_all_knowledge_bases(
    base_url: &str,
    token: &str,
) -> Result<Vec<KnowledgeBaseSummary>, Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let url = format!("{}/api/v1/knowledge/", base_url.trim_end_matches('/'));

    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Failed to list knowledge bases ({}): {}", status, body).into());
    }

    // Open WebUI may return either a paginated { "items": [...] } wrapper
    // or a flat array, depending on version.
    let body = resp.text().await?;
    
    // Try paginated format first
    #[derive(Deserialize)]
    struct PaginatedResponse {
        items: Vec<KnowledgeBaseSummary>,
    }
    
    if let Ok(paginated) = serde_json::from_str::<PaginatedResponse>(&body) {
        return Ok(paginated.items);
    }
    
    // Fall back to flat array
    let kbs: Vec<KnowledgeBaseSummary> = serde_json::from_str(&body)?;
    Ok(kbs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_upload_file_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/files/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "file-uuid-123"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "test-token");
        let result = client.upload_file("test.md", b"hello world".to_vec()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "file-uuid-123");
    }

    #[tokio::test]
    async fn test_upload_retries_on_500_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/files/"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/files/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "retry-uuid"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        let result = client.upload_file("test.md", b"data".to_vec()).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "retry-uuid");
    }

    #[tokio::test]
    async fn test_upload_retries_on_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/files/"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/files/"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "rl-uuid"})),
            )
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        assert!(client.upload_file("t.md", b"d".to_vec()).await.is_ok());
    }

    #[tokio::test]
    async fn test_upload_fails_immediately_on_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/files/"))
            .respond_with(ResponseTemplate::new(400).set_body_string("Bad Request"))
            .expect(1) // No retry
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        let result = client.upload_file("t.md", b"d".to_vec()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("400"));
    }

    #[tokio::test]
    async fn test_upload_fails_after_max_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/files/"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        assert!(client.upload_file("t.md", b"d".to_vec()).await.is_err());
    }

    #[tokio::test]
    async fn test_delete_file_success() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/api/v1/files/f-123"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        assert!(client.delete_file("f-123").await.is_ok());
    }

    #[tokio::test]
    async fn test_add_to_knowledge_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/knowledge/kb-1/file/add"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        assert!(client.add_to_knowledge("kb-1", "f-1").await.is_ok());
    }

    #[tokio::test]
    async fn test_list_knowledge_files() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledge/kb-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "files": [
                    {"id": "f1", "filename": "doc.md"},
                    {"id": "f2", "filename": "report.pdf"}
                ]
            })))
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        let files = client.list_knowledge_files("kb-1").await.unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].filename, "doc.md");
    }

    #[tokio::test]
    async fn test_poll_process_status_completed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/files/f-1/process/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"status": "completed"}),
            ))
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        assert!(client.poll_process_status("f-1").await.is_ok());
    }

    #[tokio::test]
    async fn test_poll_process_status_failed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/files/f-1/process/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"status": "failed"}),
            ))
            .mount(&server)
            .await;

        let client = OpenWebUiClient::new(&server.uri(), "tok");
        let result = client.poll_process_status("f-1").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("failed"));
    }
}

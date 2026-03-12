use serde::{Deserialize, Serialize};

/// Global application configuration stored in Redis at `config:global`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub target_directory: String,
    pub openwebui_url: String,
    pub openwebui_token: String,
    pub openwebui_knowledge_id: String,
    #[serde(default)]
    pub redis_url: String,
    #[serde(default = "default_context_header_label")]
    pub context_header_label: String,
    #[serde(default = "default_max_concurrent_uploads")]
    pub max_concurrent_uploads: u32,
    #[serde(default)]
    pub convert_to_markdown: bool,
}

fn default_context_header_label() -> String {
    "File Context".to_string()
}

fn default_max_concurrent_uploads() -> u32 {
    5
}

impl AppConfig {
    /// Load configuration from the `config:global` Redis hash.
    pub async fn load_from_redis(
        con: &mut redis::aio::MultiplexedConnection,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let values: Vec<Option<String>> = redis::pipe()
            .hget("config:global", "target_directory")
            .hget("config:global", "openwebui_url")
            .hget("config:global", "openwebui_token")
            .hget("config:global", "openwebui_knowledge_id")
            .hget("config:global", "redis_url")
            .hget("config:global", "context_header_label")
            .hget("config:global", "max_concurrent_uploads")
            .hget("config:global", "convert_to_markdown")
            .query_async(con)
            .await?;

        Ok(Self {
            target_directory: values[0].clone().unwrap_or_default(),
            openwebui_url: values[1].clone().unwrap_or_default(),
            openwebui_token: values[2].clone().unwrap_or_default(),
            openwebui_knowledge_id: values[3].clone().unwrap_or_default(),
            redis_url: values[4].clone().unwrap_or_default(),
            context_header_label: values[5].clone().unwrap_or_else(|| "File Context".to_string()),
            max_concurrent_uploads: values[6].as_deref().and_then(|v| v.parse().ok()).unwrap_or(5),
            convert_to_markdown: values[7].as_deref() == Some("true"),
        })
    }

    /// Save configuration to the `config:global` Redis hash.
    pub async fn save_to_redis(
        &self,
        con: &mut redis::aio::MultiplexedConnection,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        redis::pipe()
            .hset("config:global", "target_directory", &self.target_directory)
            .hset("config:global", "openwebui_url", &self.openwebui_url)
            .hset("config:global", "openwebui_token", &self.openwebui_token)
            .hset("config:global", "openwebui_knowledge_id", &self.openwebui_knowledge_id)
            .hset("config:global", "redis_url", &self.redis_url)
            .hset("config:global", "context_header_label", &self.context_header_label)
            .hset("config:global", "max_concurrent_uploads", self.max_concurrent_uploads.to_string())
            .hset("config:global", "convert_to_markdown", if self.convert_to_markdown { "true" } else { "false" })
            .query_async::<()>(con)
            .await?;

        Ok(())
    }
}

use redis::Value;
use tracing::{info, warn};

const LUA_SCRIPT: &str = include_str!("lua/rag_helpers.lua");

/// Load the `rag_helpers` Lua library into Redis using FUNCTION LOAD REPLACE.
pub async fn load_functions(
    con: &mut redis::aio::MultiplexedConnection,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let result: Result<Value, redis::RedisError> = redis::cmd("FUNCTION")
        .arg("LOAD")
        .arg("REPLACE")
        .arg(LUA_SCRIPT)
        .query_async(con)
        .await;

    match result {
        Ok(_) => {
            info!("Loaded rag_helpers Lua library into Redis");
            Ok(())
        }
        Err(e) => {
            warn!("Failed to load Lua functions: {}", e);
            Err(e.into())
        }
    }
}

/// FCALL get_formatted_context 1 <key>
pub async fn fcall_get_formatted_context(
    con: &mut redis::aio::MultiplexedConnection,
    key: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let result: String = redis::cmd("FCALL")
        .arg("get_formatted_context")
        .arg(1)
        .arg(key)
        .query_async(con)
        .await?;
    Ok(result)
}

/// FCALL check_file_exists 1 <key>
pub async fn fcall_check_file_exists(
    con: &mut redis::aio::MultiplexedConnection,
    key: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let result: i64 = redis::cmd("FCALL")
        .arg("check_file_exists")
        .arg(1)
        .arg(key)
        .query_async(con)
        .await?;
    Ok(result == 1)
}

/// FCALL verify_file_hash 1 <key> <local_content_hash>
pub async fn fcall_verify_file_hash(
    con: &mut redis::aio::MultiplexedConnection,
    key: &str,
    local_content_hash: &str,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let result: i64 = redis::cmd("FCALL")
        .arg("verify_file_hash")
        .arg(1)
        .arg(key)
        .arg(local_content_hash)
        .query_async(con)
        .await?;
    Ok(result == 1)
}

/// FCALL upsert_sync_state 1 <key> <absolute_path> <content_hash> <openwebui_file_id>
pub async fn fcall_upsert_sync_state(
    con: &mut redis::aio::MultiplexedConnection,
    key: &str,
    absolute_path: &str,
    content_hash: &str,
    openwebui_file_id: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    redis::cmd("FCALL")
        .arg("upsert_sync_state")
        .arg(1)
        .arg(key)
        .arg(absolute_path)
        .arg(content_hash)
        .arg(openwebui_file_id)
        .query_async::<i64>(con)
        .await?;
    Ok(())
}

/// FCALL get_cleanup_batch 0 <cursor>
/// Returns (new_cursor, Vec<(key, path, file_id)>)
pub async fn fcall_get_cleanup_batch(
    con: &mut redis::aio::MultiplexedConnection,
    cursor: &str,
) -> Result<(String, Vec<(String, String, String)>), Box<dyn std::error::Error + Send + Sync>> {
    let result: Vec<String> = redis::cmd("FCALL")
        .arg("get_cleanup_batch")
        .arg(0)
        .arg(cursor)
        .query_async(con)
        .await?;

    if result.is_empty() {
        return Ok(("0".to_string(), vec![]));
    }

    let new_cursor = result[0].clone();
    let mut items = Vec::new();

    // Results come in triplets: key, path, file_id
    let data = &result[1..];
    for chunk in data.chunks(3) {
        if chunk.len() == 3 {
            items.push((chunk[0].clone(), chunk[1].clone(), chunk[2].clone()));
        }
    }

    Ok((new_cursor, items))
}

/// Scan all tracked file keys and return (key, absolute_path, context_text) for each.
pub async fn scan_all_tracked_files(
    con: &mut redis::aio::MultiplexedConnection,
) -> Result<Vec<(String, String, String)>, Box<dyn std::error::Error + Send + Sync>> {
    let mut cursor = "0".to_string();
    let mut files = Vec::new();

    loop {
        let (new_cursor, keys): (String, Vec<String>) = redis::cmd("SCAN")
            .arg(&cursor)
            .arg("COUNT")
            .arg(200)
            .query_async(con)
            .await?;

        for key in &keys {
            if key == "config:global" {
                continue;
            }
            let path: Option<String> = redis::cmd("HGET")
                .arg(key)
                .arg("absolute_path")
                .query_async(con)
                .await?;
            let ctx: Option<String> = redis::cmd("HGET")
                .arg(key)
                .arg("context_text")
                .query_async(con)
                .await?;
            if let Some(path) = path {
                files.push((key.clone(), path, ctx.unwrap_or_default()));
            }
        }

        if new_cursor == "0" {
            break;
        }
        cursor = new_cursor;
    }

    files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(files)
}

/// Update the context_text field for a specific Redis key and mark it dirty for re-sync.
pub async fn update_context_text(
    con: &mut redis::aio::MultiplexedConnection,
    key: &str,
    context: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    redis::cmd("HSET")
        .arg(key)
        .arg("context_text")
        .arg(context)
        .arg("context_dirty")
        .arg("true")
        .query_async::<i64>(con)
        .await?;
    Ok(())
}

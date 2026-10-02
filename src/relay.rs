//! A private, inert MCP server. Selecting a client tool never executes it.
use crate::protocol::{ToolCall, ToolDefinition};
use serde_json::{Value, json};
use std::path::{Component, Path};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub(crate) const CATALOGUE_NAME: &str = "router-tools.json";
pub(crate) const HANDOFF_NAME: &str = "router-handoff.json";
const MAX_RELAY_BYTES: usize = 1024 * 1024;

/// The catalogue must be the router-created file in the relay's working directory.
/// This subcommand has no configurable executable, destination, or network transport.
pub async fn serve_stdio(catalogue: &Path) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|_| "Relay workspace unavailable")?;
    let catalogue = checked_catalogue(catalogue, &cwd).await?;
    let tools = read_catalogue(&catalogue).await?;
    serve_io(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        &tools,
        &cwd.join(HANDOFF_NAME),
    )
    .await
}

pub(crate) fn protected_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            name == "secrets"
                || name == "credentials"
                || name.starts_with(".env")
                || name.ends_with(".pem")
                || name.ends_with(".key")
        }
        Component::ParentDir => true,
        _ => false,
    })
}

async fn checked_catalogue(path: &Path, cwd: &Path) -> Result<std::path::PathBuf, String> {
    if protected_path(path) || path.file_name().is_none_or(|n| n != CATALOGUE_NAME) {
        return Err("Invalid relay catalogue path".into());
    }
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| "Relay catalogue unavailable")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("Invalid relay catalogue file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("Invalid relay catalogue links".into());
        }
    }
    let real = tokio::fs::canonicalize(path)
        .await
        .map_err(|_| "Relay catalogue unavailable")?;
    let cwd = tokio::fs::canonicalize(cwd)
        .await
        .map_err(|_| "Relay workspace unavailable")?;
    if real.parent() != Some(cwd.as_path()) || protected_path(&real) {
        return Err("Relay catalogue outside workspace".into());
    }
    Ok(real)
}

async fn read_catalogue(path: &Path) -> Result<Vec<ToolDefinition>, String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|_| "Missing catalogue")?;
    if metadata.len() > MAX_RELAY_BYTES as u64 {
        return Err("Relay catalogue too large".into());
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    let file = options
        .open(path)
        .await
        .map_err(|_| "Unreadable catalogue")?;
    let mut bytes = Vec::new();
    use tokio::io::AsyncReadExt;
    file.take(MAX_RELAY_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "Unreadable catalogue")?;
    if bytes.len() > MAX_RELAY_BYTES {
        return Err("Relay catalogue too large".into());
    }
    let tools: Vec<ToolDefinition> =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid relay catalogue")?;
    if tools.len() > 128
        || tools.iter().any(|tool| {
            tool.name.is_empty()
                || tool.name.len() > 64
                || !tool
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
        })
    {
        return Err("Invalid relay tool definitions".into());
    }
    let mut names = std::collections::HashSet::new();
    if tools.iter().any(|tool| !names.insert(&tool.name)) {
        return Err("Duplicate relay tools".into());
    }
    for tool in &tools {
        crate::protocol::validate_schema(&tool.parameters)
            .map_err(|_| "Invalid relay tool schema")?;
    }
    Ok(tools)
}

/// A bounded NDJSON reader; read_line/read_until without a cap can allocate forever.
pub(crate) async fn bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|_| "Provider pipe failed")?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |i| i + 1);
        if line.len().saturating_add(take) > limit {
            return Err("Provider output exceeds configured limit".into());
        }
        let complete = available[take - 1] == b'\n';
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if complete {
            return Ok(Some(line));
        }
    }
}

async fn respond<W: AsyncWrite + Unpin>(writer: &mut W, value: Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(&value).map_err(|_| "Relay serialisation failed")?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| "Relay output closed")?;
    writer
        .flush()
        .await
        .map_err(|_| "Relay output closed".to_string())
}

async fn capture(handoff: &Path, value: &Value) -> Result<(), String> {
    // Publish a complete file atomically. hard_link fails rather than replacing
    // an existing call or following a destination symlink.
    let temporary = handoff.with_file_name(format!(
        "router-capture-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut file = options
        .open(&temporary)
        .await
        .map_err(|_| "Unable to capture tool")?;
    let bytes = serde_json::to_vec(value).map_err(|_| "Invalid captured call")?;
    file.write_all(&bytes)
        .await
        .map_err(|_| "Unable to capture tool")?;
    file.flush().await.map_err(|_| "Unable to capture tool")?;
    drop(file);
    let published = tokio::fs::hard_link(&temporary, handoff)
        .await
        .map_err(|_| "Tool already captured".to_string());
    let _ = tokio::fs::remove_file(&temporary).await;
    published?;
    Ok(())
}

async fn serve_io<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    tools: &[ToolDefinition],
    handoff: &Path,
) -> Result<(), String> {
    let mut total = 0usize;
    loop {
        let Some(line) = bounded_line(&mut reader, MAX_RELAY_BYTES.saturating_sub(total)).await?
        else {
            return Ok(());
        };
        total += line.len();
        let request: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                respond(&mut writer, json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})).await?;
                continue;
            }
        };
        let Some(id) = request
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
        else {
            // MCP notifications (including initialized/cancelled) need no response.
            continue;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let result = match method {
            "initialize" => {
                let supplied = request
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-03-26");
                let version = match supplied {
                    "2024-11-05" | "2025-03-26" | "2025-06-18" => supplied,
                    _ => "2025-03-26",
                };
                json!({"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"ai-router-inert-relay","version":"0.1.0"}})
            }
            "ping" => json!({}),
            "tools/list" => {
                json!({"tools":tools.iter().map(|tool| json!({"name":tool.name,"description":tool.description,"inputSchema":tool.parameters})).collect::<Vec<_>>() })
            }
            "resources/list" => json!({"resources":[]}),
            "resources/templates/list" => json!({"resourceTemplates":[]}),
            "prompts/list" => json!({"prompts":[]}),
            "tools/call" => {
                let name = request
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let arguments = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let valid = tools
                    .iter()
                    .find(|tool| tool.name == name)
                    .is_some_and(|tool| {
                        arguments.is_object()
                            && jsonschema::validator_for(&tool.parameters)
                                .is_ok_and(|schema| schema.is_valid(&arguments))
                    });
                let value = if valid {
                    serde_json::to_value(ToolCall {
                        id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                        name: name.to_string(),
                        arguments,
                    })
                    .map_err(|_| "Invalid tool call")?
                } else {
                    json!({"error":"invalid_tool_call"})
                };
                capture(handoff, &value).await?;
                // Deliberately do not fabricate a tool result or let AGY continue.
                // The supervisor observes the capture and kills this process group.
                std::future::pending::<()>().await;
                unreachable!();
            }
            _ => {
                respond(&mut writer, json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})).await?;
                continue;
            }
        };
        respond(
            &mut writer,
            json!({"jsonrpc":"2.0","id":id,"result":result}),
        )
        .await?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relay_lists_exact_schema_and_captures_without_result() {
        let root = tempfile::tempdir().unwrap();
        let handoff = root.path().join(HANDOFF_NAME);
        let tool = ToolDefinition {
            name: "weather".into(),
            description: "Weather".into(),
            parameters: json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}),
        };
        let (input, mut client) = tokio::io::duplex(4096);
        let (mut response, output) = tokio::io::duplex(4096);
        let destination = handoff.clone();
        let server = tokio::spawn(async move {
            serve_io(BufReader::new(input), output, &[tool], &destination).await
        });
        client
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();
        let response = bounded_line(&mut BufReader::new(&mut response), 4096)
            .await
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(
            value.pointer("/result/tools/0/inputSchema/required"),
            Some(&json!(["city"]))
        );
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"weather\",\"arguments\":{\"city\":\"London\"}}}\n").await.unwrap();
        for _ in 0..100 {
            if tokio::fs::metadata(&handoff).await.is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let call: ToolCall =
            serde_json::from_slice(&tokio::fs::read(&handoff).await.unwrap()).unwrap();
        assert_eq!(call.name, "weather");
        assert_eq!(call.arguments, json!({"city":"London"}));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                tokio::fs::metadata(&handoff)
                    .await
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(!server.is_finished());
        server.abort();
    }

    #[tokio::test]
    async fn line_limit_applies_before_allocation_and_protected_paths_are_rejected() {
        let mut reader = BufReader::new(&b"123456789\n"[..]);
        assert!(bounded_line(&mut reader, 4).await.is_err());
        for path in [
            ".env",
            ".env.example",
            "x/a.pem",
            "x/a.key",
            "x/secrets/a",
            "x/credentials/a",
            "../a",
        ] {
            assert!(protected_path(Path::new(path)));
        }
        assert!(!protected_path(Path::new("input/document.txt")));
    }
}

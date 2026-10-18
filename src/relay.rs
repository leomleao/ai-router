//! A private, inert MCP server. Selecting a client tool never executes it.
use crate::protocol::{ToolCall, ToolDefinition};
use serde_json::{Value, json};
use std::path::{Component, Path};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

pub(crate) const CATALOGUE_NAME: &str = "router-tools.json";
pub(crate) const HANDOFF_NAME: &str = "router-handoff.json";
const MAX_RELAY_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TOOL_CORRECTIONS: usize = 2;

/// Provider-facing names cannot collide with AGY's native client-tool names.
/// The index is local to the current immutable request catalogue.
pub(crate) fn tool_alias(index: usize) -> String {
    format!("router_client_{index:03}")
}

pub(crate) fn resolve_tool_alias<'a>(
    tools: &'a [ToolDefinition],
    alias: &str,
) -> Option<&'a ToolDefinition> {
    tools
        .iter()
        .enumerate()
        .find_map(|(index, tool)| (alias == tool_alias(index)).then_some(tool))
}

pub(crate) fn rejection_path(handoff: &Path, attempt: usize) -> std::path::PathBuf {
    handoff.with_file_name(format!("router-tool-rejection-{attempt}.json"))
}

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
    if publish(handoff, value).await? {
        Ok(())
    } else {
        Err("Tool already captured".into())
    }
}

async fn publish(handoff: &Path, value: &Value) -> Result<bool, String> {
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
    let published = match tokio::fs::hard_link(&temporary, handoff).await {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(_) => Err("Unable to publish tool call".to_string()),
    };
    let _ = tokio::fs::remove_file(&temporary).await;
    published
}

async fn record_rejection(handoff: &Path, value: &Value) -> Result<usize, String> {
    // Atomic numbered publications share the budget across relay restarts and
    // concurrent MCP connections. Neither counters nor files are overwritten.
    for attempt in 1..=MAX_TOOL_CORRECTIONS + 1 {
        if publish(&rejection_path(handoff, attempt), value).await? {
            return Ok(attempt);
        }
    }
    Ok(MAX_TOOL_CORRECTIONS + 1)
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
                json!({"tools":tools.iter().enumerate().map(|(index,tool)| json!({"name":tool_alias(index),"description":tool.description,"inputSchema":tool.parameters})).collect::<Vec<_>>() })
            }
            "resources/list" => json!({"resources":[]}),
            "resources/templates/list" => json!({"resourceTemplates":[]}),
            "prompts/list" => json!({"prompts":[]}),
            "tools/call" => {
                match tokio::fs::symlink_metadata(rejection_path(handoff, MAX_TOOL_CORRECTIONS + 1))
                    .await
                {
                    Ok(_) => {
                        std::future::pending::<()>().await;
                        unreachable!();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err("Correction budget unavailable".into()),
                }
                let name = request.pointer("/params/name");
                let arguments = request
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                let tool = name
                    .and_then(Value::as_str)
                    .and_then(|alias| resolve_tool_alias(tools, alias));
                let client_name = tool.map(|tool| json!(tool.name));
                // An original client name is not a provider alias. Validate
                // unrecognized names against an empty inventory, never fall back.
                let validation = crate::tool_diagnostics::validate_call(
                    if tool.is_some() { tools } else { &[] },
                    client_name.as_ref().or(name),
                    &arguments,
                );
                if let Err(diagnostic) = &validation {
                    if diagnostic.repairable() {
                        let attempt = record_rejection(
                            handoff,
                            &json!({"name":client_name,"arguments":arguments}),
                        )
                        .await?;
                        if attempt <= MAX_TOOL_CORRECTIONS {
                            respond(&mut writer, json!({"jsonrpc":"2.0","id":id,"result":diagnostic.feedback(MAX_TOOL_CORRECTIONS-attempt+1, name.and_then(Value::as_str).unwrap())})).await?;
                            continue;
                        }
                        // The supervisor observes the third rejection and stops
                        // the request. Never let relay restarts reset the budget.
                        std::future::pending::<()>().await;
                        unreachable!();
                    }
                }
                let value = if validation.is_ok() {
                    serde_json::to_value(ToolCall {
                        id: format!("call_{}", uuid::Uuid::new_v4().simple()),
                        name: tool.unwrap().name.clone(),
                        arguments,
                    })
                    .map_err(|_| "Invalid tool call")?
                } else {
                    // Preserve the attempted call only in the owned temporary
                    // workspace. The supervisor computes safe diagnostics and
                    // deletes the workspace; raw values never enter its logs.
                    json!({"error":"invalid_tool_call","name":name,"arguments":arguments})
                };
                capture(handoff, &value).await?;
                // A valid selection never receives a fabricated tool result.
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

    #[test]
    fn aliases_resolve_only_against_the_current_catalogue() {
        let tools: Vec<_> = ["write_todos", "router_client_000", "call_mcp_tool"]
            .iter()
            .map(|name| ToolDefinition {
                name: (*name).into(),
                description: String::new(),
                parameters: json!({"type":"object"}),
            })
            .collect();
        for (index, tool) in tools.iter().enumerate() {
            assert_eq!(
                resolve_tool_alias(&tools, &tool_alias(index)).unwrap().name,
                tool.name
            );
        }
        for invalid in [
            "write_todos",
            "call_mcp_tool",
            "router_client_0",
            "router_client_003",
            "router_client_999",
            "router_client_-01",
        ] {
            assert!(resolve_tool_alias(&tools, invalid).is_none(), "{invalid}");
        }
        assert!(resolve_tool_alias(&[], "router_client_000").is_none());
    }

    #[tokio::test]
    async fn argument_corrections_share_a_budget_across_relay_restarts() {
        let root = tempfile::tempdir().unwrap();
        let handoff = root.path().join(HANDOFF_NAME);
        for attempt in 1..=MAX_TOOL_CORRECTIONS + 1 {
            let tool = ToolDefinition {
                name: "learn_tools".into(),
                description: String::new(),
                parameters: json!({"type":"object","properties":{"toolNames":{"type":"array","items":{"type":"string"}}},"required":["toolNames"],"additionalProperties":false}),
            };
            let (input, mut client) = tokio::io::duplex(8192);
            let (response, output) = tokio::io::duplex(8192);
            let destination = handoff.clone();
            let server = tokio::spawn(async move {
                serve_io(BufReader::new(input), output, &[tool], &destination).await
            });
            let mut response = BufReader::new(response);
            client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"router_client_000\",\"arguments\":{\"wrong\":\"synthetic-private\"}}}\n").await.unwrap();
            if attempt <= MAX_TOOL_CORRECTIONS {
                let line = bounded_line(&mut response, 8192).await.unwrap().unwrap();
                let reply: Value = serde_json::from_slice(&line).unwrap();
                assert_eq!(reply["result"]["isError"], true);
                assert!(!reply.to_string().contains("synthetic-private"));
                assert!(!handoff.exists());
                client.shutdown().await.unwrap();
                assert!(server.await.unwrap().is_ok());
            } else {
                for _ in 0..200 {
                    if rejection_path(&handoff, attempt).exists() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(30),
                        bounded_line(&mut response, 8192)
                    )
                    .await
                    .is_err()
                );
                assert!(!handoff.exists());
                server.abort();
            }
            assert!(rejection_path(&handoff, attempt).exists());
        }
        assert!(
            capture(&rejection_path(&handoff, 1), &json!({}))
                .await
                .is_err()
        );
    }

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
        assert_eq!(value["result"]["tools"][0]["name"], "router_client_000");
        assert_eq!(
            value.pointer("/result/tools/0/inputSchema/required"),
            Some(&json!(["city"]))
        );
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"router_client_000\",\"arguments\":{\"city\":\"London\"}}}\n").await.unwrap();
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

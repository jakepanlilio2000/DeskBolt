#![cfg(all(feature = "dev-mcp", not(target_os = "android")))]

use std::{process::Stdio, time::Duration};

use rmcp::serde_json::{Value, from_str, from_value, json};
use schemars::schema::{Schema, SchemaObject};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

#[tokio::test]
async fn mcp_stdio_lists_and_runs_all_loopback_checks() {
    tokio::time::timeout(Duration::from_secs(85), async {
        let mut process = Command::new(env!("CARGO_BIN_EXE_deskbolt-viewer"))
            .args(["--mcp", "--loopback-only"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut input = process.stdin.take().unwrap();
        let mut output = BufReader::new(process.stdout.take().unwrap()).lines();
        let tools = [
            "test_auth_handshake",
            "test_comms_and_power_sim",
            "test_file_sync_roundtrip",
            "test_pty_roundtrip",
            "test_tunnel_and_sysinfo",
        ];
        let mut requests = vec![
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {"name": "DeskBolt verification", "version": "1"}
                }
            }),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        ];
        requests.extend(tools.iter().enumerate().map(|(index, name)| {
            json!({"jsonrpc": "2.0", "id": index + 3, "method": "tools/call",
                "params": {"name": name, "arguments": {}}})
        }));
        requests.push(json!({
            "jsonrpc": "2.0", "id": tools.len() + 3, "method": "tools/call",
            "params": {"name": "test_tunnel_and_sysinfo", "arguments": {"host": "192.0.2.1"}}
        }));

        for request in requests {
            input
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            let Some(id) = request["id"].as_u64() else {
                continue;
            };
            let line = output
                .next_line()
                .await
                .unwrap()
                .expect("MCP closed stdout");
            let response: Value = from_str(&line).expect("MCP output must contain only JSON-RPC");
            assert_eq!(response["id"], id);
            if id == tools.len() as u64 + 3 {
                assert!(
                    !response["error"].is_null() || response["result"]["isError"] == true,
                    "{request}: {response}"
                );
                continue;
            }
            assert!(response["error"].is_null(), "{request}: {response}");
            let result = &response["result"];
            match id {
                1 => assert!(result["capabilities"]["tools"].is_object(), "{response}"),
                2 => {
                    let mut names: Vec<_> = result["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|tool| {
                            let schema: SchemaObject =
                                from_value(tool["inputSchema"].clone()).unwrap();
                            assert_eq!(
                                schema.object.unwrap().additional_properties.as_deref(),
                                Some(&Schema::Bool(false))
                            );
                            tool["name"].as_str().unwrap()
                        })
                        .collect();
                    names.sort_unstable();
                    assert_eq!(names, tools);
                }
                _ => {
                    assert_ne!(result["isError"], true, "{request}: {response}");
                    assert!(
                        result["content"][0]["text"]
                            .as_str()
                            .is_some_and(|s| s.ends_with("passed")),
                        "{request}: {response}"
                    );
                }
            }
        }
        drop(input);
        assert!(process.wait().await.unwrap().success());
    })
    .await
    .expect("MCP subprocess timed out");
}

#[tokio::test]
async fn mcp_rejects_address_overrides() {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(env!("CARGO_BIN_EXE_deskbolt-viewer"))
            .args(["--mcp", "--bind=0.0.0.0"])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
}

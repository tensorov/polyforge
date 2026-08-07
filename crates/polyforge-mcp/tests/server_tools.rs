//! Integration tests for the PolyForge MCP server.
//!
//! Each test drives a real client/server pair over an in-memory duplex
//! stream and exercises the four tools end-to-end against a unique temp
//! ledger path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use polyforge_mcp::server::PolyForgeServer;
use rmcp::{
    model::*,
    service::{serve_directly, RoleClient, RoleServer, RunningService},
    ClientHandler,
};
use serde_json::json;

/// Unique counter so parallel tests never collide on temp ledger paths.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A minimal tools-only client (all `ClientHandler` methods have defaults).
struct TestClient;

impl ClientHandler for TestClient {}

fn client_info() -> ClientInfo {
    ClientInfo::new(
        ClientCapabilities::builder().enable_elicitation().build(),
        Implementation::new("polyforge-mcp-test-client", "0.0.0"),
    )
    .with_protocol_version(ProtocolVersion::V_2026_07_28)
}

fn server_info() -> ServerInfo {
    let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
    info.protocol_version = ProtocolVersion::V_2026_07_28;
    info
}

/// Unique temp ledger path for one test.
fn temp_ledger() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!(
        "polyforge-mcp-test-{}-{n}.jsonl",
        std::process::id()
    ))
}

/// Run `body` against a live server/client pair over an in-memory duplex.
async fn with_pair<F, Fut>(body: F) -> anyhow::Result<()>
where
    F: FnOnce(RunningService<RoleClient, TestClient>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let ledger = temp_ledger();
    let server = PolyForgeServer::new(&ledger).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    tokio::task::LocalSet::new()
        .run_until(async move {
            let (server_transport, client_transport) = tokio::io::duplex(8192);
            let server_peer_info = client_info();
            let client_peer_info = server_info();
            let server_task = tokio::task::spawn_local(async move {
                let running = serve_directly::<RoleServer, _, _, _, _>(
                    server,
                    server_transport,
                    Some(server_peer_info),
                );
                running.waiting().await?;
                anyhow::Ok(())
            });

            let client = serve_directly::<RoleClient, _, _, _, _>(
                TestClient,
                client_transport,
                Some(client_peer_info.into()),
            );

            let result = body(client).await;

            server_task.abort();
            result
        })
        .await
}

async fn call_tool(
    client: &RunningService<RoleClient, TestClient>,
    name: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<CallToolResult> {
    let params = CallToolRequestParams::new(name.to_string())
        .with_arguments(serde_json::from_value(arguments).unwrap());
    Ok(client.call_tool(params).await?)
}

#[tokio::test(flavor = "current_thread")]
async fn test_server_lists_tools() -> anyhow::Result<()> {
    with_pair(|client| async move {
        let result = client.list_tools(None).await?;
        let mut names: Vec<String> = result.tools.iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "evidence_append".to_string(),
                "evidence_verify".to_string(),
                "gate_evaluate".to_string(),
                "gate_report".to_string(),
            ]
        );
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_evidence_append_roundtrip() -> anyhow::Result<()> {
    with_pair(|client| async move {
        let result = call_tool(
            &client,
            "evidence_append",
            json!({
                "kind": "ModelClaim",
                "payload": "{\"note\":\"task-7 done\"}",
                "task_id": "task-7",
                "commit_sha": "abc123",
                "diff_hash": "def456",
            }),
        )
        .await?;
        let text = result.content[0].as_text().unwrap().text.clone();
        let parsed: serde_json::Value = serde_json::from_str(&text)?;
        assert_eq!(parsed["entry_id"], 0);
        assert_eq!(parsed["kind"], "ModelClaim");
        assert_eq!(parsed["state"], "ModelClaimed");
        assert_eq!(parsed["task_id"], "task-7");
        assert_eq!(parsed["commit_sha"], "abc123");
        assert_eq!(parsed["diff_hash"], "def456");
        assert!(!parsed["hash"].as_str().unwrap().is_empty());
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_claim_cannot_self_verify_via_mcp() -> anyhow::Result<()> {
    with_pair(|client| async move {
        // A model cannot append a ToolAttestation (self-verification).
        let result = call_tool(
            &client,
            "evidence_append",
            json!({
                "kind": "ToolAttestation",
                "payload": "{}",
                "task_id": "task-7",
                "commit_sha": "abc123",
                "diff_hash": "def456",
            }),
        )
        .await;
        let err = result.expect_err("ToolAttestation append must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("only accepts kind=ModelClaim"),
            "unexpected error: {msg}"
        );
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_evidence_verify_roundtrip() -> anyhow::Result<()> {
    with_pair(|client| async move {
        // 1. Append a claim.
        let append = call_tool(
            &client,
            "evidence_append",
            json!({
                "kind": "ModelClaim",
                "payload": "{}",
                "task_id": "task-7",
                "commit_sha": "abc123",
                "diff_hash": "def456",
            }),
        )
        .await?;
        let append_text = append.content[0].as_text().unwrap().text.clone();
        let append_json: serde_json::Value = serde_json::from_str(&append_text)?;
        let claim_id = append_json["entry_id"].as_u64().unwrap();

        // 2. Verify the claim with an allowlisted tool.
        let verify = call_tool(
            &client,
            "evidence_verify",
            json!({
                "task_id": "task-7",
                "claim_id": claim_id,
                "tool_name": "cargo --version",
                "args": [],
            }),
        )
        .await?;
        let verify_text = verify.content[0].as_text().unwrap().text.clone();
        let verify_json: serde_json::Value = serde_json::from_str(&verify_text)?;
        assert_eq!(verify_json["kind"], "ToolAttestation");
        assert_eq!(verify_json["state"], "Verified");
        assert_eq!(verify_json["task_id"], "task-7");
        assert_eq!(verify_json["exit_code"], 0);
        assert!(!verify_json["stdout_hash"].as_str().unwrap().is_empty());
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_gate_evaluate_roundtrip() -> anyhow::Result<()> {
    with_pair(|client| async move {
        // 1. Append a claim.
        let append = call_tool(
            &client,
            "evidence_append",
            json!({
                "kind": "ModelClaim",
                "payload": "{}",
                "task_id": "task-7",
                "commit_sha": "abc123",
                "diff_hash": "def456",
            }),
        )
        .await?;
        let append_text = append.content[0].as_text().unwrap().text.clone();
        let append_json: serde_json::Value = serde_json::from_str(&append_text)?;
        let claim_id = append_json["entry_id"].as_u64().unwrap();

        // 2. Gate before verification: not passed (no Verified evidence).
        let before = call_tool(
            &client,
            "gate_evaluate",
            json!({ "task_id": "task-7", "required": ["Verified"] }),
        )
        .await?;
        let before_text = before.content[0].as_text().unwrap().text.clone();
        let before_json: serde_json::Value = serde_json::from_str(&before_text)?;
        assert_eq!(before_json["passed"], false);
        assert_eq!(before_json["claimed"], 1);
        assert_eq!(before_json["verified"], 0);
        assert_eq!(before_json["missing"], json!(["Verified"]));

        // 3. Verify the claim.
        let _verify = call_tool(
            &client,
            "evidence_verify",
            json!({
                "task_id": "task-7",
                "claim_id": claim_id,
                "tool_name": "cargo --version",
                "args": [],
            }),
        )
        .await?;

        // 4. Gate after verification: must pass.
        let after = call_tool(
            &client,
            "gate_evaluate",
            json!({ "task_id": "task-7", "required": ["Verified"] }),
        )
        .await?;
        let after_text = after.content[0].as_text().unwrap().text.clone();
        let after_json: serde_json::Value = serde_json::from_str(&after_text)?;
        assert_eq!(after_json["passed"], true);
        assert_eq!(after_json["verified"], 1);
        assert_eq!(after_json["missing"], json!([]));
        Ok(())
    })
    .await
}

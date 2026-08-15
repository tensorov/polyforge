//! Integration tests for the PolyForge MCP server.
//!
//! Each test drives a real client/server pair over an in-memory duplex
//! stream and exercises the four tools end-to-end against a unique temp
//! ledger path.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use polyforge_core::ledger::Ledger;
use polyforge_mcp::server::{PolyForgeServer, TokenGatedServer};
use rmcp::{
    model::*,
    service::{serve_client, serve_directly, RoleClient, RoleServer, RunningService},
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
/// The body also receives the temp ledger path so tests can inspect the
/// appended entries directly.
async fn with_pair<F, Fut>(body: F) -> anyhow::Result<()>
where
    F: FnOnce(RunningService<RoleClient, TestClient>, PathBuf) -> Fut,
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

            let result = body(client, ledger).await;

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
    with_pair(|client, _ledger| async move {
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
    with_pair(|client, _ledger| async move {
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
    with_pair(|client, _ledger| async move {
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
    with_pair(|client, _ledger| async move {
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
    with_pair(|client, _ledger| async move {
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

#[tokio::test(flavor = "current_thread")]
async fn test_evidence_append_roundtrips_identity_fields() -> anyhow::Result<()> {
    with_pair(|client, ledger_path| async move {
        let result = call_tool(
            &client,
            "evidence_append",
            json!({
                "kind": "ModelClaim",
                "payload": "{}",
                "task_id": "task-5",
                "commit_sha": "abc123",
                "diff_hash": "def456",
                "experiment_id": "exp-1",
                "model_fingerprint": "fp-abc",
                "run_id": "run-9",
                "budget": "$5",
                "eval_metadata": {"metric": 0.9},
            }),
        )
        .await?;
        let text = result.content[0].as_text().unwrap().text.clone();
        let parsed: serde_json::Value = serde_json::from_str(&text)?;
        assert_eq!(parsed["kind"], "ModelClaim");
        assert_eq!(parsed["state"], "ModelClaimed");

        let ledger = Ledger::new(&ledger_path);
        let entries = ledger
            .iter_entries()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let entry = entries.first().unwrap();
        assert_eq!(entry.payload["experiment_id"], json!("exp-1"));
        assert_eq!(entry.payload["model_fingerprint"], json!("fp-abc"));
        assert_eq!(entry.payload["run_id"], json!("run-9"));
        assert_eq!(entry.payload["budget"], json!("$5"));
        assert_eq!(entry.payload["eval_metadata"], json!({"metric": 0.9}));
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_evidence_append_absent_identity_fields_default_null() -> anyhow::Result<()> {
    with_pair(|client, ledger_path| async move {
        let result = call_tool(
            &client,
            "evidence_append",
            json!({
                "kind": "ModelClaim",
                "payload": "{}",
                "task_id": "task-5",
                "commit_sha": "abc123",
                "diff_hash": "def456",
            }),
        )
        .await?;
        let text = result.content[0].as_text().unwrap().text.clone();
        let parsed: serde_json::Value = serde_json::from_str(&text)?;
        assert_eq!(parsed["kind"], "ModelClaim");

        let ledger = Ledger::new(&ledger_path);
        let entries = ledger
            .iter_entries()
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let entry = entries.first().unwrap();
        assert_eq!(entry.payload["experiment_id"], serde_json::Value::Null);
        assert_eq!(entry.payload["model_fingerprint"], serde_json::Value::Null);
        assert_eq!(entry.payload["run_id"], serde_json::Value::Null);
        assert_eq!(entry.payload["budget"], serde_json::Value::Null);
        assert_eq!(entry.payload["eval_metadata"], serde_json::Value::Null);
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_new_kinds_still_rejected_via_mcp() -> anyhow::Result<()> {
    with_pair(|client, _ledger| async move {
        for kind in [
            "EvalAttestation",
            "Discrepancy",
            "ToolAttestation",
            "Validation",
        ] {
            let result = call_tool(
                &client,
                "evidence_append",
                json!({
                    "kind": kind,
                    "payload": "{}",
                    "task_id": "task-5",
                    "commit_sha": "abc123",
                    "diff_hash": "def456",
                }),
            )
            .await;
            let err = result.expect_err("non-ModelClaim kind must be rejected");
            let msg = format!("{err}");
            assert!(
                msg.contains("only accepts kind=ModelClaim"),
                "kind {kind} rejected with unexpected error: {msg}"
            );
        }
        Ok(())
    })
    .await
}

/// Like `with_pair`, but wraps the server in `TokenGatedServer` with `token`.
async fn with_gated_pair<F, Fut>(token: Option<String>, body: F) -> anyhow::Result<()>
where
    F: FnOnce(RunningService<RoleClient, TestClient>, PathBuf) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let ledger = temp_ledger();
    let server = PolyForgeServer::new(&ledger).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let server = TokenGatedServer::new(server, token);
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

            let result = body(client, ledger).await;

            server_task.abort();
            result
        })
        .await
}

fn append_args(token: Option<&str>) -> serde_json::Value {
    let mut args = json!({
        "kind": "ModelClaim",
        "payload": "{}",
        "task_id": "task-t5",
        "commit_sha": "abc123",
        "diff_hash": "def456",
    });
    if let Some(token) = token {
        args["_pf_token"] = json!(token);
    }
    args
}

#[tokio::test(flavor = "current_thread")]
async fn test_token_gate_rejects_missing_token() -> anyhow::Result<()> {
    with_gated_pair(Some("secret".to_string()), |client, _ledger| async move {
        let result = call_tool(&client, "evidence_append", append_args(None)).await;
        let err = result.expect_err("missing _pf_token must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("-32001"), "unexpected error: {msg}");
        assert!(msg.contains("invalid _pf_token"), "unexpected error: {msg}");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_token_gate_rejects_wrong_token() -> anyhow::Result<()> {
    with_gated_pair(Some("secret".to_string()), |client, _ledger| async move {
        let result = call_tool(&client, "evidence_append", append_args(Some("wrong"))).await;
        let err = result.expect_err("wrong _pf_token must be rejected");
        let msg = format!("{err}");
        assert!(msg.contains("-32001"), "unexpected error: {msg}");
        assert!(msg.contains("invalid _pf_token"), "unexpected error: {msg}");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_token_gate_accepts_correct_token() -> anyhow::Result<()> {
    with_gated_pair(Some("secret".to_string()), |client, _ledger| async move {
        let result = call_tool(&client, "evidence_append", append_args(Some("secret"))).await;
        let result = result.expect("correct _pf_token must be accepted");
        assert!(!result.content.is_empty(), "expected a tool result");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_token_gate_leaves_tools_list_open() -> anyhow::Result<()> {
    with_gated_pair(Some("secret".to_string()), |client, _ledger| async move {
        let result = client.list_tools(None).await?;
        assert_eq!(result.tools.len(), 4, "tools/list must stay open");
        Ok(())
    })
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn test_token_gate_off_when_none() -> anyhow::Result<()> {
    // stdio path: no token configured, calls must pass without _pf_token.
    with_gated_pair(None, |client, _ledger| async move {
        let result = call_tool(&client, "evidence_append", append_args(None)).await;
        result.expect("token-free server must accept calls without _pf_token");
        Ok(())
    })
    .await
}

/// Retry connecting until the spawned server accepts (it binds asynchronously).
async fn connect_with_retry(port: u16) -> anyhow::Result<tokio::net::TcpStream> {
    for _ in 0..100 {
        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(stream) => return Ok(stream),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
    anyhow::bail!("server did not accept connections on port {port}")
}

#[tokio::test(flavor = "current_thread")]
async fn test_tcp_fails_closed_without_token() -> anyhow::Result<()> {
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_polyforge-mcp"))
        .env("PF_MCP_TRANSPORT", "tcp")
        .env("PF_MCP_ADDR", "127.0.0.1:0")
        .env_remove("PF_MCP_TOKEN")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let output = child.wait_with_output().await?;
    assert!(
        !output.status.success(),
        "tcp without token must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("PF_MCP_TOKEN"), "stderr: {stderr}");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_tcp_refuses_non_loopback_bind() -> anyhow::Result<()> {
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_polyforge-mcp"))
        .env("PF_MCP_TRANSPORT", "tcp")
        .env("PF_MCP_ADDR", "0.0.0.0:18888")
        .env("PF_MCP_TOKEN", "test-token")
        .env_remove("PF_MCP_ALLOW_NON_LOOPBACK")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let output = child.wait_with_output().await?;
    assert!(
        !output.status.success(),
        "non-loopback bind without override must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("non-loopback"), "stderr: {stderr}");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_tcp_roundtrip_with_token() -> anyhow::Result<()> {
    let ledger = temp_ledger();
    let (stream, child) = loop {
        // Find a free port, then hand it to the spawned server. The probe is
        // dropped before the child binds, so a parallel test could steal the
        // port; retry the whole spawn on a fresh port if the child exits
        // before accepting a connection.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = probe.local_addr()?.port();
        drop(probe);

        let mut candidate = tokio::process::Command::new(env!("CARGO_BIN_EXE_polyforge-mcp"))
            .env("PF_MCP_TRANSPORT", "tcp")
            .env("PF_MCP_ADDR", format!("127.0.0.1:{port}"))
            .env("PF_MCP_TOKEN", "test-token")
            .env("PF_MCP_LEDGER", &ledger)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;

        match connect_with_retry(port).await {
            Ok(stream) => break (stream, Some(candidate)),
            Err(_) => {
                let _ = candidate.kill().await;
                let _ = candidate.wait().await;
            }
        }
    };
    let client = serve_client::<_, _, _, _>(TestClient, stream).await?;

    // tools/list stays open without a token.
    let tools = client.list_tools(None).await?;
    assert_eq!(tools.tools.len(), 4, "tools/list must stay open over TCP");

    // Missing token is rejected with -32001.
    let err = call_tool(&client, "evidence_append", append_args(None))
        .await
        .expect_err("missing _pf_token must be rejected over TCP");
    let msg = format!("{err}");
    assert!(msg.contains("-32001"), "unexpected error: {msg}");
    assert!(msg.contains("invalid _pf_token"), "unexpected error: {msg}");

    // Wrong token is rejected with -32001.
    let err = call_tool(&client, "evidence_append", append_args(Some("wrong")))
        .await
        .expect_err("wrong _pf_token must be rejected over TCP");
    let msg = format!("{err}");
    assert!(msg.contains("-32001"), "unexpected error: {msg}");
    assert!(msg.contains("invalid _pf_token"), "unexpected error: {msg}");

    // Correct token is accepted.
    let result = call_tool(&client, "evidence_append", append_args(Some("test-token"))).await;
    result.expect("correct _pf_token must be accepted over TCP");

    if let Some(mut child) = child {
        child.kill().await?;
        child.wait().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn test_stdio_roundtrip_without_token() -> anyhow::Result<()> {
    let ledger = temp_ledger();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_polyforge-mcp"))
        .env("PF_MCP_TRANSPORT", "stdio")
        .env("PF_MCP_LEDGER", &ledger)
        .env_remove("PF_MCP_TOKEN")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let client = serve_client::<_, _, _, _>(TestClient, (stdout, stdin)).await?;

    // stdio stays token-free: a call without _pf_token succeeds.
    let result = call_tool(&client, "evidence_append", append_args(None)).await;
    result.expect("stdio must accept calls without _pf_token");

    child.kill().await?;
    child.wait().await?;
    Ok(())
}

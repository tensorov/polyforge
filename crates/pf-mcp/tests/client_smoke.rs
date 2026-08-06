//! T8 army validator harness proof-of-life: spawn the REAL pf-mcp server
//! binary as a subprocess over stdio and drive it with a REAL rmcp client.
//!
//! No external MCP registry / network required — everything runs over the
//! child process's stdin/stdout.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rmcp::{
    model::*,
    service::{serve_directly, RoleClient, RunningService},
    ClientHandler,
};
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tokio::time::timeout;

/// Unique ledger path per run so parallel test executions never collide.
static LEDGER_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_ledger_path() -> PathBuf {
    let n = LEDGER_COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("pf-mcp-army-{}-{}.jsonl", std::process::id(), n))
}

/// Minimal tools-only client (all `ClientHandler` methods have defaults).
struct TestClient;

impl ClientHandler for TestClient {}

fn server_info() -> ServerInfo {
    let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
    info.protocol_version = ProtocolVersion::V_2026_07_28;
    info
}

/// Spawn the real server binary with a fresh ledger and return the child plus
/// a connected rmcp client over its stdio.
async fn spawn_server(ledger: &Path) -> (Child, RunningService<RoleClient, TestClient>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pf-mcp"))
        .env("PF_MCP_LEDGER", ledger.as_os_str())
        .kill_on_drop(true)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn pf-mcp binary");

    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");

    let client = serve_directly::<RoleClient, _, _, _, _>(
        TestClient,
        (stdout, stdin),
        Some(server_info().into()),
    );
    (child, client)
}

/// Call a tool and return the parsed JSON result (or the error text).
async fn call_tool(
    client: &RunningService<RoleClient, TestClient>,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    let params = CallToolRequestParams::new(name.to_string())
        .with_arguments(serde_json::from_value(arguments).unwrap());
    let result: CallToolResult = client
        .call_tool(params)
        .await
        .map_err(|e| format!("call_tool error: {e:?}"))?;

    if result.is_error == Some(true) {
        let text = result
            .content
            .iter()
            .filter_map(|b| b.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("tool error: {text}"));
    }

    let text = result
        .content
        .iter()
        .filter_map(|b| b.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");

    serde_json::from_str(&text).map_err(|e| format!("non-JSON result: {e}: {text}"))
}

/// Kill the child and wait for it to exit (no orphan binaries).
async fn teardown(child: &mut Child) {
    let _ = child.kill().await;
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_army_smoke_full_flow() {
    let ledger = unique_ledger_path();
    let (mut child, client) = spawn_server(&ledger).await;

    // 1. Append a model claim.
    let append = call_tool(
        &client,
        "evidence_append",
        json!({
            "kind": "ModelClaim",
            "payload": "{\"note\":\"army smoke claim\"}",
            "task_id": "task-8-army-smoke",
            "commit_sha": "abc123",
            "diff_hash": "def456",
        }),
    )
    .await
    .expect("evidence_append should succeed");
    let entry_id = append["entry_id"].as_u64().expect("entry_id");
    assert_eq!(append["state"], "ModelClaimed");

    // 2. Verify with the allowlisted `cargo --version` tool.
    let verify = call_tool(
        &client,
        "evidence_verify",
        json!({
            "task_id": "task-8-army-smoke",
            "claim_id": entry_id,
            "tool_name": "cargo --version",
            "args": [],
        }),
    )
    .await
    .expect("evidence_verify should succeed");
    assert_eq!(verify["state"], "Verified");
    assert_eq!(verify["exit_code"], 0);

    // 3. Evaluate the gate (empty required => passed when chain intact).
    let eval = call_tool(
        &client,
        "gate_evaluate",
        json!({ "task_id": "task-8-army-smoke" }),
    )
    .await
    .expect("gate_evaluate should succeed");
    assert_eq!(
        eval["passed"], true,
        "gate should pass on intact ledger: {eval}"
    );
    assert_eq!(eval["task_id"], "task-8-army-smoke");
    assert_eq!(eval["verified"], 1);
    assert!(!eval["chain_tail_hash"].as_str().unwrap().is_empty());

    // 4. Read-only bundle snapshot.
    let report = call_tool(
        &client,
        "gate_report",
        json!({ "task_id": "task-8-army-smoke" }),
    )
    .await
    .expect("gate_report should succeed");
    assert_eq!(report["task_id"], "task-8-army-smoke");
    assert_eq!(report["passed"], true);
    assert!(!report["bundle_sha256"].as_str().unwrap().is_empty());
    assert!(!report["entries"].as_array().unwrap().is_empty());

    teardown(&mut child).await;
}

#[tokio::test(flavor = "current_thread")]
async fn test_army_smoke_negative() {
    let ledger = unique_ledger_path();
    let (mut child, client) = spawn_server(&ledger).await;

    // Append a claim so the task exists on an intact chain.
    let _append = call_tool(
        &client,
        "evidence_append",
        json!({
            "kind": "ModelClaim",
            "payload": "{\"note\":\"pre-tamper claim\"}",
            "task_id": "task-8-army-tamper",
            "commit_sha": "abc123",
            "diff_hash": "def456",
        }),
    )
    .await
    .expect("append should succeed");

    // Stop the server, tamper the ledger, then respawn on the SAME ledger.
    teardown(&mut child).await;

    // Corrupt the ledger: flip a byte in the tail line so the merkle chain breaks.
    let raw = std::fs::read_to_string(&ledger).expect("read ledger");
    let mut lines: Vec<String> = raw.lines().map(|l| l.to_string()).collect();
    assert!(!lines.is_empty(), "ledger should have at least one line");
    let last = lines.pop().expect("last line");
    let mut bytes = last.into_bytes();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    lines.push(String::from_utf8(bytes).expect("utf8"));
    std::fs::write(&ledger, lines.join("\n") + "\n").expect("write tampered ledger");

    // Respawn on the tampered ledger.
    let (mut child2, client2) = spawn_server(&ledger).await;

    // gate_evaluate must now fail (chain integrity error) or return failed.
    let eval = call_tool(
        &client2,
        "gate_evaluate",
        json!({ "task_id": "task-8-army-tamper" }),
    )
    .await;

    match eval {
        Ok(v) => {
            let passed = v.get("passed").and_then(Value::as_bool).unwrap_or(true);
            assert!(!passed, "gate must NOT pass on tampered ledger, got: {v}");
        }
        Err(e) => {
            assert!(!e.is_empty(), "gate_evaluate error should be non-empty");
        }
    }

    teardown(&mut child2).await;
}

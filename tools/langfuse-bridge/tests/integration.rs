//! Integration tests for the optional Langfuse bridge.
//! A std TcpListener on an ephemeral port acts as the mock server; no real Langfuse.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use langfuse_bridge::{
    base64_encode, parse_base_url, post_score, prepare, resolve_trace_id, score_payload,
    PrepareOutcome,
};

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn write_temp_manifest(contents: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!(
        "langfuse-bridge-it-{}-{n}.json",
        std::process::id()
    ));
    std::fs::write(&path, contents).expect("write temp manifest");
    path
}

/// Reads exactly one HTTP request (headers plus Content-Length body), answers 200,
/// and returns the raw request bytes.
fn serve_one_request(listener: TcpListener) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("mock server accept");
        let mut raw: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        let header_end = loop {
            let n = stream.read(&mut buf).expect("mock server read");
            if n == 0 {
                break raw.len();
            }
            raw.extend_from_slice(&buf[..n]);
            if let Some(pos) = find_header_end(&raw) {
                break pos + 4;
            }
        };
        let content_length = content_length_of(&raw[..header_end.min(raw.len())]);
        while raw.len() < header_end + content_length {
            let n = stream.read(&mut buf).expect("mock server read body");
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }
        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        let _ = stream.flush();
        raw
    })
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length_of(head: &[u8]) -> usize {
    String::from_utf8_lossy(head)
        .lines()
        .find_map(|line| {
            let value = line.strip_prefix("Content-Length:")?;
            value.trim().parse::<usize>().ok()
        })
        .unwrap_or(0)
}

fn body_of(raw: &[u8]) -> &[u8] {
    find_header_end(raw)
        .map(|pos| &raw[pos + 4..])
        .unwrap_or(&[])
}

fn assert_no_pending_connections(listener: &TcpListener) {
    listener.set_nonblocking(true).expect("set nonblocking");
    let result = listener.accept();
    listener.set_nonblocking(false).ok();
    assert!(
        matches!(result, Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock),
        "expected zero incoming connections"
    );
}

#[test]
fn happy_path_posts_gate_score_with_auth_header() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();
    let server = serve_one_request(listener);

    // run_id wins over metadata.langfuse_trace_id when both are present.
    let manifest_path = write_temp_manifest(
        r#"{
            "task_id": "t11-demo",
            "tail_hash": "abc123",
            "passed": true,
            "bundle_sha256": "def456",
            "tool_versions": {},
            "run_id": "run-42",
            "metadata": { "langfuse_trace_id": "meta-trace" }
        }"#,
    );

    let outcome = prepare(&manifest_path).expect("prepare succeeds");
    let PrepareOutcome::Proceed { manifest, trace_id } = outcome else {
        panic!("expected Proceed for manifest with run_id");
    };
    assert_eq!(trace_id, "run-42");
    assert_eq!(resolve_trace_id(&manifest).as_deref(), Some("run-42"));

    let payload = score_payload(&manifest, &trace_id);
    post_score(
        &format!("http://127.0.0.1:{port}"),
        "pk-test",
        "sk-test",
        &payload,
    )
    .expect("post succeeds against mock server");

    let raw = server.join().expect("mock server thread");
    let head = String::from_utf8_lossy(&raw);
    assert!(
        head.starts_with("POST /api/public/ingestion HTTP/1.1\r\n"),
        "unexpected request line: {head}"
    );
    let expected_credentials = base64_encode(b"pk-test:sk-test");
    assert!(
        head.contains(&format!("Authorization: Basic {expected_credentials}\r\n")),
        "missing basic auth header in: {head}"
    );

    let body: serde_json::Value =
        serde_json::from_slice(body_of(&raw)).expect("body is valid json");
    assert_eq!(body["name"], "gate");
    assert_eq!(body["value"], 1);
    assert_eq!(body["traceId"], "run-42");

    let _ = std::fs::remove_file(&manifest_path);
}

#[test]
fn missing_trace_id_skips_with_warning_and_zero_requests() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");

    let manifest_path = write_temp_manifest(
        r#"{
            "task_id": "t11-no-trace",
            "tail_hash": "abc123",
            "passed": false,
            "bundle_sha256": null,
            "tool_versions": {}
        }"#,
    );

    let outcome = prepare(&manifest_path).expect("prepare succeeds");
    let PrepareOutcome::Skip { warning } = outcome else {
        panic!("expected Skip for manifest without any trace id");
    };
    assert!(warning.contains("skipping"), "warning text: {warning}");
    // Exit 0 semantics: main maps Skip to success; nothing was posted.
    assert_no_pending_connections(&listener);

    let _ = std::fs::remove_file(&manifest_path);
}

#[test]
fn closed_port_post_fails_once_naming_the_url() {
    let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = probe.local_addr().expect("local addr").port();
    drop(probe); // port is now closed

    let url = format!("http://127.0.0.1:{port}");
    let started = std::time::Instant::now();
    let result = post_score(&url, "pk", "sk", r#"{"name":"gate"}"#);
    let elapsed = started.elapsed();

    let message = result.expect_err("closed port must fail");
    assert!(message.contains(&url), "error must name the URL: {message}");
    // Single attempt by construction (no retry loop): a retry storm would multiply
    // this connect-refused latency well past one second.
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "single fast attempt expected, took {elapsed:?}"
    );

    let endpoint = parse_base_url(&url).expect("url parses");
    assert_eq!(endpoint.port, port);
}

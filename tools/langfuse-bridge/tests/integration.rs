//! Integration tests for the optional Langfuse bridge.
//! A std TcpListener on an ephemeral port acts as the mock server; no real Langfuse.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

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

/// Bounded so a mutant that never connects fails fast instead of hanging
/// cargo-mutants for its full 120s timeout.
const MOCK_ACCEPT_DEADLINE: Duration = Duration::from_secs(15);

/// Reads one HTTP request (headers plus Content-Length body), answers with
/// `response_bytes`, returns the raw request bytes. Resolves to an empty Vec
/// when no client connects before the deadline; request assertions treat that
/// as failure, which kills network-skipping mutants.
fn serve_raw(
    listener: TcpListener,
    response_bytes: &'static [u8],
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        listener
            .set_nonblocking(true)
            .expect("mock server set nonblocking");
        let deadline = Instant::now() + MOCK_ACCEPT_DEADLINE;
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Vec::new();
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("mock server accept: {err}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("mock server read timeout");
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
        let _ = stream.write_all(response_bytes);
        let _ = stream.flush();
        raw
    })
}

const OK_RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

/// Reads exactly one HTTP request (headers plus Content-Length body), answers 200,
/// and returns the raw request bytes.
fn serve_one_request(listener: TcpListener) -> std::thread::JoinHandle<Vec<u8>> {
    serve_raw(listener, OK_RESPONSE)
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

#[test]
fn base64_encode_matches_rfc4648_golden_vectors() {
    let vectors: &[(&[u8], &str)] = &[
        (b"", ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"fooba", "Zm9vYmE="),
        (b"foobar", "Zm9vYmFy"),
        (b"pk-test:sk-test", "cGstdGVzdDpzay10ZXN0"),
        (b"\x00\xff\x10\xfb", "AP8Q+w=="),
    ];
    for (input, expected) in vectors {
        assert_eq!(
            &base64_encode(input),
            expected,
            "base64 mismatch for {input:?}"
        );
    }
}

#[test]
fn non_2xx_status_fails_naming_url_and_status_line() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();
    let server = serve_raw(
        listener,
        b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );

    let url = format!("http://127.0.0.1:{port}");
    let message =
        post_score(&url, "pk", "sk", r#"{"name":"gate"}"#).expect_err("500 must fail the post");

    assert!(message.contains(&url), "error must name the URL: {message}");
    assert!(
        message.contains("500"),
        "error must quote the status line: {message}"
    );
    server.join().expect("mock server thread");
}

#[test]
fn garbage_status_line_fails_with_unexpected_status_message() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();
    let server = serve_raw(listener, b"not-http at all\r\n\r\n");

    let url = format!("http://127.0.0.1:{port}");
    let message = post_score(&url, "pk", "sk", r#"{"name":"gate"}"#)
        .expect_err("garbage status line must fail");

    assert!(message.contains("unexpected status line"), "{message}");
    assert!(message.contains("not-http at all"), "{message}");
    server.join().expect("mock server thread");
}

#[test]
fn empty_response_body_fails_as_unexpected_status_line() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();
    let server = serve_raw(listener, b"");

    let url = format!("http://127.0.0.1:{port}");
    let message =
        post_score(&url, "pk", "sk", r#"{"name":"gate"}"#).expect_err("empty response must fail");

    assert!(message.contains("unexpected status line"), "{message}");
    server.join().expect("mock server thread");
}

#[test]
fn http_201_created_is_accepted_within_success_range() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();
    let server = serve_raw(
        listener,
        b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );

    let url = format!("http://127.0.0.1:{port}");
    post_score(&url, "pk", "sk", r#"{"name":"gate"}"#).expect("2xx statuses are accepted");
    server.join().expect("mock server thread");
}

fn bridge_bin() -> &'static str {
    env!("CARGO_BIN_EXE_langfuse-bridge")
}

struct CliRun {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run_cli(args: &[&str], envs: &[(&str, &str)]) -> CliRun {
    let output = Command::new(bridge_bin())
        .args(args)
        .env_clear()
        .envs(envs.iter().copied())
        .output()
        .expect("spawn langfuse-bridge binary");
    CliRun {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn cli_no_args_exits_2_printing_usage_on_stderr() {
    let run = run_cli(&[], &[]);
    assert_eq!(
        run.code,
        Some(2),
        "stdout={:?} stderr={:?}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.stdout.is_empty(),
        "usage goes to stderr: {:?}",
        run.stdout
    );
    assert!(
        run.stderr
            .contains("usage: langfuse-bridge <gate-manifest.json> [--dry-run]"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn cli_help_exits_0_listing_usage_and_env_names() {
    for flag in ["-h", "--help"] {
        let run = run_cli(&[flag], &[]);
        assert_eq!(run.code, Some(0), "flag {flag}: stderr={}", run.stderr);
        assert!(run.stderr.is_empty(), "flag {flag}: {}", run.stderr);
        for needle in ["usage: langfuse-bridge", "LF_BASE_URL", "LF_PK", "LF_SK"] {
            assert!(
                run.stdout.contains(needle),
                "flag {flag}: missing {needle:?} in {:?}",
                run.stdout
            );
        }
    }
}

#[test]
fn cli_extra_positional_argument_exits_2() {
    let manifest_path =
        write_temp_manifest(r#"{ "task_id": "cli-extra", "passed": true, "run_id": "r" }"#);
    let manifest = manifest_path.to_str().expect("utf8 temp path");
    let run = run_cli(&[manifest, "surplus"], &[]);
    assert_eq!(run.code, Some(2));
    assert!(
        run.stderr.contains("unexpected extra argument: surplus"),
        "stderr: {}",
        run.stderr
    );
    let _ = std::fs::remove_file(&manifest_path);
}

#[test]
fn cli_missing_manifest_file_exits_2_naming_the_path() {
    let run = run_cli(&["/nonexistent/gate-manifest.json"], &[]);
    assert_eq!(run.code, Some(2));
    assert!(
        run.stderr
            .contains("cannot read manifest /nonexistent/gate-manifest.json"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn cli_missing_env_vars_exit_2_naming_the_first_unset_variable() {
    let manifest_path =
        write_temp_manifest(r#"{ "task_id": "cli-env", "passed": true, "run_id": "run-env-1" }"#);
    let manifest = manifest_path.to_str().expect("utf8 temp path");

    let none_set = run_cli(&[manifest], &[]);
    assert_eq!(none_set.code, Some(2), "stderr={}", none_set.stderr);
    assert!(
        none_set.stderr.contains("LF_BASE_URL is not set"),
        "stderr: {}",
        none_set.stderr
    );

    let partial = run_cli(&[manifest], &[("LF_BASE_URL", "http://127.0.0.1:1")]);
    assert_eq!(partial.code, Some(2), "stderr={}", partial.stderr);
    assert!(
        partial.stderr.contains("LF_PK is not set"),
        "stderr: {}",
        partial.stderr
    );

    let _ = std::fs::remove_file(&manifest_path);
}

#[test]
fn cli_dry_run_prints_exact_payload_bytes_and_posts_nothing() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    let port = listener.local_addr().expect("local addr").port();

    let manifest_path =
        write_temp_manifest(r#"{ "task_id": "cli-dry", "passed": true, "run_id": "run-dry-1" }"#);
    let manifest = manifest_path.to_str().expect("utf8 temp path");

    let run = run_cli(
        &[manifest, "--dry-run"],
        &[
            ("LF_BASE_URL", &format!("http://127.0.0.1:{port}")),
            ("LF_PK", "pk-live"),
            ("LF_SK", "sk-live"),
        ],
    );

    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);
    assert_eq!(
        run.stdout, "{\"name\":\"gate\",\"traceId\":\"run-dry-1\",\"value\":1}\n",
        "dry-run payload bytes must be exact"
    );
    assert_no_pending_connections(&listener);

    let _ = std::fs::remove_file(&manifest_path);
}

#[test]
fn cli_dry_run_reflected_gate_failure_emits_value_zero() {
    let manifest_path = write_temp_manifest(
        r#"{ "task_id": "cli-dry-fail", "passed": false, "metadata": { "langfuse_trace_id": "meta-only" } }"#,
    );
    let manifest = manifest_path.to_str().expect("utf8 temp path");

    let run = run_cli(&[manifest, "--dry-run"], &[]);
    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);
    assert_eq!(
        run.stdout,
        "{\"name\":\"gate\",\"traceId\":\"meta-only\",\"value\":0}\n"
    );

    let _ = std::fs::remove_file(&manifest_path);
}

#[test]
fn cli_skip_without_trace_ids_warns_exits_zero_posts_nothing() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind probe listener");
    let port = listener.local_addr().expect("local addr").port();

    let manifest_path = write_temp_manifest(r#"{ "task_id": "cli-skip", "passed": false }"#);
    let manifest = manifest_path.to_str().expect("utf8 temp path");

    let run = run_cli(
        &[manifest],
        &[
            ("LF_BASE_URL", &format!("http://127.0.0.1:{port}")),
            ("LF_PK", "pk"),
            ("LF_SK", "sk"),
        ],
    );

    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);
    assert!(run.stdout.is_empty(), "{:?}", run.stdout);
    assert!(
        run.stderr
            .starts_with("warning: no run_id and no metadata.langfuse_trace_id in "),
        "stderr: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("skipping Langfuse post"),
        "{}",
        run.stderr
    );
    assert_no_pending_connections(&listener);

    let _ = std::fs::remove_file(&manifest_path);
}

#[test]
fn cli_failed_post_exits_1_naming_the_url() {
    let probe = TcpListener::bind("127.0.0.1:0").expect("bind probe");
    let port = probe.local_addr().expect("local addr").port();
    drop(probe);

    let manifest_path =
        write_temp_manifest(r#"{ "task_id": "cli-fail", "passed": true, "run_id": "run-x" }"#);
    let manifest = manifest_path.to_str().expect("utf8 temp path");

    let run = run_cli(
        &[manifest],
        &[
            ("LF_BASE_URL", &format!("http://127.0.0.1:{port}")),
            ("LF_PK", "pk"),
            ("LF_SK", "sk"),
        ],
    );

    assert_eq!(run.code, Some(1), "stderr={}", run.stderr);
    assert!(
        run.stderr.contains(&format!("http://127.0.0.1:{port}")),
        "stderr: {}",
        run.stderr
    );

    let _ = std::fs::remove_file(&manifest_path);
}

#[test]
fn cli_successful_post_prints_task_and_trace_confirmation() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let port = listener.local_addr().expect("local addr").port();
    let server = serve_one_request(listener);

    let manifest_path = write_temp_manifest(
        r#"{ "task_id": "cli-ok-task", "passed": true, "run_id": "run-cli-ok" }"#,
    );
    let manifest = manifest_path.to_str().expect("utf8 temp path");

    let run = run_cli(
        &[manifest],
        &[
            ("LF_BASE_URL", &format!("http://127.0.0.1:{port}")),
            ("LF_PK", "pk"),
            ("LF_SK", "sk"),
        ],
    );

    assert_eq!(run.code, Some(0), "stderr={}", run.stderr);
    assert_eq!(
        run.stdout,
        "posted gate score for task cli-ok-task (trace run-cli-ok)\n"
    );

    let raw = server.join().expect("mock server thread");
    assert!(!raw.is_empty(), "a request must reach the server");
    let _ = std::fs::remove_file(&manifest_path);
}

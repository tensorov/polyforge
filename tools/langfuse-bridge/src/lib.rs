//! Optional analytics bridge between PolyForge gate manifests and a self-hosted
//! Langfuse instance. This crate is NEVER part of the PolyForge trust contour:
//! gates never depend on it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// Parsed gate manifest (`gate-<task_id>.manifest.json` written by polyforge-cli).
#[derive(Debug, Clone)]
pub struct GateManifest {
    pub task_id: String,
    pub tail_hash: Option<String>,
    pub passed: bool,
    pub bundle_sha256: Option<String>,
    pub run_id: Option<String>,
    pub langfuse_trace_id: Option<String>,
}

/// Result of preparing a bridge run from a manifest path.
#[derive(Debug)]
pub enum PrepareOutcome {
    /// Trace id resolved; safe to build and post the score.
    Proceed {
        manifest: GateManifest,
        trace_id: String,
    },
    /// No trace id found; print the warning on stderr and exit 0 with zero HTTP posts.
    Skip { warning: String },
}

/// Normalized pieces of `LF_BASE_URL`.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    /// Request path including any base-path prefix, always ending in `/api/public/ingestion`.
    pub ingestion_path: String,
    /// Full URL used in error messages.
    pub url: String,
}

const INGESTION_SUFFIX: &str = "/api/public/ingestion";
const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Reads and validates one gate manifest from disk.
pub fn load_manifest(path: &Path) -> Result<GateManifest, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read manifest {}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("cannot parse manifest {}: {e}", path.display()))?;
    let task_id = value
        .get("task_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("manifest {} is missing string field task_id", path.display()))?
        .to_owned();
    let passed = value
        .get("passed")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| format!("manifest {} is missing boolean field passed", path.display()))?;
    let opt_str = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    let langfuse_trace_id = value
        .get("metadata")
        .and_then(|meta| meta.get("langfuse_trace_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok(GateManifest {
        task_id,
        tail_hash: opt_str("tail_hash"),
        passed,
        bundle_sha256: opt_str("bundle_sha256"),
        run_id: opt_str("run_id"),
        langfuse_trace_id,
    })
}

/// Trace id priority: `run_id` first, then `metadata.langfuse_trace_id`.
pub fn resolve_trace_id(manifest: &GateManifest) -> Option<String> {
    manifest
        .run_id
        .clone()
        .or_else(|| manifest.langfuse_trace_id.clone())
}

/// Loads the manifest and decides whether a post should proceed or be skipped.
///
/// `Skip` maps to exit 0 semantics in main: one warning line, zero HTTP posts.
pub fn prepare(manifest_path: &Path) -> Result<PrepareOutcome, String> {
    let manifest = load_manifest(manifest_path)?;
    match resolve_trace_id(&manifest) {
        Some(trace_id) => Ok(PrepareOutcome::Proceed { manifest, trace_id }),
        None => Ok(PrepareOutcome::Skip {
            warning: format!(
                "warning: no run_id and no metadata.langfuse_trace_id in {}; skipping Langfuse post",
                manifest_path.display()
            ),
        }),
    }
}

/// Builds the exact score payload bytes posted to Langfuse.
pub fn score_payload(manifest: &GateManifest, trace_id: &str) -> String {
    serde_json::json!({
        "name": "gate",
        "value": if manifest.passed { 1 } else { 0 },
        "traceId": trace_id,
    })
    .to_string()
}

/// Parses `LF_BASE_URL`. Plain http only; TLS is intentionally unsupported.
pub fn parse_base_url(base_url: &str) -> Result<Endpoint, String> {
    let trimmed = base_url.trim();
    let rest = trimmed.strip_prefix("http://").ok_or_else(|| {
        format!("langfuse bridge: LF_BASE_URL must use plain http (TLS unsupported): {trimmed}")
    })?;
    let (authority, prefix) = match rest.find('/') {
        Some(idx) => (&rest[..idx], rest[idx..].trim_end_matches('/')),
        None => (rest, ""),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (
            h.to_owned(),
            p.parse::<u16>()
                .map_err(|_| format!("langfuse bridge: invalid port in LF_BASE_URL: {trimmed}"))?,
        ),
        None => (authority.to_owned(), 80),
    };
    if host.is_empty() {
        return Err(format!(
            "langfuse bridge: empty host in LF_BASE_URL: {trimmed}"
        ));
    }
    let ingestion_path = format!("{prefix}{INGESTION_SUFFIX}");
    Ok(Endpoint {
        url: format!("http://{host}:{port}{ingestion_path}"),
        host,
        port,
        ingestion_path,
    })
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Minimal standard base64 encoder (no external crates allowed here).
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(char::from(BASE64_ALPHABET[((triple >> 18) & 63) as usize]));
        out.push(char::from(BASE64_ALPHABET[((triple >> 12) & 63) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(BASE64_ALPHABET[((triple >> 6) & 63) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(BASE64_ALPHABET[(triple & 63) as usize])
        } else {
            '='
        });
    }
    out
}

/// Performs exactly ONE POST of `payload` to `{base_url}/api/public/ingestion`
/// with `Authorization: Basic base64(public_key:secret_key)`. No retries.
///
/// Every error message names the full target URL.
pub fn post_score(
    base_url: &str,
    public_key: &str,
    secret_key: &str,
    payload: &str,
) -> Result<(), String> {
    let endpoint = parse_base_url(base_url)?;
    let addr = format!("{}:{}", endpoint.host, endpoint.port);
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("langfuse bridge: POST {} failed: cannot connect to {addr}: {e}", endpoint.url))?;
    stream
        .set_write_timeout(Some(IO_TIMEOUT))
        .map_err(|e| format!("langfuse bridge: POST {} failed: set write timeout: {e}", endpoint.url))?;
    stream
        .set_read_timeout(Some(IO_TIMEOUT))
        .map_err(|e| format!("langfuse bridge: POST {} failed: set read timeout: {e}", endpoint.url))?;

    let body = payload.as_bytes();
    let credentials = base64_encode(format!("{public_key}:{secret_key}").as_bytes());
    let request = format!(
        "POST {} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Basic {credentials}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.ingestion_path,
        body.len(),
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("langfuse bridge: POST {} failed: write request: {e}", endpoint.url))?;
    stream
        .write_all(body)
        .map_err(|e| format!("langfuse bridge: POST {} failed: write body: {e}", endpoint.url))?;
    stream
        .flush()
        .map_err(|e| format!("langfuse bridge: POST {} failed: flush: {e}", endpoint.url))?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|e| format!("langfuse bridge: POST {} failed: read response: {e}", endpoint.url))?;
    let text = String::from_utf8_lossy(&response);
    let status_line = text.lines().next().unwrap_or_default();
    let status_ok = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|code| (200..300).contains(&code));
    if !status_ok {
        return Err(format!(
            "langfuse bridge: POST {} failed: unexpected status line: {status_line}",
            endpoint.url
        ));
    }
    Ok(())
}

//! PolyForge MCP server binary entry point.
//!
//! The server core lives in `lib.rs` so integration tests can drive the
//! server over an in-memory duplex transport. This binary only selects the
//! transport (stdio by default, tcp when `PF_MCP_TRANSPORT=tcp`) and serves.

use std::path::PathBuf;

use polyforge_mcp::server::{PolyForgeServer, TokenGatedServer};
use rmcp::service::ServiceExt;

/// Ledger path: default `.pf/ledger.jsonl`, overridable via `PF_MCP_LEDGER`.
fn ledger_path() -> PathBuf {
    std::env::var("PF_MCP_LEDGER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(".pf/ledger.jsonl"))
}

/// True when `addr` binds to a loopback interface. Handles IPv4
/// (`127.0.0.1:18888`), hostnames (`localhost:18888`), and IPv6 with or
/// without brackets (`[::1]:18888`, `::1`).
fn is_loopback_addr(addr: &str) -> bool {
    let addr = addr.strip_prefix("tcp://").unwrap_or(addr);
    let host = if let Some(rest) = addr.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else if addr.parse::<std::net::IpAddr>().is_ok() {
        addr
    } else {
        addr.split(':').next().unwrap_or(addr)
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let transport = std::env::var("PF_MCP_TRANSPORT").unwrap_or_else(|_| "stdio".to_string());
    let ledger_path = ledger_path();

    let server =
        PolyForgeServer::new(ledger_path).map_err(|e| format!("failed to open ledger: {e:?}"))?;

    match transport.as_str() {
        "stdio" => {
            server
                .serve(rmcp::transport::stdio())
                .await?
                .waiting()
                .await?;
        }
        "tcp" => {
            let addr =
                std::env::var("PF_MCP_ADDR").unwrap_or_else(|_| "127.0.0.1:18888".to_string());
            let token = match std::env::var("PF_MCP_TOKEN") {
                Ok(value) if !value.is_empty() => Some(value),
                _ => {
                    eprintln!(
                        "polyforge-mcp: PF_MCP_TRANSPORT=tcp requires a non-empty PF_MCP_TOKEN \
                         (fail closed)"
                    );
                    std::process::exit(1);
                }
            };
            if !is_loopback_addr(&addr)
                && std::env::var("PF_MCP_ALLOW_NON_LOOPBACK").unwrap_or_default() != "1"
            {
                eprintln!(
                    "polyforge-mcp: refusing non-loopback bind address {addr:?}; \
                     set PF_MCP_ALLOW_NON_LOOPBACK=1 to override"
                );
                std::process::exit(1);
            }
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            eprintln!("polyforge-mcp listening on {addr} (PF_MCP_TRANSPORT=tcp)");
            let server = TokenGatedServer::new(server, token);
            loop {
                let (stream, _) = listener.accept().await?;
                let server = server.clone();
                tokio::spawn(async move {
                    if let Ok(running) = server.serve(stream).await {
                        let _ = running.waiting().await;
                    }
                });
            }
        }
        other => {
            return Err(format!(
                "unknown PF_MCP_TRANSPORT={other:?} (expected \"stdio\" or \"tcp\")"
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::ledger_path;
    use std::path::PathBuf;

    /// With no PF_MCP_LEDGER override, the compiled-in default must resolve
    /// under `.pf/` — the tracked runtime home (C7 relocation).
    #[test]
    fn test_default_ledger_resolves_under_pf() {
        // Clear any ambient override so this test exercises the default
        // regardless of the invoking shell's environment.
        std::env::remove_var("PF_MCP_LEDGER");

        assert_eq!(ledger_path(), PathBuf::from(".pf/ledger.jsonl"));
    }
}

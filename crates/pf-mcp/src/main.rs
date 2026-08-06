//! PolyForge MCP server binary entry point.
//!
//! The server core lives in `lib.rs` so integration tests can drive the
//! server over an in-memory duplex transport. This binary only selects the
//! transport (stdio by default, tcp when `PF_MCP_TRANSPORT=tcp`) and serves.

use pf_mcp::server::PolyForgeServer;
use rmcp::service::ServiceExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let transport = std::env::var("PF_MCP_TRANSPORT").unwrap_or_else(|_| "stdio".to_string());
    let ledger_path =
        std::env::var("PF_MCP_LEDGER").unwrap_or_else(|_| ".omo/ledger.jsonl".to_string());

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
            let listener = tokio::net::TcpListener::bind(&addr).await?;
            eprintln!("pf-mcp listening on {addr} (PF_MCP_TRANSPORT=tcp)");
            loop {
                let (stream, _) = listener.accept().await?;
                let server = server.clone();
                tokio::spawn(async move {
                    let _ = server.serve(stream).await;
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

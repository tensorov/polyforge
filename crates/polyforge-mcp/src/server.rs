//! PolyForge MCP server core.
//!
//! The server is stateless per call: every tool opens the ledger at
//! `ledger_path` fresh, so concurrent clients never share a mutable handle
//! and the append-only merkle chain stays the single source of truth.

use std::path::PathBuf;

use polyforge_core::evidence::EvidenceState;
use polyforge_core::gate::evaluate_complete;
use polyforge_core::ledger::{EvidenceEntry as LedgerEntry, Ledger, LedgerError};
use polyforge_toolrunner::runner::lookup;
use polyforge_toolrunner::verify::verify_and_append;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ErrorCode, ErrorData, ListToolsResult,
    PaginatedRequestParams, ServerInfo, Tool,
};
use rmcp::schemars::JsonSchema;
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{tool, tool_router, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

/// The PolyForge MCP server. Binds to a single ledger path.
#[derive(Debug, Clone)]
pub struct PolyForgeServer {
    ledger_path: PathBuf,
}

impl PolyForgeServer {
    /// Create a server bound to `ledger_path`, creating the parent directory
    /// so the ledger can be appended on first use.
    pub fn new(ledger_path: impl Into<PathBuf>) -> Result<Self, LedgerError> {
        let path = ledger_path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| LedgerError::Io(e.to_string()))?;
            }
        }
        Ok(Self { ledger_path: path })
    }
}

/// Token-gated wrapper around [`PolyForgeServer`].
///
/// When a token is set, every `tools/call` must carry `arguments._pf_token`
/// equal to the expected value; otherwise the server answers with MCP error
/// `-32001` (server error range `-32000..-32099`). `tools/list`, `ping` and
/// the rest of the protocol stay open: they leak nothing.
#[derive(Debug, Clone)]
pub struct TokenGatedServer {
    inner: PolyForgeServer,
    token: Option<String>,
}

impl TokenGatedServer {
    /// Wrap `inner`; `token` gates `tools/call` when `Some`.
    pub fn new(inner: PolyForgeServer, token: Option<String>) -> Self {
        Self { inner, token }
    }
}

impl ServerHandler for TokenGatedServer {
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if let Some(expected) = &self.token {
            let provided = request
                .arguments
                .as_ref()
                .and_then(|args| args.get("_pf_token"))
                .and_then(|value| value.as_str());
            if provided != Some(expected.as_str()) {
                return Err(ErrorData::new(ErrorCode(-32001), "invalid _pf_token", None));
            }
        }
        self.inner.call_tool(request, context).await
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.inner.list_tools(request, context).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.inner.get_tool(name)
    }

    fn get_info(&self) -> ServerInfo {
        self.inner.get_info()
    }
}

/// Timestamp datum (supplied by the caller, never injected by the ledger).
fn now_ts() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_state(s: &str) -> Result<EvidenceState, ErrorData> {
    match s {
        "ModelClaimed" => Ok(EvidenceState::ModelClaimed),
        "Verified" => Ok(EvidenceState::Verified),
        "Validated" => Ok(EvidenceState::Validated),
        "Refuted" => Ok(EvidenceState::Refuted),
        other => Err(ErrorData::invalid_params(
            format!(
                "unknown evidence state {other:?} (expected ModelClaimed|Verified|Validated|Refuted)"
            ),
            None,
        )),
    }
}

// ---------------------------------------------------------------------------
// Tool parameter / result types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct EvidenceAppendParams {
    /// Must be exactly "ModelClaim". Other kinds are rejected.
    pub kind: String,
    /// Arbitrary claim payload (data only, never interpreted by the server).
    pub payload: String,
    pub task_id: String,
    pub commit_sha: String,
    pub diff_hash: String,
    /// Optional eval experiment id (record-only, never enforced).
    #[serde(default)]
    pub experiment_id: Option<String>,
    /// Optional model fingerprint (record-only, never enforced).
    #[serde(default)]
    pub model_fingerprint: Option<String>,
    /// Optional eval run id (record-only, never enforced).
    #[serde(default)]
    pub run_id: Option<String>,
    /// Optional budget datum (record-only, never enforced).
    #[serde(default)]
    pub budget: Option<String>,
    /// Optional eval metadata blob (record-only, never enforced).
    #[serde(default)]
    pub eval_metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EvidenceAppendResult {
    pub entry_id: u64,
    pub kind: String,
    pub state: String,
    pub task_id: String,
    pub commit_sha: String,
    pub diff_hash: String,
    pub hash: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct EvidenceVerifyParams {
    pub task_id: String,
    pub claim_id: u64,
    /// Allowlist tool name, e.g. "cargo --version".
    pub tool_name: String,
    /// Extra arguments appended to the tool's fixed arg vector.
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct EvidenceVerifyResult {
    pub entry_id: u64,
    pub kind: String,
    pub state: String,
    pub task_id: String,
    pub commit_sha: String,
    pub diff_hash: String,
    pub command: String,
    pub exit_code: i32,
    pub stdout_hash: String,
    pub hash: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct GateEvaluateParams {
    pub task_id: String,
    /// Required final states, e.g. ["Verified"].
    #[serde(default)]
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GateEvaluateResult {
    pub task_id: String,
    pub passed: bool,
    pub claimed: u64,
    pub verified: u64,
    pub validated: u64,
    pub missing: Vec<String>,
    pub chain_tail_hash: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
pub struct GateReportParams {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GateReportEntry {
    pub seq: u64,
    pub kind: String,
    pub state: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GateReportResult {
    pub task_id: String,
    pub tail_hash: String,
    pub passed: bool,
    pub bundle_sha256: String,
    pub entries: Vec<GateReportEntry>,
}

// ---------------------------------------------------------------------------
// Tool router
// ---------------------------------------------------------------------------

#[tool_router(server_handler)]
impl PolyForgeServer {
    /// Append a ModelClaim evidence entry to the ledger. Only `kind=ModelClaim`
    /// is accepted; `ToolAttestation`/`Validation` are rejected because a model
    /// cannot self-verify.
    #[tool(
        name = "evidence_append",
        description = "Append a ModelClaim evidence entry to the ledger. kind must be 'ModelClaim'; ToolAttestation/Validation are rejected (models cannot self-verify)."
    )]
    async fn evidence_append(
        &self,
        Parameters(arguments): Parameters<EvidenceAppendParams>,
    ) -> Result<Json<EvidenceAppendResult>, ErrorData> {
        if arguments.kind.trim() != "ModelClaim" {
            return Err(ErrorData::invalid_params(
                format!(
                    "evidence_append only accepts kind=ModelClaim, got {:?}",
                    arguments.kind
                ),
                None,
            ));
        }
        let mut claim = polyforge_core::evidence::EvidenceEntry::new_claim(
            arguments.task_id.clone(),
            arguments.commit_sha.clone(),
            arguments.diff_hash.clone(),
            now_ts(),
        );
        claim.experiment_id = arguments.experiment_id;
        claim.model_fingerprint = arguments.model_fingerprint;
        claim.run_id = arguments.run_id;
        claim.budget = arguments.budget;
        claim.eval_metadata = arguments.eval_metadata;
        let mut ledger_entry = claim.to_ledger_entry();
        // Store the caller's payload verbatim (data only, never interpreted).
        ledger_entry.payload["payload"] = json!(arguments.payload);

        let mut ledger = Ledger::new(&self.ledger_path);
        let entry_id = ledger
            .append(ledger_entry)
            .map_err(|e| ErrorData::internal_error(format!("ledger append failed: {e:?}"), None))?;
        let entries = ledger
            .iter_entries()
            .map_err(|e| ErrorData::internal_error(format!("ledger read failed: {e:?}"), None))?;
        let appended = entries
            .iter()
            .find(|e| e.seq == entry_id)
            .ok_or_else(|| ErrorData::internal_error("appended entry not found", None))?;
        Ok(Json(EvidenceAppendResult {
            entry_id,
            kind: appended.kind.clone(),
            state: appended.payload["state"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            task_id: appended.payload["task_id"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            commit_sha: appended.payload["commit_sha"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            diff_hash: appended.payload["diff_hash"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            hash: appended.hash.clone(),
        }))
    }

    /// Verify a claim by running an allowlisted tool. The tool is resolved
    /// through the allowlist; arbitrary binaries are never executed.
    #[tool(
        name = "evidence_verify",
        description = "Verify a ModelClaim by running an allowlisted tool. The tool is resolved via the allowlist; arbitrary binaries are never executed."
    )]
    async fn evidence_verify(
        &self,
        Parameters(arguments): Parameters<EvidenceVerifyParams>,
    ) -> Result<Json<EvidenceVerifyResult>, ErrorData> {
        let tool = lookup(&arguments.tool_name).ok_or_else(|| {
            ErrorData::invalid_params(
                format!("tool {:?} is not on the allowlist", arguments.tool_name),
                None,
            )
        })?;
        let mut ledger = Ledger::new(&self.ledger_path);
        let verified = verify_and_append(
            &mut ledger,
            &arguments.task_id,
            arguments.claim_id,
            &tool,
            &arguments.args,
        )
        .map_err(|e| ErrorData::internal_error(format!("verify failed: {e:?}"), None))?;
        let entries = ledger
            .iter_entries()
            .map_err(|e| ErrorData::internal_error(format!("ledger read failed: {e:?}"), None))?;
        let last = entries
            .last()
            .ok_or_else(|| ErrorData::internal_error("ledger empty after verify", None))?;
        Ok(Json(EvidenceVerifyResult {
            entry_id: last.seq,
            kind: "ToolAttestation".to_string(),
            state: "Verified".to_string(),
            task_id: verified.task_id,
            commit_sha: verified.commit_sha,
            diff_hash: verified.diff_hash,
            command: verified.command,
            exit_code: verified.exit_code,
            stdout_hash: verified.stdout_hash,
            hash: last.hash.clone(),
        }))
    }

    /// Evaluate a stage gate for a task.
    #[tool(
        name = "gate_evaluate",
        description = "Evaluate a stage gate for a task against required evidence states."
    )]
    async fn gate_evaluate(
        &self,
        Parameters(arguments): Parameters<GateEvaluateParams>,
    ) -> Result<Json<GateEvaluateResult>, ErrorData> {
        let required: Vec<EvidenceState> = arguments
            .required
            .iter()
            .map(|s| parse_state(s))
            .collect::<Result<_, _>>()?;
        let ledger = Ledger::new(&self.ledger_path);
        let eval = evaluate_complete(&ledger, &arguments.task_id, &required)
            .map_err(|e| ErrorData::internal_error(format!("gate evaluate failed: {e:?}"), None))?;
        Ok(Json(GateEvaluateResult {
            task_id: eval.task_id,
            passed: eval.passed,
            claimed: eval.counts.claimed,
            verified: eval.counts.verified,
            validated: eval.counts.validated,
            missing: eval.missing,
            chain_tail_hash: eval.chain_tail_hash,
        }))
    }

    /// Read-only bundle snapshot for a task: chain tail hash, gate pass
    /// status, and a SHA-256 over the task's canonical ledger entries.
    #[tool(
        name = "gate_report",
        description = "Read-only bundle snapshot for a task: tail hash, gate pass status, and bundle SHA-256."
    )]
    async fn gate_report(
        &self,
        Parameters(arguments): Parameters<GateReportParams>,
    ) -> Result<Json<GateReportResult>, ErrorData> {
        let ledger = Ledger::new(&self.ledger_path);
        let entries = ledger
            .iter_entries()
            .map_err(|e| ErrorData::internal_error(format!("ledger read failed: {e:?}"), None))?;
        let chain = ledger
            .verify_chain()
            .map_err(|e| ErrorData::internal_error(format!("chain verify failed: {e:?}"), None))?;

        let task_entries: Vec<&LedgerEntry> = entries
            .iter()
            .filter(|e| e.payload["task_id"].as_str() == Some(arguments.task_id.as_str()))
            .collect();

        let mut hasher = Sha256::new();
        for e in &task_entries {
            let canonical = serde_json::to_string(e).unwrap_or_default();
            hasher.update(canonical.as_bytes());
            hasher.update(b"\n");
        }
        let bundle_sha256 = hex(&hasher.finalize());

        let passed = evaluate_complete(&ledger, &arguments.task_id, &[EvidenceState::Verified])
            .map(|e| e.passed)
            .unwrap_or(false);

        Ok(Json(GateReportResult {
            task_id: arguments.task_id,
            tail_hash: chain.head_hash,
            passed,
            bundle_sha256,
            entries: task_entries
                .iter()
                .map(|e| GateReportEntry {
                    seq: e.seq,
                    kind: e.kind.clone(),
                    state: e.payload["state"].as_str().unwrap_or_default().to_string(),
                    hash: e.hash.clone(),
                })
                .collect(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_accepts_all_four_states() {
        assert_eq!(
            parse_state("ModelClaimed").unwrap(),
            EvidenceState::ModelClaimed
        );
        assert_eq!(parse_state("Verified").unwrap(), EvidenceState::Verified);
        assert_eq!(parse_state("Validated").unwrap(), EvidenceState::Validated);
        assert_eq!(parse_state("Refuted").unwrap(), EvidenceState::Refuted);
    }

    #[test]
    fn parse_state_rejects_unknown_state() {
        assert!(parse_state("SomethingElse").is_err());
    }

    /// Unique temp ledger path for one unit test.
    fn unit_ledger(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "polyforge-mcp-unit-{tag}-{}.jsonl",
            std::process::id()
        ))
    }

    /// Mutation guard: get_tool must resolve a registered tool by its exact
    /// name and return the tool whose name matches the lookup key.
    #[test]
    fn get_tool_resolves_registered_tool_by_name() {
        let server = PolyForgeServer::new(unit_ledger("get-tool-some")).expect("server");
        let tool = ServerHandler::get_tool(&server, "evidence_append")
            .expect("evidence_append must be a registered tool");
        assert_eq!(tool.name.to_string(), "evidence_append");
    }

    /// Mutation guard: get_tool must answer None for an unknown name.
    #[test]
    fn get_tool_returns_none_for_unknown_name() {
        let server = PolyForgeServer::new(unit_ledger("get-tool-none")).expect("server");
        assert!(
            ServerHandler::get_tool(&server, "definitely-not-a-tool").is_none(),
            "unknown tool name must resolve to None"
        );
    }

    /// Mutation guard: get_info must carry a concrete implementation version,
    /// never a default stub. The version comes from rmcp's build environment
    /// (Implementation::from_build_env), so it is rmcp's package version.
    #[test]
    fn get_info_pins_concrete_version() {
        let server = PolyForgeServer::new(unit_ledger("get-info")).expect("server");
        let info = ServerHandler::get_info(&server);
        assert!(
            !info.server_info.name.is_empty(),
            "implementation name must be non-empty"
        );
        assert_eq!(
            info.server_info.version, "3.1.1",
            "version must equal the concrete value produced by the impl"
        );
    }

    /// Mutation guard: the token-gated wrapper must delegate get_tool to the
    /// inner server unchanged when no token is set, resolving a registered
    /// tool by its exact name.
    #[test]
    fn gated_get_tool_delegates_registered_tool() {
        let server = TokenGatedServer::new(
            PolyForgeServer::new(unit_ledger("tag")).expect("server"),
            None,
        );
        let tool = ServerHandler::get_tool(&server, "evidence_append")
            .expect("evidence_append must resolve through the wrapper");
        assert_eq!(tool.name.to_string(), "evidence_append");
    }

    /// Mutation guard: the token-gated wrapper must delegate get_tool None
    /// answers for unknown names instead of fabricating a tool.
    #[test]
    fn gated_get_tool_delegates_unknown_name() {
        let server = TokenGatedServer::new(
            PolyForgeServer::new(unit_ledger("tag")).expect("server"),
            None,
        );
        assert!(
            ServerHandler::get_tool(&server, "definitely-not-a-tool").is_none(),
            "unknown tool name must stay None through the wrapper"
        );
    }

    /// Mutation guard: the token-gated wrapper must delegate get_info to the
    /// inner server so protocol metadata survives wrapping. Version alone
    /// cannot distinguish a Default::default() body because rmcp's
    /// ServerInfo::default() embeds Implementation::from_build_env();
    /// capabilities.tools is the distinguishing field.
    #[test]
    fn gated_get_info_delegates_version() {
        let server = TokenGatedServer::new(
            PolyForgeServer::new(unit_ledger("tag")).expect("server"),
            None,
        );
        let info = ServerHandler::get_info(&server);
        assert_eq!(
            info.server_info.version, "3.1.1",
            "wrapper must forward the concrete implementation version"
        );
        // Distinguishing field: rmcp's ServerInfo::default() carries no tool
        // capabilities, while the real server registers its tools.
        assert!(
            info.capabilities.tools.is_some(),
            "wrapper must forward tool capabilities"
        );
    }

    /// Mutation guard: now_ts must produce a millisecond Unix timestamp
    /// string, not seconds or a placeholder.
    #[test]
    fn now_ts_is_millisecond_unix_timestamp() {
        let ts = super::now_ts();
        let parsed = ts.parse::<u64>().expect("millisecond unix timestamp");
        assert!(
            parsed > 1_700_000_000_000,
            "timestamp {parsed} must exceed the millisecond epoch floor"
        );
    }

    /// Mutation guard: hex must lowercase-encode every byte with two digits
    /// and yield an empty string for empty input.
    #[test]
    fn hex_encodes_bytes_lowercase_and_empty_input() {
        assert_eq!(super::hex(&[0xDE, 0xAD]), "dead");
        assert_eq!(super::hex(&[]), "");
    }
}

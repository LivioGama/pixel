//! MCP server over the official Rust SDK (`rmcp`), stdio transport.
//!
//! Each tool is a thin wrapper over `crate::query` — the same functions the
//! CLI calls — routed through one shared `call_tool`, so the MCP
//! `structuredContent` is the same `serde_json` serialization the CLI's
//! `--json` flag emits.

use std::sync::Mutex;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::query;
use crate::store::Store;

/// Execute one tool call against the shared query layer. Returns the
/// structured result value, or a user-facing error string. THE single
/// dispatch point both the rmcp tool methods and the equivalence tests use.
pub fn call_tool(store: &Store, name: &str, args: &Value) -> Result<Value, String> {
    fn to_value<T: serde::Serialize>(
        v: Result<T, crate::store::StoreError>,
    ) -> Result<Value, String> {
        v.map_err(|e| e.to_string())
            .and_then(|ok| serde_json::to_value(ok).map_err(|e| e.to_string()))
    }
    match name {
        "errors_since" => {
            let cursor = args
                .get("cursor")
                .and_then(Value::as_i64)
                .ok_or("missing integer argument: cursor")?;
            to_value(query::since(store, cursor))
        }
        "error_show" => {
            let id = args
                .get("id")
                .and_then(Value::as_i64)
                .ok_or("missing integer argument: id")?;
            match query::show(store, id).map_err(|e| e.to_string())? {
                Some(result) => serde_json::to_value(result).map_err(|e| e.to_string()),
                None => Err(format!("no error with id {id}")),
            }
        }
        "errors_query" => {
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or("missing string argument: text")?;
            to_value(query::search(store, text, 20))
        }
        "hmr_status" => {
            let file = args.get("file").and_then(Value::as_str);
            to_value(query::hmr(store, file))
        }
        "env_fingerprint" => {
            let diff = args.get("diff").and_then(Value::as_bool).unwrap_or(false);
            to_value(query::env(store, diff))
        }
        other => Err(format!("unknown tool {other:?}")),
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ErrorsSinceParams {
    /// Last seen error id; 0 for everything.
    pub cursor: i64,
}

#[derive(Deserialize, JsonSchema)]
pub struct ErrorShowParams {
    /// Error id from errors_since or a listing.
    pub id: i64,
}

#[derive(Deserialize, JsonSchema)]
pub struct ErrorsQueryParams {
    /// Search text.
    pub text: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct HmrStatusParams {
    /// Only hmr updates touching this file path fragment.
    pub file: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct EnvFingerprintParams {
    /// Include the field-level diff against the previous run.
    pub diff: Option<bool>,
}

/// The sniper MCP server: five read-only tools over one shared query layer.
pub struct SniperServer {
    store: Mutex<Store>,
    tool_router: ToolRouter<Self>,
}

impl SniperServer {
    pub fn new(store: Store) -> Self {
        Self {
            store: Mutex::new(store),
            tool_router: Self::tool_router(),
        }
    }

    /// Tool names as served (rmcp lists alphabetically).
    pub fn tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn dispatch(&self, name: &str, args: Value) -> CallToolResult {
        let store = match self.store.lock() {
            Ok(store) => store,
            Err(poisoned) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "store lock poisoned: {poisoned}"
                ))]);
            }
        };
        match call_tool(&store, name, &args) {
            Ok(value) => CallToolResult::structured(value),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        }
    }
}

#[tool_router]
impl SniperServer {
    #[tool(
        description = "New error records past a cursor (the id printed by every listing). One call replaces dev-log reading, console polling, and test-log grepping after an edit or failed run."
    )]
    fn errors_since(&self, Parameters(p): Parameters<ErrorsSinceParams>) -> CallToolResult {
        self.dispatch("errors_since", json!({"cursor": p.cursor}))
    }

    #[tool(
        description = "Full detail for one error id: frames with package provenance, captured values, run fingerprint, and lifecycle events within ±30s."
    )]
    fn error_show(&self, Parameters(p): Parameters<ErrorShowParams>) -> CallToolResult {
        self.dispatch("error_show", json!({"id": p.id}))
    }

    #[tool(description = "Substring search over stored errors (message, kind, stack, frames).")]
    fn errors_query(&self, Parameters(p): Parameters<ErrorsQueryParams>) -> CallToolResult {
        self.dispatch("errors_query", json!({"text": p.text}))
    }

    #[tool(
        description = "\"Did my edit land?\" — recent HMR/reload/dep-optimize lifecycle events, optionally filtered to one file."
    )]
    fn hmr_status(&self, Parameters(p): Parameters<HmrStatusParams>) -> CallToolResult {
        self.dispatch("hmr_status", json!({"file": p.file}))
    }

    #[tool(
        description = "Latest dev-server run fingerprint (git head, lockfile hash, vite dep hash); diff=true compares against the previous run — names the 'bun install under a stale server' class."
    )]
    fn env_fingerprint(&self, Parameters(p): Parameters<EnvFingerprintParams>) -> CallToolResult {
        self.dispatch("env_fingerprint", json!({"diff": p.diff}))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SniperServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "pixel-session",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "One-look error capture for this repository. After any edit or \
                 failed run, call errors_since with your last cursor instead of \
                 reading dev logs, polling the console, or grepping test output.",
            )
    }
}

/// Blocking stdio entrypoint: owns the store, runs a current-thread tokio
/// runtime around rmcp's stdio transport.
pub fn run(store: Store) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    runtime.block_on(async {
        let service = SniperServer::new(store)
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|e| format!("mcp serve: {e}"))?;
        service
            .waiting()
            .await
            .map_err(|e| format!("mcp wait: {e}"))?;
        Ok(())
    })
}

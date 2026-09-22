//! `pixel mcp` — the repository index over MCP stdio.
//!
//! One integration for every MCP-capable agent instead of a per-harness
//! wrapper: the same daemon ops the CLI runs, exposed as read-only tools.
//! Each call goes through `crate::execute`, so answers carry the daemon's
//! freshness and epistemics; the server itself holds no index state.

use std::path::PathBuf;

use pixel_daemon::api::Request;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Regex pattern to find in file contents.
    pub pattern: String,
    /// Max hits (default: the daemon's own bound).
    pub limit: Option<usize>,
    /// Ranked code-mode search instead of path order.
    pub scope: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ImpactParams {
    /// Symbol name or uid.
    pub symbol: String,
    /// `upstream` (callers) or `downstream` (callees); default upstream.
    pub direction: Option<String>,
    /// Traversal depth bound.
    pub depth: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
pub struct UsesParams {
    /// Symbol name or uid.
    pub symbol: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct EvaluateParams {
    /// Source symbol: uid or unambiguous name.
    pub from: String,
    /// Target symbol: uid or unambiguous name.
    pub to: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ContextParams {
    /// Symbol uid or name to pack context around.
    pub uid: String,
    /// Token budget for the packed context.
    pub budget_tokens: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ResolveParams {
    /// Concept or phrase to resolve to symbols.
    pub phrase: String,
    /// Max results.
    pub limit: Option<usize>,
}

/// The pixel MCP server: read-only tools over the repo's daemon ops.
pub struct PixelServer {
    root: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl PixelServer {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            tool_router: Self::tool_router(),
        }
    }

    /// Tool names as served (rmcp lists alphabetically).
    #[cfg(test)]
    pub fn tool_names() -> Vec<String> {
        Self::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn call(&self, op: Request) -> CallToolResult {
        match crate::execute(&self.root, op, false) {
            Ok(value) => CallToolResult::structured(value),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        }
    }
}

#[tool_router]
impl PixelServer {
    #[tool(
        description = "Index + graph freshness status for this repository: what is indexed, how old, and whether the daemon is warm."
    )]
    fn status(&self) -> CallToolResult {
        self.call(Request::Status {})
    }

    #[tool(
        description = "Regex search over indexed file contents — the answer comes with paths and lines, not file dumps. scope=\"code\" reranks by file-level signals."
    )]
    fn search(&self, Parameters(p): Parameters<SearchParams>) -> CallToolResult {
        self.call(Request::Search {
            pattern: p.pattern,
            json: true,
            limit: p.limit,
            offset: None,
            paths: None,
            scope: p.scope,
        })
    }

    #[tool(description = "Resolve a concept or phrase to the symbols that implement it.")]
    fn resolve(&self, Parameters(p): Parameters<ResolveParams>) -> CallToolResult {
        self.call(Request::Resolve {
            phrase: p.phrase,
            limit: p.limit,
        })
    }

    #[tool(
        description = "Blast radius of a symbol: who calls it (upstream) or what it calls (downstream), with the bounded answer's epistemics."
    )]
    fn impact(&self, Parameters(p): Parameters<ImpactParams>) -> CallToolResult {
        self.call(Request::Impact {
            uid_or_name: p.symbol,
            direction: p.direction.unwrap_or_else(|| "upstream".to_string()),
            depth: p.depth,
        })
    }

    #[tool(description = "Direct callers of a symbol.")]
    fn callers(&self, Parameters(p): Parameters<UsesParams>) -> CallToolResult {
        self.call(Request::Uses {
            uid_or_name: p.symbol,
            role: "callers".to_string(),
            offset: None,
        })
    }

    #[tool(description = "Direct callees of a symbol.")]
    fn callees(&self, Parameters(p): Parameters<UsesParams>) -> CallToolResult {
        self.call(Request::Uses {
            uid_or_name: p.symbol,
            role: "callees".to_string(),
            offset: None,
        })
    }

    #[tool(
        description = "Does a call-graph path exist between two symbols? Answers established / absent_in_snapshot / unknown with a witness or the typed reason — never a guess."
    )]
    fn evaluate(&self, Parameters(p): Parameters<EvaluateParams>) -> CallToolResult {
        self.call(Request::Evaluate {
            from: p.from,
            to: p.to,
            traversal: None,
            tiers: None,
            max_depth: None,
            time_budget_ms: None,
            scope: None,
            at_snapshot: false,
        })
    }

    #[tool(
        description = "The context around one symbol at a bounded token cost — signature, body, and the neighbors that matter, instead of whole files."
    )]
    fn context(&self, Parameters(p): Parameters<ContextParams>) -> CallToolResult {
        self.call(Request::Context {
            uid: p.uid,
            budget_tokens: p.budget_tokens,
        })
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for PixelServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("pixel", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Deterministic answers about this repository's code index: \
                 search, resolve, impact, callers/callees, evaluate, context. \
                 Prefer these over reading files — they are bounded and carry \
                 evidence.",
            )
    }
}

/// Blocking stdio entrypoint over a current-thread tokio runtime, same
/// shape as the sniper MCP server.
#[cfg_attr(test, mutants::skip)] // serving stdio blocks forever; the tool surface is what tests pin
pub fn run(root: PathBuf) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    runtime.block_on(async {
        let service = PixelServer::new(root)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_documented_tool_set() {
        let mut names = PixelServer::tool_names();
        names.sort();
        assert_eq!(
            names,
            vec![
                "callees", "callers", "context", "evaluate", "impact", "resolve", "search",
                "status",
            ]
        );
    }

    #[test]
    fn a_tool_error_becomes_a_structured_failure_not_a_panic() {
        let server = PixelServer::new(std::env::temp_dir().join("px-mcp-no-such-root"));
        let result = server.call(Request::Status {});
        assert!(result.is_error.unwrap_or(false));
    }

    /// Every tool must produce a real CallToolResult — a `Default::default()`
    /// body answers success-with-no-content, which is worse than an error.
    /// On a root with no daemon each tool must surface the failure.
    #[test]
    fn every_tool_reports_the_error_instead_of_a_default() {
        // A path that does not exist: root discovery fails before any
        // daemon or service work, so every tool surfaces the error fast.
        // SAFETY: process-wide env mutation, but the test restores it before
        // returning and no assertion depends on a concurrent read.
        unsafe {
            std::env::set_var("PIXEL_DAEMON_AUTO_START", "0");
        }
        let root = std::env::temp_dir().join(format!("px-mcp-dead-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let server = PixelServer::new(root.clone());
        let results = [
            server.status(),
            server.search(Parameters(SearchParams {
                pattern: "x".to_string(),
                limit: None,
                scope: None,
            })),
            server.resolve(Parameters(ResolveParams {
                phrase: "x".to_string(),
                limit: None,
            })),
            server.impact(Parameters(ImpactParams {
                symbol: "x".to_string(),
                direction: None,
                depth: None,
            })),
            server.callers(Parameters(UsesParams {
                symbol: "x".to_string(),
            })),
            server.callees(Parameters(UsesParams {
                symbol: "x".to_string(),
            })),
            server.evaluate(Parameters(EvaluateParams {
                from: "a".to_string(),
                to: "b".to_string(),
            })),
            server.context(Parameters(ContextParams {
                uid: "x".to_string(),
                budget_tokens: None,
            })),
        ];
        for result in results {
            assert!(
                result.is_error.unwrap_or(false),
                "a dead root must surface an error, not a default answer"
            );
        }
        // SAFETY: restoring the pre-test environment (see above).
        unsafe {
            std::env::remove_var("PIXEL_DAEMON_AUTO_START");
        }
    }

    /// get_info carries the server's identity and instructions — a default
    /// would hand clients an anonymous, instructionless server.
    #[test]
    fn get_info_names_the_server_and_its_contract() {
        let server = PixelServer::new(PathBuf::from("."));
        let info = server.get_info();
        assert_eq!(info.server_info.name, "pixel");
        assert!(
            info.instructions
                .as_deref()
                .is_some_and(|i| i.contains("Deterministic")),
            "{:?}",
            info.instructions
        );
    }
}

//! Versioned task contracts, factual observations and runner-owned receipts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result, digest};

pub const SCHEMA_VERSION: u8 = 2;
pub const POLICY_VERSION: &str = "task-gates-v1";
pub const MAX_CORRECTIONS: u32 = 3;
pub const MAX_REPEATED_STATE: u32 = 2;

fn yes() -> bool {
    true
}
fn contract_version() -> u8 {
    1
}
fn timeout() -> u64 {
    300_000
}
fn cwd() -> String {
    ".".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub id: String,
    pub argv: Vec<String>,
    #[serde(default = "cwd")]
    pub cwd: String,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "yes")]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub checks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskContract {
    #[serde(default = "contract_version")]
    pub version: u8,
    pub objective: String,
    #[serde(default)]
    pub checks: Vec<Check>,
    #[serde(default)]
    pub criteria: Vec<Criterion>,
    #[serde(default)]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Required full checks explicitly covering unavailable structural evidence.
    #[serde(default)]
    pub conservative_checks: Vec<String>,
    #[serde(default = "yes")]
    pub require_preparation: bool,
    #[serde(default = "yes")]
    pub require_review: bool,
    /// Child executable name/path to its required SHA-256 content digest.
    #[serde(default)]
    pub toolchain: BTreeMap<String, String>,
}

impl Default for TaskContract {
    fn default() -> Self {
        Self {
            version: 1,
            objective: String::new(),
            checks: Vec::new(),
            criteria: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            conservative_checks: Vec::new(),
            require_preparation: true,
            require_review: true,
            toolchain: BTreeMap::new(),
        }
    }
}

impl TaskContract {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 || self.objective.trim().is_empty() || self.objective.len() > 16_384 {
            return Err(Error::Invalid("contract version/objective".into()));
        }
        if self.checks.len() > 128
            || self.criteria.len() > 128
            || self.inputs.len() > 256
            || self.outputs.len() > 256
        {
            return Err(Error::Invalid(
                "contract exceeds bounded item limits".into(),
            ));
        }
        let mut checks = BTreeSet::new();
        for check in &self.checks {
            valid_id(&check.id)?;
            if !checks.insert(&check.id)
                || check.argv.is_empty()
                || check.argv[0].is_empty()
                || check.argv.iter().any(|arg| arg.contains('\0'))
                || check.timeout_ms == 0
                || check.timeout_ms > 86_400_000
            {
                return Err(Error::Invalid(format!(
                    "invalid or duplicate check {}",
                    check.id
                )));
            }
            relative_path(&check.cwd, true)?;
        }
        let mut criteria = BTreeSet::new();
        for criterion in &self.criteria {
            valid_id(&criterion.id)?;
            if !criteria.insert(&criterion.id)
                || criterion.description.trim().is_empty()
                || criterion.checks.iter().any(|id| !checks.contains(id))
            {
                return Err(Error::Invalid(format!(
                    "invalid criterion {}",
                    criterion.id
                )));
            }
        }
        for input in &self.inputs {
            relative_path(input, false)?;
        }
        if self.conservative_checks.iter().any(|id| {
            !self
                .checks
                .iter()
                .any(|check| &check.id == id && check.required)
        }) {
            return Err(Error::Invalid(
                "conservative checks must name required checks".into(),
            ));
        }
        for output in &self.outputs {
            relative_path(output, false)?;
            if self.inputs.iter().any(|input| overlaps(input, output)) {
                return Err(Error::Invalid(format!(
                    "output overlaps declared input: {output}"
                )));
            }
        }
        for (program, identity) in &self.toolchain {
            if program.is_empty()
                || program.contains('\0')
                || identity.len() != 64
                || !identity.bytes().all(|byte| byte.is_ascii_hexdigit())
                || identity.bytes().any(|byte| byte.is_ascii_uppercase())
            {
                return Err(Error::Invalid(
                    "invalid toolchain executable identity".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn id(&self) -> Result<String> {
        digest(self)
    }

    /// Changes preserving every old obligation may be made without human waiver.
    pub fn preserves(&self, prior: &Self) -> bool {
        let unconfigured = prior.checks.is_empty()
            && prior
                .criteria
                .iter()
                .all(|criterion| criterion.checks.is_empty());
        self.objective == prior.objective
            && (!prior.require_preparation || self.require_preparation)
            && (!prior.require_review || self.require_review)
            && prior
                .checks
                .iter()
                .filter(|check| {
                    check.required
                        || prior
                            .criteria
                            .iter()
                            .any(|criterion| criterion.checks.contains(&check.id))
                })
                .all(|old| self.checks.contains(old))
            && prior.criteria.iter().all(|old| {
                self.criteria.iter().any(|new| {
                    new.id == old.id
                        && new.description == old.description
                        && old.checks.iter().all(|id| new.checks.contains(id))
                })
            })
            && prior.inputs.iter().all(|old| self.inputs.contains(old))
            && prior
                .conservative_checks
                .iter()
                .all(|old| self.conservative_checks.contains(old))
            && (unconfigured
                || self
                    .outputs
                    .iter()
                    .all(|output| prior.outputs.contains(output)))
            && prior
                .toolchain
                .iter()
                .all(|(program, identity)| self.toolchain.get(program) == Some(identity))
    }
}

pub fn valid_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_:.".contains(&byte))
        || value == "."
        || value == ".."
    {
        return Err(Error::Invalid("invalid identifier".into()));
    }
    Ok(())
}

pub fn relative_path(value: &str, allow_dot: bool) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || value.contains('\0')
        || (!allow_dot && value == ".")
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(Error::Invalid(format!(
            "expected repository-relative path: {value}"
        )));
    }
    Ok(())
}

pub fn overlaps(a: &str, b: &str) -> bool {
    Path::new(a).starts_with(b) || Path::new(b).starts_with(a)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Contracted,
    Prepared,
    Editing,
    Verifying,
    Reviewing,
    Complete,
    Blocked,
    Cancelled,
    Incomplete,
}

impl Phase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    Edit,
    Finish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    Investigate,
    Configure,
    Prepare,
    Edit,
    Verify,
    Review,
    Finish,
    Recover,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub allowed: bool,
    pub reasons: Vec<String>,
    pub eligible_routes: Vec<Route>,
    pub phase: Phase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFile {
    pub path: String,
    pub sha256: String,
    pub mode: u32,
    pub symlink: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSnapshot {
    pub content_id: String,
    pub root: String,
    pub head: Option<String>,
    pub index_id: String,
    #[serde(default)]
    pub refs_id: String,
    pub captured_ms: u64,
    pub files: Vec<SourceFile>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    Scope,
    Impact,
    TestSuggestions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub kind: ObservationKind,
    pub source_id: String,
    pub complete: bool,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    Passed,
    Failed,
    TimedOut,
    SourceChanged,
    Interrupted,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationReceipt {
    pub run_id: String,
    pub check_id: String,
    pub source_id: String,
    pub contract_id: String,
    pub check_digest: String,
    pub outcome: CheckOutcome,
    pub exit_code: Option<i32>,
    pub started_ms: u64,
    pub finished_ms: u64,
    pub duration_ms: u64,
    pub stdout_sha256: String,
    pub stderr_sha256: String,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub execution_root: String,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReceipt {
    pub source_id: String,
    pub contract_id: String,
    pub passed: bool,
    pub findings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CorrectionBudget {
    pub continuations: u32,
    pub same_state_repeats: u32,
    pub last_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRun {
    pub run_id: String,
    pub request_id: String,
    pub source_id: String,
    pub owner_pid: u32,
    pub started_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryEvent {
    pub id: String,
    pub kind: String,
    pub attempt_id: String,
    pub occurred_ms: u64,
    #[serde(default)]
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub version: u8,
    pub task_id: String,
    pub provider: String,
    pub session_id: Option<String>,
    pub attempt_id: String,
    pub revision: u64,
    pub phase: Phase,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub contract: TaskContract,
    pub source: Option<SourceSnapshot>,
    pub observations: Vec<Observation>,
    pub receipts: Vec<VerificationReceipt>,
    pub review: Option<ReviewReceipt>,
    pub claims: Vec<String>,
    pub budget: CorrectionBudget,
    pub running: Option<VerificationRun>,
    pub legacy_status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    SetContract {
        contract: TaskContract,
        human_authorized: bool,
    },
    Prepare {
        observations: Vec<Observation>,
    },
    Edited,
    Review {
        passed: bool,
        findings: Vec<String>,
    },
    Finish,
    Cancel,
    Correction {
        state_key: String,
    },
    Observe {
        event: TrajectoryEvent,
    },
    Claim {
        text: String,
    },
    Recover,
}

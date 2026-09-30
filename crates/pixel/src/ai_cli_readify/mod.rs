//! `pixel ai-cli-readify` — honest provider readiness for the four agent
//! CLIs, and the config rewrite that points them at the provider that
//! answered.
//!
//! The shape of a run is two waves, not a queue:
//!
//! 1. the three providers are probed concurrently, and the winner is the
//!    highest-priority one that answered — so the wall clock is the slowest
//!    provider, not their sum;
//! 2. each selected agent is probed in its own thread, and a lane that meets
//!    a classified failure (a throttled or exhausted provider) re-loops onto
//!    the next provider in priority order rather than reporting the whole
//!    agent unready.
//!
//! A third, optional step follows: under `--approve`, the agent's own startup
//! gate is cleared for the one workspace named on the command line. It is
//! off by default and reported either way, because a trust write outlives the
//! run and which folders get one is the user's decision, not this command's.
//!
//! A fourth, also optional, is the one step that leaves this process: under
//! `--authenticate`, a Claude lane stuck on an auth wall is handed to the
//! installed browser flow ([`auth`]), which signs the CLI in. Off by default,
//! because it drives a real browser against the user's real profile, and
//! firing only for the one failure a login clears.
//!
//! The honesty rules live in the sibling modules: [`provider`] classifies a
//! failure instead of collapsing it into "failed", [`agents`] refuses to
//! call a probe ready on an exit code alone, [`terminal`] reports a
//! startup prompt rather than guessing a keystroke at it, and [`approve`]
//! writes a trust decision only where it was asked to and says so when it
//! was not.

pub(crate) mod agents;
pub(crate) mod approve;
pub(crate) mod auth;
pub(crate) mod config;
pub(crate) mod provider;
pub(crate) mod rpc;
pub(crate) mod terminal;

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::Duration,
};

use serde::Serialize;

pub(crate) use agents::{Agent, AgentFlag};

use agents::{
    AgentProbe, CLAUDE_CREDENTIAL_ENVS, PROBE_PROMPT, any_env_set, blocked_by, devin_auth_argv,
    parse_devin_auth, parse_probe, parse_probe_with, probe_argv,
};
use approve::Approval;
use auth::{AuthChain, CLAUDE_AUTH_FLOW};
use config::{
    antigravity_settings, claude_settings, codex_config, devin_config, write_antigravity,
    write_claude, write_codex,
};
use provider::{ProbeFailure, ProbeOutcome, Provider, probe};
use terminal::{DEFAULT_LAUNCH_BUDGET, ScriptTerminal, drive_until_settled};

#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) apply: bool,
    pub(crate) timeout: Duration,
    pub(crate) agents: Vec<Agent>,
    /// Answer the startup prompts whose key the prompt itself documents.
    /// Off by default: the reference fails closed, and a guessed keystroke on
    /// a trust dialog is the user's decision, not this command's.
    pub(crate) answer_prompts: bool,
    /// Clear each agent's own startup gate — Codex's workspace and hook
    /// trust, Claude's onboarding and trust dialog. Off by default for the
    /// same reason and a stronger one: those writes survive the run.
    pub(crate) approve: bool,
    /// The folder the trust writes are about. One workspace, named, never a
    /// walk up the tree: a trust level granted to `/work` covers every folder
    /// below it, so a run that guessed would grant more than it was asked to.
    pub(crate) workspace: PathBuf,
    /// Hand a Claude lane stuck on an auth wall to the installed
    /// `claude-code-auth-flow`, which drives a real browser against the
    /// user's real profile. Off by default, and it fires only for the one
    /// failure a login clears.
    pub(crate) authenticate: bool,
    /// The account the flow should pick — the email `claude auth login` is
    /// handed and the flow's own account shortcut. Only consulted when the
    /// chain runs.
    pub(crate) account: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProviderRow {
    pub(crate) provider: &'static str,
    pub(crate) priority: usize,
    pub(crate) ready: bool,
    /// The reply, or the classified reason there was none.
    pub(crate) detail: String,
    pub(crate) failure: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AgentRow {
    pub(crate) agent: &'static str,
    /// The provider that answered, when one did.
    pub(crate) provider: Option<&'static str>,
    pub(crate) ready: bool,
    /// Providers the lane walked before settling, in order.
    pub(crate) tried: Vec<&'static str>,
    pub(crate) detail: String,
    /// A startup prompt or an auth wall that stopped the probe.
    pub(crate) blocker: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub(crate) providers: Vec<ProviderRow>,
    /// The provider the agents were pointed at, if any answered.
    pub(crate) selected: Option<&'static str>,
    pub(crate) agents: Vec<AgentRow>,
    /// Config files rewritten by `--apply`.
    pub(crate) applied: Vec<String>,
    /// What `--apply` would have done, when it was not given.
    pub(crate) pending: Vec<String>,
    /// Configs verified but never written, with the file named so the
    /// absence of a rewrite reads as a decision rather than a gap.
    pub(crate) verified_only: Vec<String>,
    /// What `--approve` cleared, one row per agent.
    ///
    /// `None` is "not asked" and `Some(vec![])` is "asked, nothing to do" —
    /// a distinction the report needs, because a run that never attempted an
    /// approval and a run that found nothing to approve look identical in an
    /// empty list.
    pub(crate) approvals: Option<Vec<Approval>>,
    /// What `--authenticate` did about an auth-walled Claude lane.
    ///
    /// `None` is "not asked" and `Some(NotNeeded)` is "asked, nothing to do"
    /// — the same distinction `approvals` draws, for the same reason: a run
    /// that never touched a browser and a run that found no auth wall must
    /// not read alike.
    pub(crate) auth_chain: Option<AuthChain>,
}

impl Report {
    /// True when every requested agent completed a real round trip.
    ///
    /// The one line of this report that is a judgement rather than an
    /// observation, and it is deliberately the strictest one available: a run
    /// where every agent but one answered is not a readiness run, and saying
    /// otherwise is exactly the narrowing the honest probe exists to refuse.
    pub(crate) fn all_ready(&self) -> bool {
        !self.agents.is_empty() && self.agents.iter().all(|row| row.ready)
    }
}

/// Read one provider's key from the environment, if it is there and not
/// blank. A missing key is its own outcome, not a transport error: it is the
/// one failure a user can fix without touching the provider.
fn key_for(provider: Provider) -> Option<String> {
    match std::env::var(provider.key_env()) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

/// One provider's outcome, given an already-resolved key. `key` is a
/// parameter rather than a lookup so the missing-key branch is a decision
/// this function makes, and a test can hand it `None` without touching the
/// process environment.
fn outcome_for_key<F>(
    provider: Provider,
    key: Option<String>,
    timeout: Duration,
    probe_one: &F,
) -> ProbeOutcome
where
    F: Fn(Provider, Duration) -> ProbeOutcome,
{
    match key {
        None => ProbeOutcome::failed(provider, ProbeFailure::MissingKey, String::new()),
        Some(_) => probe_one(provider, timeout),
    }
}

/// Probe every provider concurrently and return the rows in priority order
/// plus the winner.
///
/// `probe_one` is the seam a test drives with a fake: the production call is
/// [`provider::probe`], and a test substitutes a closure that answers per
/// provider without a socket.
fn probe_providers_with<F>(timeout: Duration, probe_one: F) -> (Vec<ProviderRow>, Option<Provider>)
where
    F: Fn(Provider, Duration) -> ProbeOutcome + Sync,
{
    let rows: Mutex<Vec<ProviderRow>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for provider in Provider::PROBE_ORDER {
            let rows = &rows;
            let probe_one = &probe_one;
            scope.spawn(move || {
                let outcome = outcome_for_key(provider, key_for(provider), timeout, probe_one);
                let row = ProviderRow {
                    provider: provider.name(),
                    priority: provider.rank(),
                    ready: outcome.ready,
                    detail: outcome.detail,
                    failure: outcome.failure.as_ref().map(ProbeFailure::label),
                };
                rows.lock().expect("provider rows mutex").push(row);
            });
        }
    });
    let mut rows = rows.into_inner().expect("provider rows mutex");
    rows.sort_by_key(|row| row.priority);
    let winner = rows
        .iter()
        .find(|row| row.ready)
        .map(|row| Provider::PROBE_ORDER[row.priority]);
    (rows, winner)
}

/// The production provider probe: send one real completion through whichever
/// key the environment holds.
fn probe_providers(timeout: Duration) -> (Vec<ProviderRow>, Option<Provider>) {
    probe_providers_with(timeout, |provider, timeout| match key_for(provider) {
        Some(key) => probe(provider, provider.base_url(), &key, timeout),
        // `outcome_for_key` already turned a missing key into its own
        // outcome, so this arm is unreachable through `probe_providers_with`
        // and exists only to keep the closure total.
        None => ProbeOutcome::failed(provider, ProbeFailure::MissingKey, String::new()),
    })
}

/// Run one command, capture its output, and bound it by `timeout`.
///
/// The two pipes are drained by their own threads because a child that fills
/// one while the parent is still polling `try_wait` would deadlock, and a
/// probe that hangs is exactly the failure this has to survive.
#[cfg_attr(test, mutants::skip)] // a spawn-and-drain adapter; its callers are tested against a fake
fn run_capture(argv: &[String], timeout: Duration) -> io::Result<(bool, String)> {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("ANTHROPIC_API_KEY")
        .spawn()?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut stderr = child.stderr.take().expect("stderr was piped");
    let (out, err) = std::thread::scope(|scope| {
        let out = scope.spawn(move || {
            let mut buffer = String::new();
            let _ = stdout.read_to_string(&mut buffer);
            buffer
        });
        let err = scope.spawn(move || {
            let mut buffer = String::new();
            let _ = stderr.read_to_string(&mut buffer);
            buffer
        });
        (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        )
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let success = status.is_some_and(|s| s.success());
    Ok((success, format!("{out}\n{err}")))
}

/// Probe one agent against one provider.
///
/// The plain capture runs first because it is cheap and is what a
/// non-interactive probe wants. Only when it fails does the pty driver run,
/// because a startup prompt is the one failure the capture cannot explain:
/// it is what turns "probe reached no model" into "workspace trust
/// confirmation required".
fn probe_agent_with<F>(
    agent: Agent,
    provider: Option<Provider>,
    timeout: Duration,
    answer_prompts: bool,
    run: &F,
    diagnose: &dyn Fn(Agent, bool) -> Option<&'static str>,
) -> AgentProbe
where
    F: Fn(&[String], Duration) -> io::Result<(bool, String)>,
{
    // Devin's readiness is its own auth status: it is verified, never
    // repointed at a provider, so a provider round trip would say nothing
    // about it.
    let argv = if agent == Agent::Devin {
        devin_auth_argv()
    } else {
        let Some(provider) = provider else {
            return blocked_by(
                "no provider answered",
                "every provider in priority order failed or had no key",
            );
        };
        let _ = provider;
        probe_argv(
            agent,
            PROBE_PROMPT,
            agents::DEFAULT_CLAUDE_MODEL,
            &format!("{}s", timeout.as_secs()),
            "/dev/null",
            true,
        )
    };
    match run(&argv, timeout) {
        Ok((success, output)) => {
            // Two lanes need a reader other than the reply-token one. Devin's
            // command is its auth check and carries no reply token at all,
            // and Claude's CLI answers `authentication_failed` both for a
            // credential that was rejected and for one that was never
            // configured — so its lane is the one that reads the environment
            // to choose the wording.
            let probe_result = match agent {
                Agent::Devin => parse_devin_auth(&output, success),
                Agent::Claude => {
                    parse_probe_with(&output, success, any_env_set(&CLAUDE_CREDENTIAL_ENVS))
                }
                _ => parse_probe(&output, success),
            };
            if probe_result.ready {
                return probe_result;
            }
            // The capture failed and has no explanation: ask the pty whether
            // a startup prompt is why.
            if let Some(blocker) = diagnose(agent, answer_prompts) {
                return blocked_by(blocker, &probe_result.detail);
            }
            probe_result
        }
        Err(e) => AgentProbe::failed(format!("could not start {}: {e}", agent.executable())),
    }
}

/// Drive an agent's launch under a pty and report the startup prompt that
/// stopped it, if one did.
#[cfg_attr(test, mutants::skip)] // the pty adapter; `next_step` above carries the policy
fn diagnose_startup(agent: Agent, answer_prompts: bool, timeout: Duration) -> Option<&'static str> {
    let export = std::env::temp_dir().join(format!("pixel-readify-{}.json", agent.name()));
    let argv = probe_argv(
        agent,
        PROBE_PROMPT,
        agents::DEFAULT_CLAUDE_MODEL,
        &format!("{}s", timeout.as_secs()),
        &export.to_string_lossy(),
        true,
    );
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // Interior mutability, not a shared `&mut`: the driver calls pump and
    // send one after the other, but two closures cannot hold a single
    // mutable borrow, and the terminal is only ever touched from this thread.
    let terminal = std::cell::RefCell::new(ScriptTerminal::spawn(&argv, &cwd, &[]).ok()?);
    let outcome = drive_until_settled(
        || {
            terminal
                .borrow_mut()
                .pump()
                .map(str::to_string)
                .unwrap_or_default()
        },
        |keys| {
            let _ = terminal.borrow_mut().send(keys);
        },
        answer_prompts,
        DEFAULT_LAUNCH_BUDGET.min(timeout),
        &mut std::time::Instant::now,
    );
    // A prompt still on screen after the driver ran is the most specific
    // thing that can be said, whether the driver answered it and the keys
    // did not work or it never had an answer to send. The driver's own
    // fallback ("the launch budget ran out") is the vaguer of the two, so it
    // loses to a named prompt.
    if let Some(rule) = terminal::detect_prompt(&outcome.screen) {
        return Some(rule.label);
    }
    outcome.blocker
}

/// The providers a lane may walk: the ones that answered, in priority order,
/// so the winner is first.
///
/// A provider that just failed is deliberately absent. Walking it again
/// would spend another timeout to learn what the probe two seconds ago
/// already established, and with no provider ready the list is empty — which
/// is what keeps an agent from being launched at all when there is nowhere
/// for it to go.
fn lane_order(rows: &[ProviderRow]) -> Vec<Provider> {
    rows.iter()
        .filter(|row| row.ready)
        .map(|row| Provider::PROBE_ORDER[row.priority])
        .collect()
}

/// Probe the selected agents concurrently, each lane re-looping through the
/// provider order on a classified failure.
fn probe_lane<F, D>(
    agents: &[Agent],
    providers: &[Provider],
    timeout: Duration,
    answer_prompts: bool,
    run: &F,
    diagnose: &D,
) -> Vec<AgentRow>
where
    F: Fn(&[String], Duration) -> io::Result<(bool, String)> + Sync,
    D: Fn(Agent, bool, Duration) -> Option<&'static str> + Sync,
{
    let rows: Mutex<Vec<AgentRow>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for agent in agents {
            let agent = *agent;
            let rows = &rows;
            scope.spawn(move || {
                let mut tried: Vec<&'static str> = Vec::new();
                let mut last: Option<AgentProbe> = None;
                let candidates: Vec<Option<Provider>> =
                    if agent == Agent::Devin || providers.is_empty() {
                        vec![None]
                    } else {
                        providers.iter().copied().map(Some).collect()
                    };
                for provider in candidates {
                    // Devin's own auth is the only probe it gets.
                    let provider_name = if agent == Agent::Devin {
                        None
                    } else {
                        provider.map(Provider::name)
                    };
                    let probe_result = probe_agent_with(
                        agent,
                        provider,
                        timeout,
                        answer_prompts,
                        run,
                        &|agent, answer| diagnose(agent, answer, timeout),
                    );
                    if let Some(name) = provider_name {
                        tried.push(name);
                    }
                    if probe_result.ready {
                        rows.lock().expect("agent rows mutex").push(AgentRow {
                            agent: agent.name(),
                            provider: provider_name,
                            ready: true,
                            tried,
                            detail: probe_result.detail,
                            blocker: None,
                        });
                        return;
                    }
                    // A prompt or an auth wall is not the provider's fault:
                    // walking the rest of the order would spend two more
                    // minutes to learn the same thing.
                    if probe_result.blocker.is_some() {
                        rows.lock().expect("agent rows mutex").push(AgentRow {
                            agent: agent.name(),
                            provider: provider_name,
                            ready: false,
                            tried,
                            detail: probe_result.detail,
                            blocker: probe_result.blocker,
                        });
                        return;
                    }
                    last = Some(probe_result);
                }
                let detail = last.map_or_else(
                    || "no provider to probe".to_string(),
                    |probe_result| probe_result.detail,
                );
                rows.lock().expect("agent rows mutex").push(AgentRow {
                    agent: agent.name(),
                    provider: None,
                    ready: false,
                    tried,
                    detail,
                    blocker: None,
                });
            });
        }
    });
    let mut rows = rows.into_inner().expect("agent rows mutex");
    rows.sort_by_key(|row| {
        Agent::ALL
            .iter()
            .position(|agent| agent.name() == row.agent)
            .unwrap_or(usize::MAX)
    });
    rows
}

/// Rewrite each requested agent's config to point at `provider`, returning
/// what was written.
///
/// Only the agents the run asked for are touched, and only those whose config
/// this command owns: `rewrites_config` is the single place that decides the
/// second half, so adding a fifth agent cannot quietly start writing to a
/// file nobody sanctioned.
fn write_configs(home: &std::path::Path, provider: Provider, agents: &[Agent]) -> Vec<String> {
    let mut written = Vec::new();
    for agent in agents {
        if !agent.rewrites_config() {
            continue;
        }
        let result = match agent {
            Agent::Codex => write_codex(&codex_config(home), provider),
            Agent::Claude => write_claude(&claude_settings(home), provider),
            Agent::Antigravity => write_antigravity(&antigravity_settings(home), provider),
            // Unreachable past the `rewrites_config` guard above. Spelled out
            // so a new agent cannot arrive without a writer beside it.
            Agent::Devin => continue,
        };
        if let Ok(line) = result {
            written.push(line);
        }
    }
    written
}

pub(crate) fn run(options: &Options) -> Result<Report, String> {
    let (providers, winner) = probe_providers(options.timeout);
    let agents = if options.agents.is_empty() {
        Agent::ALL.to_vec()
    } else {
        options.agents.clone()
    };
    let order = lane_order(&providers);
    let rows = probe_lane(
        &agents,
        &order,
        options.timeout,
        options.answer_prompts,
        &run_capture,
        &diagnose_startup,
    );
    let auth_chain = auth_chain_for(options, &rows, &order);
    let home = home_dir()?;
    let (applied, pending) = match (winner, options.apply) {
        (Some(provider), true) => (write_configs(&home, provider, &agents), Vec::new()),
        (Some(provider), false) => (Vec::new(), planned_configs(&home, provider, &agents)),
        (None, _) => (Vec::new(), Vec::new()),
    };
    let approvals = options
        .approve
        .then(|| clear_gates(&home, &agents, &options.workspace, options.timeout));
    Ok(Report {
        providers,
        selected: winner.map(Provider::name),
        agents: rows,
        applied,
        pending,
        verified_only: verified_only(&home, &agents),
        approvals,
        auth_chain,
    })
}

/// The auth chain `--authenticate` asked for, or `None` when it was not
/// given.
///
/// Glue: it either replays a real browser flow or builds the row that says
/// there was nothing to do. Both decisions are made and tested in [`auth`] —
/// [`auth::should_authenticate`] chooses the branch, and
/// [`AuthChain::not_needed`] fills the other one — so the spawn itself,
/// reached only under `--authenticate`, is the one thing with no test under
/// it.
#[cfg_attr(test, mutants::skip)] // runs a real login; the decision it branches on is tested
fn auth_chain_for(options: &Options, rows: &[AgentRow], order: &[Provider]) -> Option<AuthChain> {
    options.authenticate.then(|| {
        if auth::should_authenticate(rows, options.authenticate) {
            auth::authenticate(options.account.as_deref(), || {
                probe_lane(
                    &[Agent::Claude],
                    order,
                    options.timeout,
                    options.answer_prompts,
                    &run_capture,
                    &diagnose_startup,
                )
                .first()
                .is_some_and(|row| row.ready)
            })
        } else {
            AuthChain::not_needed(rows)
        }
    })
}

/// Clear each agent's startup gate, one agent per thread.
///
/// Concurrency is safe here in a way it is not for the provider probes: each
/// agent's approval writes a file no other agent's does — Codex's
/// `config.toml` and its hook state, Claude's `~/.claude.json` — and each
/// app-server session is this process's own child. The rows come back in
/// `Agent::ALL` order regardless of which finished first, so the report does
/// not reflect a race the user cannot see.
fn clear_gates(
    home: &Path,
    agents: &[Agent],
    workspace: &Path,
    timeout: Duration,
) -> Vec<Approval> {
    let resolved: Vec<Approval> = std::thread::scope(|scope| {
        let handles: Vec<_> = agents
            .iter()
            .map(|agent| scope.spawn(move || approve::approve(home, *agent, workspace, timeout)))
            .collect();
        handles
            .into_iter()
            .map(|handle| match handle.join() {
                Ok(approval) => approval,
                // A panic inside an approval is not the run's verdict on the
                // agent, and swallowing it would report a gate as cleared by
                // a thread that died before it wrote anything.
                Err(_) => Approval {
                    agent: "unknown",
                    approved: false,
                    detail:
                        "the approval thread panicked before it reported; nothing here was written"
                            .to_string(),
                },
            })
            .collect()
    });
    let mut rows = resolved;
    rows.sort_by_key(|row| {
        Agent::ALL
            .iter()
            .position(|agent| agent.name() == row.agent)
            .unwrap_or(Agent::ALL.len())
    });
    rows
}

/// What `--apply` would write, without writing it. Silence here for Devin is
/// deliberate and not an oversight, which is why it is a separate line
/// ([`Report::verified_only`]) rather than a missing one.
fn planned_configs(home: &std::path::Path, provider: Provider, agents: &[Agent]) -> Vec<String> {
    let mut planned = Vec::new();
    if agents.contains(&Agent::Codex) {
        planned.push(format!(
            "{} via {}",
            codex_config(home).display(),
            provider.name()
        ));
    }
    if agents.contains(&Agent::Claude) {
        planned.push(format!(
            "{} via {}",
            claude_settings(home).display(),
            provider.name()
        ));
    }
    if agents.contains(&Agent::Antigravity) {
        planned.push(format!(
            "{} via {}",
            antigravity_settings(home).display(),
            provider.name()
        ));
    }
    planned
}

/// The agents whose config this command verifies but never writes. Devin's
/// model is a Devin-side identifier no provider list here has an equivalent
/// for, so repointing it would mean guessing; it is reported instead.
fn verified_only(home: &std::path::Path, agents: &[Agent]) -> Vec<String> {
    agents
        .iter()
        .filter(|agent| !agent.rewrites_config())
        .map(|agent| {
            format!(
                "{}: {} verified, never rewritten",
                agent.name(),
                devin_config(home).display()
            )
        })
        .collect()
}

/// The human-readable report. Every line is a fact the run established: a
/// provider's classified condition, the provider each agent settled on, and
/// One provider row's text after the state column.
///
/// The label reaches the reader exactly once. A classified provider error
/// already opens with its own label in `detail` — that is what the probe
/// records, status and all — so printing the label ahead of it read as
/// `rate limited (429): rate limited (429) (HTTP 429): …`. A missing key is
/// the opposite case: there is no provider text to carry, so the label is
/// the whole message.
fn provider_line(row: &ProviderRow) -> String {
    match row.failure {
        Some(failure) if row.detail.starts_with(failure) => row.detail.clone(),
        Some(failure) if row.detail.trim().is_empty() => failure.to_string(),
        Some(failure) => format!("{failure}: {}", row.detail),
        None => row.detail.clone(),
    }
}

/// the exact file `--apply` wrote or would write. Nothing here says "ready"
/// on behalf of a probe that did not complete one.
pub(crate) fn print_report(report: &Report) {
    println!("providers");
    for row in &report.providers {
        let state = if row.ready { "ready" } else { "not ready" };
        println!("  {:<9} {state:<10} {}", row.provider, provider_line(row));
    }
    match report.selected {
        Some(provider) => println!("\nselected: {provider}"),
        None => println!("\nselected: none — every provider failed, so nothing was rewritten"),
    }
    println!("\nagents");
    for row in &report.agents {
        let state = if row.ready { "ready" } else { "not ready" };
        let via = row.provider.unwrap_or("-");
        println!(
            "  {:<12} {state:<10} via {via:<9} {}",
            row.agent, row.detail
        );
        if let Some(blocker) = row.blocker {
            println!("  {:<12} blocked: {blocker}", "");
        }
        if row.tried.len() > 1 {
            println!("  {:<12} tried: {}", "", row.tried.join(" -> "));
        }
    }
    if !report.applied.is_empty() {
        println!("\napplied");
        for line in &report.applied {
            println!("  {line}");
        }
    }
    if !report.pending.is_empty() {
        println!("\nwould apply (re-run with --apply)");
        for line in &report.pending {
            println!("  {line}");
        }
    }
    if !report.verified_only.is_empty() {
        println!("\nverified only");
        for line in &report.verified_only {
            println!("  {line}");
        }
    }
    match &report.approvals {
        None => println!(
            "\napprovals: not attempted — re-run with --approve to clear each agent's own startup gate"
        ),
        Some(rows) => {
            println!("\napprovals");
            for row in rows {
                let state = if row.approved { "cleared" } else { "left" };
                println!("  {:<12} {state:<8} {}", row.agent, row.detail);
            }
        }
    }
    match &report.auth_chain {
        None => println!(
            "\nauthentication: not attempted — re-run with --authenticate to clear a Claude auth wall through {CLAUDE_AUTH_FLOW}"
        ),
        Some(chain) => {
            println!("\nauthentication ({})", chain.flow);
            for step in &chain.steps {
                println!("  {step}");
            }
            let state = if chain.ready_after {
                "ready"
            } else {
                "not ready"
            };
            println!("  {} — claude afterwards: {state}", chain.outcome.label());
        }
    }
    let overall = if report.all_ready() {
        "ready"
    } else {
        "not ready"
    };
    println!("\noverall: {overall}");
}

fn home_dir() -> Result<std::path::PathBuf, String> {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "HOME is not set, so there is no agent config to read".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider::ProbeOutcome;

    fn ready(provider: Provider) -> ProbeOutcome {
        ProbeOutcome::ready(provider, "READY".to_string())
    }

    fn throttled(provider: Provider) -> ProbeOutcome {
        ProbeOutcome::failed(provider, ProbeFailure::RateLimited, "429".to_string())
    }

    #[test]
    fn every_requested_agent_completes_a_real_round_trip() {
        let (rows, winner) = probe_providers_with(Duration::from_secs(1), |provider, _| {
            if provider == Provider::Groq {
                ready(provider)
            } else {
                throttled(provider)
            }
        });
        assert_eq!(winner, Some(Provider::Groq));
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|row| row.ready), "{rows:?}");
        assert!(!rows[0].ready, "the top-priority provider was throttled");
        assert_eq!(rows[0].failure, Some("rate limited (429)"));
        assert_eq!(rows[0].provider, "ollama", "rows are in priority order");
    }

    #[test]
    fn the_winner_is_the_highest_priority_provider_that_answered() {
        let (_, winner) = probe_providers_with(Duration::from_secs(1), |provider, _| {
            if provider == Provider::Cerebras {
                ready(provider)
            } else {
                throttled(provider)
            }
        });
        assert_eq!(winner, Some(Provider::Cerebras));
    }

    #[test]
    fn ollama_beats_groq_when_both_answer() {
        let (_, winner) =
            probe_providers_with(Duration::from_secs(1), |provider, _| ready(provider));
        assert_eq!(winner, Some(Provider::Ollama));
    }

    #[test]
    fn a_provider_with_no_key_is_not_ready_and_says_which() {
        // The seam is handed `None` rather than having the environment
        // emptied, so this asserts the branch and not the machine's shell.
        let probed = Mutex::new(0);
        let probe_one = |provider: Provider, _timeout: Duration| {
            *probed.lock().unwrap() += 1;
            ready(provider)
        };
        let outcome = outcome_for_key(Provider::Groq, None, Duration::from_secs(1), &probe_one);
        assert!(!outcome.ready, "{outcome:?}");
        assert_eq!(outcome.failure, Some(ProbeFailure::MissingKey));
        assert_eq!(
            outcome.failure.as_ref().map(ProbeFailure::label),
            Some("no key")
        );
        assert_eq!(*probed.lock().unwrap(), 0, "no request without a key");
    }

    #[test]
    fn a_provider_with_a_key_is_probed() {
        let probe_one = |provider: Provider, _timeout: Duration| ready(provider);
        let outcome = outcome_for_key(
            Provider::Groq,
            Some("k".to_string()),
            Duration::from_secs(1),
            &probe_one,
        );
        assert!(outcome.ready, "{outcome:?}");
        assert_eq!(outcome.failure, None);
    }

    #[test]
    fn no_provider_answering_selects_nothing() {
        let (rows, winner) =
            probe_providers_with(Duration::from_secs(1), |provider, _| throttled(provider));
        assert_eq!(winner, None);
        assert!(rows.iter().all(|row| !row.ready), "{rows:?}");
    }

    /// A row as the probe records one: `detail` opens with the label.
    fn failed_row(detail: &str, failure: &'static str) -> ProviderRow {
        ProviderRow {
            provider: "ollama",
            priority: 0,
            ready: false,
            detail: detail.to_string(),
            failure: Some(failure),
        }
    }

    #[test]
    fn a_classified_failure_is_named_once_in_the_report() {
        // The real line from a throttled Ollama Cloud run: the label was
        // printed ahead of a detail that already began with it.
        let row = failed_row(
            "rate limited (429) (HTTP 429): you have reached your session usage limit",
            "rate limited (429)",
        );
        let line = provider_line(&row);
        assert_eq!(
            line.matches("rate limited (429)").count(),
            1,
            "the label belongs once in the line: {line}"
        );
        assert!(
            line.contains("HTTP 429"),
            "the status has to survive: {line}"
        );
        assert!(
            line.contains("session usage limit"),
            "the provider's own words have to survive: {line}"
        );
    }

    #[test]
    fn a_missing_key_still_reads_as_its_own_reason() {
        // `MissingKey` is the one failure with no provider text to carry, so
        // the label is the whole message and must not print an empty tail.
        let row = failed_row("", "no key");
        assert_eq!(provider_line(&row), "no key");
    }

    #[test]
    fn a_ready_row_prints_its_reply_alone() {
        let row = ProviderRow {
            provider: "groq",
            priority: 1,
            ready: true,
            detail: "READY".to_string(),
            failure: None,
        };
        assert_eq!(provider_line(&row), "READY");
    }

    /// The rows a lane order is built from, ready or not.
    fn rows(ready: &[Provider]) -> Vec<ProviderRow> {
        Provider::PROBE_ORDER
            .iter()
            .map(|provider| ProviderRow {
                provider: provider.name(),
                priority: provider.rank(),
                ready: ready.contains(provider),
                detail: String::new(),
                failure: None,
            })
            .collect()
    }

    #[test]
    fn a_lane_walks_only_the_providers_that_answered() {
        // Cerebras is ready and Groq is not, so the lane is Cerebras alone:
        // re-probing a provider that just failed costs a timeout to learn
        // what the probe already established.
        assert_eq!(
            lane_order(&rows(&[Provider::Cerebras])),
            vec![Provider::Cerebras]
        );
    }

    #[test]
    fn a_lane_walks_the_ready_providers_in_priority_order() {
        assert_eq!(
            lane_order(&rows(&[Provider::Ollama, Provider::Cerebras])),
            vec![Provider::Ollama, Provider::Cerebras],
            "the winner comes first, then the rest by priority"
        );
    }

    #[test]
    fn no_provider_ready_leaves_the_lane_empty() {
        assert_eq!(lane_order(&rows(&[])), Vec::<Provider>::new());
    }

    #[test]
    fn a_throttled_provider_hands_the_lane_to_the_next_one() {
        let attempts = Mutex::new(Vec::new());
        let run = |_argv: &[String], _timeout: Duration| {
            let n = {
                let mut guard = attempts.lock().unwrap();
                guard.push(1);
                guard.len()
            };
            // The first provider a lane walks reports a rate limit; the
            // second answers.
            if n % 2 == 1 {
                Ok((true, r#"{"message":"429 too many requests"}"#.to_string()))
            } else {
                Ok((true, r#"{"text":"READY"}"#.to_string()))
            }
        };
        let rows = probe_lane(
            &[Agent::Codex],
            &[Provider::Ollama, Provider::Groq],
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ready, "{rows:?}");
        assert_eq!(rows[0].provider, Some("groq"));
        assert_eq!(rows[0].tried, vec!["ollama", "groq"]);
    }

    #[test]
    fn a_lane_stops_at_a_blocker_instead_of_walking_every_provider() {
        let run = |_argv: &[String], _timeout: Duration| Ok((false, String::new()));
        let rows = probe_lane(
            &[Agent::Claude],
            &[Provider::Ollama, Provider::Groq, Provider::Cerebras],
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| Some("Workspace trust confirmation required"),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].blocker,
            Some("Workspace trust confirmation required")
        );
        assert_eq!(rows[0].tried, vec!["ollama"], "a blocker ends the lane");
    }

    #[test]
    fn devin_is_probed_through_its_own_auth_and_not_a_provider() {
        let seen = Mutex::new(Vec::new());
        let run = |argv: &[String], _timeout: Duration| {
            seen.lock().unwrap().push(argv.join(" "));
            Ok((true, "Logged in as someone".to_string()))
        };
        let rows = probe_lane(
            &[Agent::Devin],
            &[Provider::Ollama],
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert_eq!(rows[0].provider, None, "{rows:?}");
        assert_eq!(rows[0].tried.len(), 0, "{rows:?}");
        assert_eq!(seen.lock().unwrap().as_slice(), ["devin auth status"]);
        // The assertion this test was missing: it ran Devin's auth command,
        // saw a logged-in line, and still reported the row unready, because
        // the lane parsed its output with the reply-token reader.
        assert!(rows[0].ready, "an authenticated Devin is ready: {rows:?}");
    }

    #[test]
    fn an_agent_with_no_provider_is_blocked_rather_than_run() {
        let ran = Mutex::new(0);
        let run = |_argv: &[String], _timeout: Duration| {
            *ran.lock().unwrap() += 1;
            Ok((true, "READY".to_string()))
        };
        let rows = probe_lane(
            &[Agent::Codex],
            &[],
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert!(!rows[0].ready, "{rows:?}");
        assert_eq!(rows[0].blocker, Some("no provider answered"));
        assert_eq!(*ran.lock().unwrap(), 0, "nothing should have been spawned");
    }

    #[test]
    fn the_agent_order_in_the_report_is_the_fixed_one() {
        let run = |_argv: &[String], _timeout: Duration| Ok((true, "READY".to_string()));
        let rows = probe_lane(
            &[Agent::Devin, Agent::Codex],
            &[Provider::Groq],
            Duration::from_secs(1),
            false,
            &run,
            &|_, _, _| None,
        );
        assert_eq!(rows[0].agent, "codex", "{rows:?}");
        assert_eq!(rows[1].agent, "devin", "{rows:?}");
    }

    #[test]
    fn a_report_with_no_agent_is_not_all_ready() {
        let report = Report {
            providers: Vec::new(),
            selected: None,
            agents: Vec::new(),
            applied: Vec::new(),
            pending: Vec::new(),
            verified_only: Vec::new(),
            approvals: None,
            auth_chain: None,
        };
        assert!(!report.all_ready(), "an empty run proves nothing");
    }

    #[test]
    fn a_report_is_all_ready_only_when_every_agent_is() {
        let row = |ready: bool| AgentRow {
            agent: "codex",
            provider: Some("groq"),
            ready,
            tried: vec!["groq"],
            detail: String::new(),
            blocker: None,
        };
        let mut report = Report {
            providers: Vec::new(),
            selected: Some("groq"),
            agents: vec![row(true)],
            applied: Vec::new(),
            pending: Vec::new(),
            verified_only: Vec::new(),
            approvals: None,
            auth_chain: None,
        };
        assert!(report.all_ready());
        report.agents.push(row(false));
        assert!(!report.all_ready(), "one unready agent is not ready");
    }

    #[test]
    fn each_provider_reads_its_own_key_variable() {
        // The report names the variable a user has to set, so the three must
        // stay distinct and must be the providers' own names.
        assert_eq!(Provider::Ollama.key_env(), "OLLAMA_API_KEY");
        assert_eq!(Provider::Groq.key_env(), "GROQ_API_KEY");
        assert_eq!(Provider::Cerebras.key_env(), "CEREBRAS_API_KEY");
    }
}

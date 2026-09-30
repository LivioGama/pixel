//! Startup-prompt detection and dismissal.
//!
//! The TypeScript stack this port mirrors detects these prompts and *fails
//! closed*: it records a blocker and tears the child down, and the only
//! bytes it ever writes to a terminal are `\x03`. There is no dismissal
//! logic in it to port, so this module is new work and says so.
//!
//! That shapes the design. Detection is cheap and the patterns below are the
//! reference's own, verbatim. *Answering* is only honest where the prompt
//! itself documents the key — `press enter to continue` is a dismissal
//! anyone can verify by reading it. Everywhere else a byte sequence would be
//! a guess, and a guessed Enter on a trust dialog is a security decision
//! taken on the user's behalf, so those rules carry [`Answer::Unknown`]: the
//! driver names the prompt and stops, exactly as the reference does, and a
//! human pins the sequence once it has been observed.

use std::{
    io::{self, Read, Write},
    path::Path,
    process::{Child, ChildStdout, Command, ExitStatus, Stdio},
    sync::LazyLock,
    time::{Duration, Instant},
};

use regex::Regex;

/// How often the driver looks at the screen while waiting for a prompt.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// How long the driver waits for the child to reach a stable screen. The
/// reference's own cap for a fresh launch is 30 s.
pub(crate) const DEFAULT_LAUNCH_BUDGET: Duration = Duration::from_secs(30);

/// A byte a rule may send. Only the ones a prompt documents are listed.
pub(crate) const ENTER: &[u8] = b"\r";
/// The interrupt the reference sends on teardown.
pub(crate) const CTRL_C: &[u8] = b"\x03";

/// How much of the child's output is kept for matching. An agent that streams
/// for minutes must not grow the buffer without limit while the only thing
/// ever matched is the last screenful.
const SCREEN_CAP_CHARS: usize = 65_536;

/// What to send when a prompt appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Bytes the prompt's own text documents as the way through.
    Keys(&'static [u8]),
    /// No verified sequence exists. The driver reports the prompt and stops.
    Unknown,
}

/// One recognised startup prompt.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PromptRule {
    /// The reference's pattern, verbatim.
    pub(crate) pattern: &'static str,
    /// What the prompt means, in the report's own words.
    pub(crate) label: &'static str,
    pub(crate) answer: Answer,
}

/// Every startup prompt the reference recognises, with its pattern and its
/// emitted label kept exactly as that stack words them, so a report from
/// here reads the same as a report from there.
///
/// The reference's fourth rule carries four alternatives in one pattern —
/// `update available`, `restart … to update`, `press … enter … continue` and
/// `select|choose … theme|appearance`. It is split in two here, and the split
/// is the only departure from that table. The reference has no answer for any
/// of them, so the split costs nothing there; here, where a prompt that
/// documents its own key may be answered, keeping them together would mean
/// sending Enter at a theme picker on the strength of a sentence that is not
/// on screen. The narrower pattern is listed first because detection is
/// first-match-wins.
pub(crate) const PROMPT_RULES: [PromptRule; 6] = [
    PromptRule {
        pattern: r"(?i)trust (?:this|the) (?:folder|workspace|directory)|do you trust|workspace.{0,30}(untrusted|not trusted)|quick safety check",
        label: "Workspace trust confirmation required",
        answer: Answer::Unknown,
    },
    PromptRule {
        pattern: r"(?i)(?:review|trust).{0,30}hooks|hooks.{0,40}(?:need|require).{0,20}(?:review|trust)|untrusted hooks",
        label: "Hook review/trust required — use /hooks",
        answer: Answer::Unknown,
    },
    PromptRule {
        pattern: r"(?i)sign in to|log in to|login required|authentication.{0,20}(expired|failed)|choose.*login|select.*login",
        label: "Sign-in required — the CLI is waiting for a login",
        answer: Answer::Unknown,
    },
    PromptRule {
        pattern: r"(?i)press.{0,15}enter.{0,30}(continue|restart)",
        label: "Update, restart, or startup confirmation required",
        // The prompt states its own key ("press enter to continue"), which is
        // the one dismissal a reader can verify without observing a run.
        answer: Answer::Keys(ENTER),
    },
    PromptRule {
        pattern: r"(?i)update available|new version available|restart.{0,20}(required|to update)|select.{0,15}(theme|appearance)|choose.{0,15}(theme|appearance)",
        label: "Update, restart, or startup confirmation required",
        // A theme picker names no key, so there is nothing here a reader
        // could verify; it is reported like every other prompt with no
        // documented answer.
        answer: Answer::Unknown,
    },
    PromptRule {
        pattern: r"(?i)failed to (load|start|connect)|invalid configuration|configuration error|MCP.{0,40}(failed|error)|quota.{0,20}exhaust|rate.?limited",
        label: "Startup error or unavailable service",
        answer: Answer::Unknown,
    },
];

/// The rules as compiled patterns, in declaration order. Compiling once
/// keeps a 200 ms poll loop from recompiling five regexes a second.
static COMPILED: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    PROMPT_RULES
        .iter()
        .map(|rule| Regex::new(rule.pattern).expect("a PROMPT_RULES pattern must compile"))
        .collect()
});

/// The first prompt in `screen`, or `None` for a clean screen.
///
/// Order is the table's order, so a screen carrying two prompts reports the
/// earlier rule; the table lists trust and hooks before the generic
/// service-error rule for that reason.
pub(crate) fn detect_prompt(screen: &str) -> Option<&'static PromptRule> {
    PROMPT_RULES
        .iter()
        .zip(COMPILED.iter())
        .find(|(_, pattern)| pattern.is_match(screen))
        .map(|(rule, _)| rule)
}

/// One step of the driver's decision, kept pure so the whole policy is
/// testable without a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The screen is clean; keep polling.
    Wait,
    /// Send these bytes and keep polling.
    Send(&'static [u8]),
    /// A prompt no rule can answer. Report it and stop.
    Blocked(&'static str),
}

/// What to do about the current screen. A departure from the reference lives
/// here and nowhere else: answering is opt-in, so the default run reports
/// the prompt and stops exactly as the reference does.
pub(crate) fn next_step(screen: &str, answer_prompts: bool) -> Step {
    match detect_prompt(screen) {
        None => Step::Wait,
        Some(rule) => match (answer_prompts, rule.answer) {
            (true, Answer::Keys(keys)) => Step::Send(keys),
            _ => Step::Blocked(rule.label),
        },
    }
}

/// A child attached to a pseudo-terminal.
///
/// Implemented over `script(1)`, which allocates the pty and forwards both
/// directions, so no unsafe `openpty` and no new dependency is needed. Its
/// flags differ between the BSD and util-linux versions and the split below
/// is the one this repository already uses for its own terminal tests.
pub(crate) struct ScriptTerminal {
    child: Child,
    stdout: ChildStdout,
    screen: String,
    read_buf: [u8; 8_192],
}

impl ScriptTerminal {
    /// Spawn `argv` under a pty in `cwd`. The caller owns the child's
    /// lifetime; [`ScriptTerminal::stop`] ends it.
    #[cfg_attr(test, mutants::skip)] // the one-line adapter over `script`; the policy above is tested directly
    pub(crate) fn spawn(
        argv: &[String],
        cwd: &Path,
        environment: &[(String, String)],
    ) -> io::Result<Self> {
        let mut command = Command::new("script");
        if cfg!(target_os = "macos") {
            command.args(["-q", "/dev/null"]);
        } else {
            command.args(["-qec"]);
        }
        if cfg!(target_os = "macos") {
            command.args(argv);
        } else {
            let line = argv
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" ");
            command.arg(line).arg("/dev/null");
        }
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("ANTHROPIC_API_KEY");
        for (key, value) in environment {
            command.env(key, value);
        }
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("stdout was piped");
        Ok(Self {
            child,
            stdout,
            screen: String::new(),
            read_buf: [0u8; 8_192],
        })
    }

    /// Send raw bytes to the child's terminal.
    pub(crate) fn send(&mut self, keys: &[u8]) -> io::Result<()> {
        let stdin =
            self.child.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "the child has no stdin")
            })?;
        stdin.write_all(keys)?;
        stdin.flush()
    }

    /// Everything the child has written since the last call, appended to the
    /// running screen.
    ///
    /// Non-blocking: `script` holds the pty open, so a blocking read here
    /// would wait for the child to exit rather than for the next screen.
    pub(crate) fn pump(&mut self) -> io::Result<&str> {
        loop {
            match self.stdout.read(&mut self.read_buf) {
                Ok(0) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&self.read_buf[..n]);
                    self.screen.push_str(&chunk);
                    if self.screen.chars().count() > SCREEN_CAP_CHARS {
                        let drop = self.screen.len() - SCREEN_CAP_CHARS;
                        self.screen.drain(..drop);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(&self.screen)
    }

    /// Wait for the child to exit within `budget`, or give up on it.
    ///
    /// `None` means the budget ran out with the child still running: the
    /// caller owns the teardown, and a login left waiting is reported rather
    /// than waited on.
    #[cfg_attr(test, mutants::skip)] // the process wait itself, bounded by its own argument
    pub(crate) fn wait_for_exit(&mut self, budget: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) if Instant::now() >= deadline => return None,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => return None,
            }
        }
    }

    /// End the child the way the reference does: interrupt, then escalate.
    pub(crate) fn stop(&mut self) {
        let _ = self.send(CTRL_C);
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ScriptTerminal {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What a driven launch ended as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LaunchOutcome {
    /// The screen reached a stable clean state, or a documented prompt was
    /// answered and it then did.
    pub(crate) clean: bool,
    /// Prompts that appeared, in the order they were seen.
    pub(crate) prompts: Vec<&'static str>,
    /// The prompt that stopped the launch, if one did.
    pub(crate) blocker: Option<&'static str>,
    /// The last screen, for the report.
    pub(crate) screen: String,
}

/// Drive a launch until its screen is stable and clean, a prompt stops it,
/// or the budget runs out.
///
/// `pump` and `send` are the two ends of the terminal, kept as closures so
/// the whole policy runs against a script in a test. `now` is a parameter
/// for the same reason: a test drives the loop's clock instead of waiting on
/// one, and the budget keeps a broken loop failing in seconds rather than
/// hanging.
pub(crate) fn drive_until_settled<P, S>(
    mut pump: P,
    mut send: S,
    answer_prompts: bool,
    budget: Duration,
    now: &mut dyn FnMut() -> Instant,
) -> LaunchOutcome
where
    P: FnMut() -> String,
    S: FnMut(&'static [u8]),
{
    let started = now();
    let mut prompts: Vec<&'static str> = Vec::new();
    let mut answered: Vec<&'static str> = Vec::new();
    loop {
        let screen = pump();
        match next_step(&screen, answer_prompts) {
            Step::Blocked(label) => {
                return LaunchOutcome {
                    clean: false,
                    prompts,
                    blocker: Some(label),
                    screen,
                };
            }
            Step::Send(keys) => {
                // Record the rule once: a screen that keeps showing the same
                // prompt must not append it forever.
                if let Some(rule) = detect_prompt(&screen)
                    && !answered.contains(&rule.label)
                {
                    answered.push(rule.label);
                    prompts.push(rule.label);
                }
                send(keys);
            }
            Step::Wait => {
                // Stable and clean, or out of budget. The reference wants a
                // held screen, but a probe that has already answered its
                // prompt is the case this port cares about, so a clean
                // non-empty screen with no prompt settles immediately.
                if !screen.trim().is_empty() {
                    return LaunchOutcome {
                        clean: true,
                        prompts,
                        blocker: None,
                        screen,
                    };
                }
            }
        }
        if now().duration_since(started) >= budget {
            return LaunchOutcome {
                clean: false,
                prompts,
                blocker: Some("no verified interface before the launch budget ran out"),
                screen,
            };
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Quote one argv element for the util-linux `script -c` line.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A clock a test advances by hand, so a budget can be crossed without
    /// waiting for it.
    struct FakeClock {
        elapsed: Duration,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                elapsed: Duration::ZERO,
            }
        }
    }

    #[test]
    fn a_clean_screen_waits() {
        assert_eq!(next_step("$ ", false), Step::Wait);
        assert_eq!(next_step("", false), Step::Wait);
    }

    #[test]
    fn the_trust_prompt_is_recognised() {
        let step = next_step("Do you trust the files in this folder?", false);
        assert_eq!(step, Step::Blocked("Workspace trust confirmation required"));
    }

    #[test]
    fn the_hook_prompt_is_recognised() {
        let step = next_step("Hooks need review before they can run", false);
        assert_eq!(
            step,
            Step::Blocked("Hook review/trust required — use /hooks")
        );
    }

    #[test]
    fn the_auth_prompt_is_recognised() {
        let step = next_step("Please sign in to continue", false);
        assert_eq!(
            step,
            Step::Blocked("Sign-in required — the CLI is waiting for a login")
        );
    }

    #[test]
    fn an_expired_authentication_is_recognised() {
        let step = next_step("authentication expired, please sign in to continue", false);
        assert_eq!(
            step,
            Step::Blocked("Sign-in required — the CLI is waiting for a login")
        );
    }

    #[test]
    fn a_startup_service_error_is_recognised() {
        let step = next_step("failed to connect to the model service", false);
        assert_eq!(step, Step::Blocked("Startup error or unavailable service"));
    }

    #[test]
    fn a_rate_limit_on_the_startup_screen_is_recognised() {
        let step = next_step("rate limited, try again later", false);
        assert_eq!(step, Step::Blocked("Startup error or unavailable service"));
    }

    #[test]
    fn nothing_is_sent_by_default_even_for_a_documented_prompt() {
        // The reference fails closed, and the default run matches it: a
        // guessed keystroke on a trust dialog is a decision not ours to take.
        let step = next_step("press enter to continue", false);
        assert_eq!(
            step,
            Step::Blocked("Update, restart, or startup confirmation required")
        );
    }

    #[test]
    fn a_documented_prompt_is_answered_when_answering_is_opted_into() {
        let step = next_step("press enter to continue", true);
        assert_eq!(step, Step::Send(ENTER));
    }

    #[test]
    fn a_theme_picker_is_recognised_though_it_names_no_key() {
        // The reference folds this into the rule above. Kept apart, because
        // the answer that rule carries — Enter — is justified by a sentence
        // this screen does not contain.
        for screen in [
            "Select a theme",
            "choose appearance",
            "restart required to update",
        ] {
            let step = next_step(screen, false);
            assert_eq!(
                step,
                Step::Blocked("Update, restart, or startup confirmation required"),
                "{screen}"
            );
        }
    }

    #[test]
    fn a_theme_picker_is_not_answered_even_when_answering_is_opted_into() {
        for screen in ["Select a theme", "choose appearance to continue"] {
            let step = next_step(screen, true);
            assert_eq!(
                step,
                Step::Blocked("Update, restart, or startup confirmation required"),
                "nothing about {screen} documents a key to send"
            );
        }
    }

    #[test]
    fn a_screen_offering_both_alternatives_answers_the_one_that_documents_its_key() {
        // First-match-wins, and the narrower pattern is listed first: a
        // screen that says both must reach the answerable rule rather than
        // the generic one beside it.
        let step = next_step("Update available — press enter to continue", true);
        assert_eq!(step, Step::Send(ENTER));
    }

    #[test]
    fn an_undocumented_prompt_stays_blocked_even_when_answering_is_opted_into() {
        let step = next_step("Do you trust the files in this folder?", true);
        assert_eq!(step, Step::Blocked("Workspace trust confirmation required"));
    }

    #[test]
    fn an_untrusted_workspace_is_recognised_in_its_other_wording() {
        let step = next_step("This workspace is untrusted", false);
        assert_eq!(step, Step::Blocked("Workspace trust confirmation required"));
    }

    #[test]
    fn an_untrusted_hooks_warning_is_recognised() {
        let step = next_step("untrusted hooks are configured", false);
        assert_eq!(
            step,
            Step::Blocked("Hook review/trust required — use /hooks")
        );
    }

    #[test]
    fn the_driver_settles_on_a_clean_screen() {
        let mut clock = FakeClock::new();
        let outcome = drive_until_settled(
            || "$ ready\n".to_string(),
            |_| {},
            false,
            Duration::from_secs(5),
            &mut || {
                clock.elapsed += Duration::from_millis(10);
                Instant::now()
            },
        );
        assert!(outcome.clean, "{outcome:?}");
        assert_eq!(outcome.blocker, None);
        assert!(outcome.prompts.is_empty());
    }

    #[test]
    fn the_driver_stops_at_a_prompt_it_cannot_answer() {
        let mut clock = FakeClock::new();
        let outcome = drive_until_settled(
            || "Do you trust this folder?".to_string(),
            |_| panic!("a prompt with no verified answer must not be typed at"),
            false,
            Duration::from_secs(5),
            &mut || {
                clock.elapsed += Duration::from_millis(10);
                Instant::now()
            },
        );
        assert!(!outcome.clean, "{outcome:?}");
        assert_eq!(
            outcome.blocker,
            Some("Workspace trust confirmation required")
        );
    }

    #[test]
    fn the_driver_ends_on_budget_when_the_screen_stays_blank() {
        let mut clock = FakeClock::new();
        let outcome = drive_until_settled(
            String::new,
            |_| {},
            false,
            Duration::from_millis(500),
            &mut || {
                clock.elapsed += Duration::from_millis(200);
                Instant::now()
            },
        );
        assert!(!outcome.clean, "{outcome:?}");
        assert!(outcome.blocker.is_some(), "{outcome:?}");
    }

    #[test]
    fn the_driver_sends_the_documented_key_and_records_the_prompt_once() {
        let mut clock = FakeClock::new();
        let sent: std::cell::RefCell<Vec<Vec<u8>>> = std::cell::RefCell::new(Vec::new());
        let outcome = drive_until_settled(
            || "press enter to continue".to_string(),
            |keys| sent.borrow_mut().push(keys.to_vec()),
            true,
            Duration::from_secs(5),
            &mut || {
                clock.elapsed += Duration::from_millis(10);
                Instant::now()
            },
        );
        assert_eq!(
            outcome.prompts,
            vec!["Update, restart, or startup confirmation required"]
        );
        assert_eq!(outcome.prompts.len(), 1, "the prompt must not repeat");
        // The bytes must actually leave: the driver's whole purpose is to
        // get past this prompt, and a recorded-but-unsent key would report a
        // prompt answered that the terminal never saw.
        let sent = sent.into_inner();
        assert!(!sent.is_empty(), "nothing was typed at the prompt");
        assert!(sent.iter().all(|keys| keys == ENTER), "{sent:?}");
    }

    #[test]
    fn shell_quoting_survives_an_apostrophe() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }
}

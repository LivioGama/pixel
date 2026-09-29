//! The human-facing exit of an interactive `pixel install`: the summary a
//! person at a terminal reads, with the tour of what to visit next.
//! `--json` and a piped stdout keep the machine-readable report — this
//! banner is never the contract an agent parses.

use crate::install::{CheckStatus, InstallReport};

/// "pixel" in the ANSI Shadow figlet face. Five rows tall and 44 columns
/// wide, so it survives every terminal the install runs in.
const LOGO: &str = "
 ██████╗ ██╗██╗  ██╗███████╗███████╗██╗
 ██╔══██╗██║██║   ██║██╔════╝██╔════╝██║
 ██████╔╝██║██║   ██║█████╗  ███████╗██║
 ██╔═══╝ ██║██║   ██║██╔══╝  ╚════██║██║
 ██║     ██║╚██████╔╝███████╗███████║███████╗
 ╚═╝     ╚═╝ ╚═════╝ ╚══════╝╚══════╝╚══════╝";

/// The homepage titles, quoted as the tribute they are: the site is built
/// like the tool, down to its section headings.
pub const TOUR: &[(&str, &str)] = &[
    (
        "https://pixel-cli.dev",
        "the landing page — \u{201c}Every session starts cold.\u{201d}",
    ),
    ("https://pixel-cli.dev/docs", "the manual, hook by hook"),
    (
        "https://pixel-cli.dev/vs/jev",
        "the classify benchmark, caveats included",
    ),
];

/// Wrap `text` in the SGR `code` when `color` is on; pass it through plain
/// otherwise. [`NO_COLOR`] is resolved by the caller.
fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// The status symbol and SGR code of one install step: ✓ green, • yellow,
/// ✗ red — the same shapes `pixel doctor` renders.
fn symbol(status: CheckStatus) -> (&'static str, &'static str) {
    match status {
        CheckStatus::Green => ("✓", "32"),
        CheckStatus::Yellow => ("•", "33"),
        CheckStatus::Red => ("✗", "31"),
    }
}

/// Render the interactive install exit. `color` gates every SGR sequence;
/// the layout, symbols and links render identically without it.
pub fn render(report: &InstallReport, color: bool) -> String {
    let mut out = String::new();
    for line in LOGO.trim().lines() {
        out.push_str(&paint(color, "1;36", line));
        out.push('\n');
    }
    out.push('\n');
    let InstallReport {
        version: _,
        ok,
        executable_path,
        home: _,
        dry_run,
        steps,
        summary: _,
    } = report;
    let headline = if *dry_run {
        "dry run — nothing was written; every step below is what WOULD happen."
    } else if *ok {
        "installed. Every agent on this machine now starts with the map."
    } else {
        "finished with red steps — they are named below, and re-running is safe."
    };
    out.push_str(&paint(color, "1", headline));
    out.push('\n');
    out.push_str(&paint(color, "2", executable_path));
    out.push_str("\n\n");
    for step in steps {
        let (mark, code) = symbol(step.status);
        out.push_str("  ");
        out.push_str(&paint(color, code, mark));
        out.push(' ');
        out.push_str(&step.summary);
        out.push('\n');
    }
    out.push('\n');
    let crate::install::InstallSummary { green, yellow, red } = report.summary;
    let counts = format!("{green} green · {yellow} yellow · {red} red");
    out.push_str(&paint(color, "1", &counts));
    out.push('\n');
    if *ok && !*dry_run {
        out.push_str("\nNext:\n");
        out.push_str(&paint(
            color,
            "36",
            "  pixel doctor . --fix        verify a repository end to end",
        ));
        out.push('\n');
        out.push_str(&paint(
            color,
            "36",
            "  pixel install --repo .      add project-local enforcement to a clone",
        ));
        out.push('\n');
    }
    out.push_str(&paint(
        color,
        "2",
        "\nMade by hand, down to the landing page:\n",
    ));
    for (url, note) in TOUR {
        out.push_str("  ");
        out.push_str(&paint(color, "36", url));
        out.push_str("  —  ");
        out.push_str(note);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install::{InstallStep, InstallSummary};

    fn report(status: CheckStatus, summary: &str) -> InstallReport {
        InstallReport {
            version: "v1".into(),
            ok: status == CheckStatus::Green,
            executable_path: "/usr/local/bin/pixel".into(),
            home: "/home/example".into(),
            dry_run: false,
            steps: vec![InstallStep {
                id: "agent-prompt".into(),
                status,
                summary: summary.into(),
                detail: None,
            }],
            summary: match status {
                CheckStatus::Green => InstallSummary {
                    green: 1,
                    yellow: 0,
                    red: 0,
                },
                CheckStatus::Yellow => InstallSummary {
                    green: 0,
                    yellow: 1,
                    red: 0,
                },
                CheckStatus::Red => InstallSummary {
                    green: 0,
                    yellow: 0,
                    red: 1,
                },
            },
        }
    }

    #[test]
    fn the_banner_names_every_step_with_its_status_symbol_and_counts() {
        let banner = render(
            &report(CheckStatus::Green, "verified agent-prompt.md"),
            false,
        );
        assert!(banner.contains("✓ verified agent-prompt.md"), "{banner}");
        assert!(banner.contains("1 green · 0 yellow · 0 red"), "{banner}");
        assert!(banner.contains("installed."), "{banner}");
        assert!(banner.contains("/usr/local/bin/pixel"), "{banner}");

        let yellow = render(&report(CheckStatus::Yellow, "legacy wrapper left"), false);
        assert!(yellow.contains("• legacy wrapper left"), "{yellow}");
        assert!(!yellow.contains("installed."), "{yellow}");

        let red = render(&report(CheckStatus::Red, "could not write hooks"), false);
        assert!(red.contains("✗ could not write hooks"), "{red}");
        assert!(red.contains("re-running is safe"), "{red}");
    }

    #[test]
    fn the_banner_carries_the_tour_and_the_next_commands() {
        let banner = render(&report(CheckStatus::Green, "verified"), false);
        for (url, _) in TOUR {
            assert!(banner.contains(url), "{banner}");
        }
        assert!(banner.contains("pixel-cli.dev"), "{banner}");
        assert!(banner.contains("pixel doctor . --fix"), "{banner}");
        assert!(banner.contains("pixel install --repo ."), "{banner}");
        assert!(!banner.contains('"'), "{banner}");
    }

    #[test]
    fn a_dry_run_prints_what_would_happen_and_no_next_commands() {
        let mut dry = report(CheckStatus::Green, "verified agent-prompt.md");
        dry.dry_run = true;
        dry.ok = true;
        let banner = render(&dry, false);
        assert!(banner.contains("dry run — nothing was written"), "{banner}");
        assert!(!banner.contains("Next:"), "{banner}");
        assert!(!banner.contains("installed."), "{banner}");
    }

    #[test]
    fn color_off_never_emits_an_escape_sequence_and_color_on_does() {
        let plain = render(&report(CheckStatus::Green, "verified"), false);
        assert!(!plain.contains('\x1b'), "{plain}");
        let colored = render(&report(CheckStatus::Green, "verified"), true);
        assert!(colored.contains("\x1b[1;36m"), "{colored}");
        assert!(colored.contains("\x1b[32m✓\x1b[0m"), "{colored}");
        assert!(colored.ends_with('\n'), "{colored}");
    }

    #[test]
    fn the_logo_spells_the_name_in_six_rows() {
        let banner = render(&report(CheckStatus::Green, "verified"), false);
        let logo_rows = LOGO.trim().lines().count();
        assert_eq!(logo_rows, 6, "{LOGO}");
        assert!(banner.contains("███████╗"), "{banner}");
    }
}

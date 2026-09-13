//! Exact, deliberately narrow native-search compatibility for hook routing.
//! Unsupported inputs execute the original search; ordinary `pixel search`
//! keeps its richer, bounded interface. Never emit a partial native result.

use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum SearchTool {
    Rg,
    Grep,
}

impl SearchTool {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rg => "rg",
            Self::Grep => "grep",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SearchArgs {
    pattern: String,
    path: String,
    numbered: bool,
    filename: bool,
}

/// Parse only a single explicit-file literal search. Every unknown option
/// falls back rather than being dropped. Last -H/-h wins, like the tools.
pub fn parse_args(tool: SearchTool, args: &[String]) -> Option<SearchArgs> {
    let mut numbered = false;
    let mut filename = false;
    let mut fixed = false;
    let mut positional = Vec::new();
    let mut options = true;
    for arg in args {
        if options && arg == "--" {
            if !positional.is_empty() {
                return None;
            }
            options = false;
            continue;
        }
        if options && arg.starts_with('-') {
            if !positional.is_empty() {
                // GNU option permutation and BSD/POSIX argument ordering
                // differ. Never reinterpret a flag after the pattern.
                return None;
            }
            match arg.as_str() {
                "--line-number" => numbered = true,
                "--with-filename" => filename = true,
                "--no-filename" => filename = false,
                "--fixed-strings" => fixed = true,
                _ if arg.len() > 1 && !arg.starts_with("--") => {
                    for flag in arg[1..].chars() {
                        match flag {
                            'n' => numbered = true,
                            'H' => filename = true,
                            'h' if tool == SearchTool::Grep => filename = false,
                            'F' => fixed = true,
                            _ => return None,
                        }
                    }
                }
                _ => return None,
            }
        } else {
            positional.push(arg);
        }
    }
    if positional.len() != 2 {
        return None;
    }
    let pattern = positional[0];
    if pattern.is_empty()
        || !pattern.is_ascii()
        || pattern.bytes().any(|b| b.is_ascii_control())
        || (!fixed
            && !pattern
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_ -".contains(&b)))
        || positional[1] == "-"
    {
        return None;
    }
    Some(SearchArgs {
        pattern: pattern.clone(),
        path: positional[1].clone(),
        numbered,
        filename,
    })
}

/// A small shell grammar, not a permissive shell approximation. Quotes are
/// accepted; expansions, escapes, operators and unquoted globs are not.
pub fn shell_argv(command: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut started = false;
    for c in command.chars() {
        if matches!(c, '\n' | '\r' | '\0' | '\\' | '$' | '`') {
            return None;
        }
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if matches!(c, '\'' | '"') => {
                quote = Some(c);
                started = true;
            }
            None if matches!(
                c,
                ';' | '|'
                    | '&'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '*'
                    | '?'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '~'
                    | '#'
            ) =>
            {
                return None;
            }
            None if matches!(c, ' ' | '\t') => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None if c.is_whitespace() => return None,
            None => {
                current.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        args.push(current);
    }
    Some(args)
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Eligibility is repeated at execution time. The hook checks only shape
/// and an in-repository regular-file path, never starts/builds an index.
pub fn rewrite(command: &str, cwd: &Path) -> Option<String> {
    let argv = shell_argv(command)?;
    let tool = match argv.first()?.as_str() {
        "rg" => SearchTool::Rg,
        "grep" => SearchTool::Grep,
        _ => return None,
    };
    if native_configuration(tool) {
        return None;
    }
    let parsed = parse_args(tool, &argv[1..])?;
    checked_path(&parsed.path, cwd)?;
    Some(format!(
        "pixel search-compat {} -- {}",
        tool.name(),
        argv[1..]
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" ")
    ))
}

fn checked_path(raw: &str, cwd: &Path) -> Option<(PathBuf, PathBuf, String)> {
    if credential_path(Path::new(raw)) {
        return None;
    }
    let path = cwd.join(raw);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > pixel_index::index::MAX_FILE_BYTES {
        return None;
    }
    let abs = path.canonicalize().ok()?;
    let root = crate::discover_root(cwd).ok()?;
    if !root.join(".pixel").is_dir() {
        return None;
    }
    let relative = abs.strip_prefix(&root).ok()?.to_str()?.to_string();
    if relative.starts_with(".pixel/") || relative.starts_with(".git/") || credential_path(&abs) {
        return None;
    }
    Some((root, abs, relative))
}

/// Credential-shaped paths never qualify for automatic permission grants.
/// This is metadata-only eligibility, not a claim to detect every secret.
fn credential_path(path: &Path) -> bool {
    if path.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("secrets"))
    }) {
        return true;
    }
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    name.starts_with(".env")
        || name.ends_with(".env")
        || name.starts_with("credentials.")
        || (name.contains("secret") && name.contains('.'))
        || name == "serviceaccountkey.json"
        || name.ends_with("-credentials.json")
        || [
            ".pem",
            ".key",
            ".p12",
            ".pfx",
            ".jks",
            ".keystore",
            ".truststore",
            "_rsa",
            "_dsa",
            "_ecdsa",
            "_ed25519",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// The user configured the tool being replaced: its native output may no
/// longer match the literal-search emulation, so the command stays native.
/// Only that tool's own configuration counts. `RIPGREP_CONFIG_PATH` changes
/// nothing about `grep` and `GREP_OPTIONS` nothing about `rg`; a developer
/// with an rg config would otherwise never get a `grep` rewrite.
fn native_configuration(tool: SearchTool) -> bool {
    let variable = match tool {
        SearchTool::Rg => "RIPGREP_CONFIG_PATH",
        SearchTool::Grep => "GREP_OPTIONS",
    };
    std::env::var_os(variable).is_some()
}

fn compatible_output(
    tool: SearchTool,
    args: &SearchArgs,
    cwd: &Path,
) -> Result<(Vec<u8>, i32), &'static str> {
    if std::io::stdout().is_terminal() || native_configuration(tool) {
        return Err("native-configuration");
    }
    let (root, abs, relative) = checked_path(&args.path, cwd).ok_or("unsupported-file")?;
    let before = pixel_index::index::read_regular_bounded(&abs, pixel_index::index::MAX_FILE_BYTES)
        .map_err(|_| "unreadable-file")?;
    if !before.is_ascii() || before.contains(&0) || before.contains(&b'\r') {
        return Err("unsupported-file-bytes");
    }
    let mut index = pixel_index::indexset::IndexSet::open_or_build(
        &root,
        Box::new(pixel_index::TrigramExtractor),
    )
    .map_err(|_| "index-unavailable")?;
    // Refresh the exact file synchronously: a stale watcher must never turn
    // a newly added match into a false negative in the compatibility path.
    index.refresh_file(&relative);
    if !index.paths().contains(&relative) {
        return Err("file-not-covered");
    }
    let (matches, stats) = index
        .search_page_in(
            &regex::escape(&args.pattern),
            0,
            Some(10_000),
            Some(&[relative]),
        )
        .map_err(|_| "search-failed")?;
    if stats.truncated {
        return Err("result-cap");
    }
    if pixel_index::index::read_regular_bounded(&abs, pixel_index::index::MAX_FILE_BYTES)
        .ok()
        .as_deref()
        != Some(before.as_slice())
    {
        return Err("file-changed");
    }
    let mut output = Vec::new();
    for m in &matches {
        if args.filename {
            output.extend_from_slice(args.path.as_bytes());
            output.push(b':');
        }
        if args.numbered {
            output.extend_from_slice(m.line_number.to_string().as_bytes());
            output.push(b':');
        }
        output.extend_from_slice(m.line.as_bytes());
        output.push(b'\n');
        if output.len() > 64 * 1024 {
            return Err("output-cap");
        }
    }
    Ok((output, i32::from(matches.is_empty())))
}

fn record(cwd: &Path, backend: &str, reason: &str) {
    if let Ok(root) = crate::discover_root(cwd) {
        let mut logger = pixel_actionlog::ActionLog::spawn_for_root(&root);
        logger.log(pixel_actionlog::ActionEvent::new(
            "search-compat",
            format!("backend={backend} reason={reason}"),
        ));
        logger.finish();
    }
}

pub fn run(tool: SearchTool, argv: Vec<String>) -> ! {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let result = parse_args(tool, &argv)
        .ok_or("unsupported-arguments")
        .and_then(|args| compatible_output(tool, &args, &cwd));
    match result {
        Ok((output, code)) => {
            record(&cwd, "pixel", "equivalent-literal-file");
            if let Err(error) = std::io::stdout().write_all(&output) {
                eprintln!("{}: {error}", tool.name());
                std::process::exit(2);
            }
            std::process::exit(code);
        }
        Err(_) => {
            // Do not write observability files before a native fallback:
            // recursive searches may include the log in their own corpus.
            let error = std::process::Command::new(tool.name()).args(&argv).exec();
            eprintln!("{}: {error}", tool.name());
            std::process::exit(127);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn literal_parser_preserves_flags_and_rejects_unknown_semantics() {
        let parsed = parse_args(SearchTool::Grep, &args(&["-nHFh", "foo.bar", "a.rs"])).unwrap();
        assert_eq!(parsed.pattern, "foo.bar");
        assert!(parsed.numbered);
        assert!(!parsed.filename);
        for raw in [
            vec!["-rln", "needle", "."],
            vec!["-A20", "needle", "a.rs"],
            vec!["-i", "needle", "a.rs"],
            vec!["needle", "a.rs", "b.rs"],
            vec!["foo.*", "a.rs"],
            vec!["", "a.rs"],
            vec!["needle", "-n", "a.rs"],
        ] {
            assert!(
                parse_args(SearchTool::Grep, &args(&raw)).is_none(),
                "{raw:?}"
            );
        }
        assert!(parse_args(SearchTool::Rg, &args(&["-h", "needle", "a.rs"])).is_none());
    }

    #[test]
    fn shell_parser_is_conservative_and_keeps_quoted_words() {
        assert_eq!(
            shell_argv("grep -F 'foo bar' 'a file.rs'"),
            Some(args(&["grep", "-F", "foo bar", "a file.rs"]))
        );
        assert_eq!(
            shell_argv("grep -F needle '#file'"),
            Some(args(&["grep", "-F", "needle", "#file"]))
        );
        for command in [
            "rg x a | wc -l",
            "rg x a > out",
            "rg $PATTERN a",
            "rg x *.rs",
            "rg 'unterminated",
            "rg x a && touch b",
            "grep -F needle #file",
            "grep needle\u{a0} a.rs",
        ] {
            assert!(shell_argv(command).is_none(), "{command}");
        }
    }
}

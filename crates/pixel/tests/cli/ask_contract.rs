//! Repository ask's real CLI boundary; retrieval failures must never silently skip.
#[cfg(feature = "model2vec")]
#[test]
fn ask_reports_cosine_ranking_coverage_and_honest_human_labels() {
    let repo = std::env::temp_dir().join(format!("pixel-ask-contract-{}", std::process::id()));
    std::fs::create_dir_all(&repo).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(init.status.success());
    std::fs::write(
        repo.join("manual.md"),
        "Manual setup configures the search agent without installation.\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("noise.rs"),
        "pub fn unrelated_binary_reader() {}\n",
    )
    .unwrap();
    let run = |extra: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_pixel"))
            .args(["search-meaning", "manual setup", "."])
            .args(extra)
            .current_dir(&repo)
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("PIXEL_METRICS", "0")
            .output()
            .unwrap()
    };
    let out = run(&["--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let hits = value["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0]["path"], "manual.md");
    for hit in hits {
        let path = hit["path"].as_str().unwrap();
        assert!(!std::path::Path::new(path).is_absolute());
        assert!(!path.contains(&repo.display().to_string()));
        assert_eq!(hit["score"], hit["semantic_score"]);
        assert!(hit["ranking_score"].as_f64().unwrap() > 0.0);
    }
    assert_eq!(hits[0]["lexical_matches"], 2);
    assert_eq!(value["coverage"]["searched_files"], 2);
    assert_eq!(value["coverage"]["degraded"], false);
    // No index at the root: every chunk embedded, nothing written.
    assert_eq!(value["coverage"]["vector_cache"], "no_index");
    assert_eq!(value["coverage"]["embedded_chunks"], 2);
    assert_eq!(value["coverage"]["cached_chunks"], 0);
    assert!(!repo.join(".pixel/code-vectors").exists());
    let limited = run(&["--json", "--limit", "1"]);
    assert!(limited.status.success());
    let limited: serde_json::Value = serde_json::from_slice(&limited.stdout).unwrap();
    assert_eq!(limited["hits"].as_array().unwrap().len(), 1);
    assert_eq!(limited["coverage"]["result_limit_reached"], true);
    let human = run(&[]);
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("hybrid matches"));
    assert!(text.contains("RRF") && text.contains("cosine"));
    assert!(text.contains("manual.md"));
    assert!(!text.contains(&repo.display().to_string()));
    assert!(!text.contains("note: searched a deterministic sample"));

    // Twelve files: the default returns ten hits and says the list was cut,
    // with the whole tree inside the default file budget.
    for i in 0..10 {
        std::fs::write(
            repo.join(format!("extra{i}.rs")),
            "pub fn setup_step() {}\n",
        )
        .unwrap();
    }
    let defaults = run(&["--json"]);
    assert!(defaults.status.success());
    let defaults: serde_json::Value = serde_json::from_slice(&defaults.stdout).unwrap();
    assert_eq!(defaults["hits"].as_array().unwrap().len(), 10);
    assert_eq!(defaults["coverage"]["result_limit_reached"], true);
    assert_eq!(defaults["coverage"]["candidate_files"], 12);
    assert_eq!(defaults["coverage"]["max_files"], 6000);
    assert_eq!(defaults["coverage"]["file_limit_reached"], false);

    // Over an explicit budget, the human output names the sample.
    let sampled = run(&["--max-files", "5"]);
    assert!(sampled.status.success());
    let text = String::from_utf8(sampled.stdout).unwrap();
    assert!(
        text.contains(
            "note: searched a deterministic sample of 5 of 12 eligible files (--max-files 5)"
        ),
        "{text}"
    );

    // Once the root carries an index, the second question reads every
    // chunk vector back from `.pixel/code-vectors` and embeds none.
    let index = std::process::Command::new(env!("CARGO_BIN_EXE_pixel"))
        .args(["build-index", "."])
        .current_dir(&repo)
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(
        index.status.success(),
        "{}",
        String::from_utf8_lossy(&index.stderr)
    );
    let cold: serde_json::Value = serde_json::from_slice(&run(&["--json"]).stdout).unwrap();
    assert_eq!(cold["coverage"]["vector_cache"], "persisted");
    assert_eq!(cold["coverage"]["chunks"], 12);
    assert_eq!(
        cold["coverage"]["embedded_chunks"], 3,
        "the ten identical extra files are one text, embedded once"
    );
    assert!(repo.join(".pixel/code-vectors/manifest.json").is_file());
    let warm: serde_json::Value = serde_json::from_slice(&run(&["--json"]).stdout).unwrap();
    assert_eq!(warm["coverage"]["embedded_chunks"], 0);
    assert_eq!(warm["coverage"]["cached_chunks"], 12);
    assert_eq!(warm["hits"], cold["hits"], "the cache changes no ranking");
    std::fs::remove_dir_all(&repo).unwrap();
}

/// The defaults a user reads in `--help` are the ones the command applies:
/// ten hits, and a file budget of 6000 that covers a mid-size repository.
#[test]
fn search_meaning_help_states_the_defaults() {
    let out = crate::support::pixel_command()
        .args(["search-meaning", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    // One flag's block: its line and the description lines up to the next flag.
    let flag = |name: &str| {
        let mut lines = help
            .lines()
            .skip_while(|line| !line.trim_start().starts_with(name));
        let first = lines.next().unwrap_or_default();
        std::iter::once(first)
            .chain(lines.take_while(|line| !line.trim_start().starts_with('-')))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let limit = flag("--limit");
    assert!(limit.contains("[default: 10]"), "{help}");
    let max_files = flag("--max-files");
    assert!(max_files.contains("[default: 6000]"), "{help}");
    assert!(max_files.contains("deterministic sample"), "{help}");
}

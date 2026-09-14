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
    assert!(hits[0]["path"].as_str().unwrap().ends_with("manual.md"));
    for hit in hits {
        assert_eq!(hit["score"], hit["semantic_score"]);
        assert!(hit["ranking_score"].as_f64().unwrap() > 0.0);
    }
    assert_eq!(hits[0]["lexical_matches"], 2);
    assert_eq!(value["coverage"]["searched_files"], 2);
    assert_eq!(value["coverage"]["degraded"], false);
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
    std::fs::remove_dir_all(&repo).unwrap();
}

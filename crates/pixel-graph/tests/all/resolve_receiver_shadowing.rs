//! Integration test for T0 receiver shadowing: a qualified call from a file
//! whose own same-name function is the callee must not resolve to itself.

use std::fs;
use std::path::Path;

use pixel_git::GitRunner;
use pixel_graph::build::build_graph;
use pixel_graph::plan::{PlanQuery, run_plan_queries};
use pixel_graph::{EdgeKind, GraphStore, Tier};

/// Three Rust files: `crates/bridge/src/lib.rs` has the wrapper `f` (which
/// calls `graph::build::f()`) and `g` (which calls the wrapper unqualified);
/// `crates/graph/src/build.rs` has the real, otherwise uncalled `f`; and
/// `crates/standalone/src/lib.rs` has `h`, uncalled with no same-name shadow
/// (the control for the dead-code assertion).
fn fixture(root: &Path) {
    let files = [
        (
            "crates/bridge/src/lib.rs",
            "pub fn f() -> u32 {\n    graph::build::f()\n}\n\npub fn g() -> u32 {\n    f()\n}\n",
        ),
        (
            "crates/graph/src/build.rs",
            "pub fn f() -> u32 {\n    1\n}\n",
        ),
        (
            "crates/standalone/src/lib.rs",
            "pub fn h() -> u32 {\n    2\n}\n",
        ),
    ];
    for (path, body) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
}

#[test]
fn qualified_call_no_longer_links_the_callers_own_same_name_symbol() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let db = root.path().join(".pixel/graph.db");
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    build_graph(root.path(), &db).unwrap();
    let store = GraphStore::open(&db).unwrap();

    let bridge_file = store
        .file_by_path("crates/bridge/src/lib.rs")
        .unwrap()
        .unwrap();
    let bridge_f = store
        .symbols_by_name("f", 10)
        .unwrap()
        .into_iter()
        .find(|s| s.file_id == bridge_file.id)
        .expect("bridge's own `f` is extracted");
    let bridge_g = store
        .symbols_by_name("g", 10)
        .unwrap()
        .into_iter()
        .find(|s| s.file_id == bridge_file.id)
        .expect("bridge's own `g` is extracted");

    // Pre-fix, `graph::build::f()` inside `f` resolved (Probable) to `f`
    // itself: the wrapper was its own caller. The call is now unresolved.
    assert!(
        store
            .edges_from(bridge_f.id, Some(EdgeKind::Calls))
            .unwrap()
            .is_empty(),
        "the qualified call must not link `f` to itself"
    );
    // The unqualified `f()` inside `g` still resolves Exact to the local `f`.
    let callers = store.edges_to(bridge_f.id, Some(EdgeKind::Calls)).unwrap();
    assert_eq!(callers.len(), 1, "callers: {callers:?}");
    assert_eq!(callers[0].src_id, bridge_g.id);
    assert_eq!(callers[0].tier, Tier::Exact);
    // The unresolved row keeps the epistemic envelope honest, which is what
    // makes `pixel plan --query dead-code` skip the name.
    let envelope = store.envelope_for_name("f").unwrap();
    assert!(envelope.lower_bound, "envelope: {envelope:?}");
    assert_eq!(
        envelope.unresolved_same_name, 1,
        "one unresolved `graph::build::f()` site"
    );
    // `pixel plan --query dead-code` keys on that envelope: the real `f` has
    // no linked caller, so without it the name would be listed as removable.
    // The genuinely uncalled `h` (no same-name shadow) is still listed, so
    // the assertion is not vacuous.
    let runner = GitRunner::new(root.path());
    let findings = run_plan_queries(&store, root.path(), &runner, &[PlanQuery::DeadCode]).unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f.file == "crates/standalone/src/lib.rs"),
        "the uncalled control `h` is still listed: {findings:?}"
    );
    assert!(
        findings
            .iter()
            .all(|f| f.file != "crates/graph/src/build.rs"),
        "the shadowed `f` must not be listed as dead: {findings:?}"
    );
}

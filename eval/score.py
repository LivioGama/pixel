#!/usr/bin/env python3
"""Score eval transcripts against scenario rubrics.

Claude arms: stream-json (last `result` event carries the answer + metrics).
agy arms: stream-json (final `result` event with `response`).

Score = sum(must pattern hits) - sum(never penalties), floored at 0.
`answered` is False when the run ended in error (e.g. error_max_turns) —
an unanswered run scores 0 regardless of partial text.
"""
import argparse, json, re, sys
from pathlib import Path

def load_result(path: Path, cli: str):
    """Return (answer, metrics) for a transcript, parsed per CLI."""
    answer, metrics = "", {}
    if cli == "codex":
        texts, turns, input_tokens = [], 0, 0
        for line in path.read_text().splitlines():
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "item.completed":
                item = ev.get("item") or {}
                if item.get("type") == "agent_message" and item.get("text"):
                    texts.append(item["text"])
            elif ev.get("type") == "turn.completed":
                turns += 1
                usage = ev.get("usage") or {}
                input_tokens += usage.get("input_tokens") or 0
        answer = "\n\n".join(texts)
        metrics = {"answered": bool(answer.strip()), "turns": turns or None,
                   "input_tokens": input_tokens or None, "cost_usd": None}
        return answer, metrics
    if cli == "pi":
        try:
            r = json.loads(path.read_text())
        except json.JSONDecodeError:
            return "", {"answered": False, "turns": None, "input_tokens": None,
                        "cost_usd": None}
        return r.get("response") or "", {
            "answered": r.get("status") == "SUCCESS" and bool((r.get("response") or "").strip()),
            "turns": r.get("num_turns"), "input_tokens": None, "cost_usd": None}
    for line in path.read_text().splitlines():
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "result":          # claude
            answer = ev.get("result") or ""
            metrics = {
                "answered": ev.get("subtype") == "success",
                "turns": ev.get("num_turns"),
                "input_tokens": (ev.get("usage") or {}).get("input_tokens"),
                "cost_usd": ev.get("total_cost_usd"),
            }
        elif "result" in ev and isinstance(ev["result"], dict):  # agy
            r = ev["result"]
            answer = r.get("response") or ""
            metrics = {
                "answered": r.get("status") == "SUCCESS",
                "turns": r.get("num_turns"),
                "input_tokens": (r.get("usage") or {}).get("input_tokens"),
                "cost_usd": None,
            }
    return answer, metrics

def score_answer(answer: str, rubric: dict):
    earned, hits, penalties = 0, [], 0
    for m in rubric.get("must", []):
        if re.search(m["pattern"], answer, re.IGNORECASE):
            earned += m["points"]
            hits.append(m["pattern"])
    for n in rubric.get("never", []):
        if re.search(n["pattern"], answer, re.IGNORECASE):
            penalties += n.get("penalty", 1)
    return max(0, earned - penalties), hits, penalties

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True)
    ap.add_argument("--scenarios-dir", required=True)
    ap.add_argument("arms", nargs="*")
    args = ap.parse_args()
    results = Path(args.results)
    rubrics = {p.stem: json.loads(p.read_text())
               for p in Path(args.scenarios_dir).glob("*.json")}
    rows = []
    for f in sorted(results.rglob("*.jsonl")):
        if f.name == "scores.json":
            continue
        stem = f.stem                      # <scenario>-<arm>.<cli>
        scenario = next((k for k in rubrics if stem.startswith(k + "-")), None)
        if scenario is None:
            continue
        arm, cli = stem[len(scenario) + 1:].rsplit(".", 1)
        if args.arms and arm not in args.arms:
            continue
        answer, metrics = load_result(f, cli)
        if not metrics:
            # A transcript with no terminal result (interrupted run) is a
            # failed trial: score it zero instead of dropping it.
            metrics = {"answered": False, "turns": None, "input_tokens": None,
                       "cost_usd": None}
        if not metrics["answered"]:
            score = 0
        else:
            score, hits, penalties = score_answer(answer, rubrics[scenario])
            metrics["penalties"] = penalties
        rows.append({"scenario": scenario, "arm": arm, "cli": cli,
                     "score": score, "max": sum(m["points"] for m in rubrics[scenario]["must"]),
                     **metrics})
    rows.sort(key=lambda r: (r["scenario"], r["arm"], r["cli"]))
    # table
    print(f"{'scenario':<18} {'arm':<10} {'cli':<7} {'score':>5} {'answered':>8} {'turns':>5} {'in_tok':>8} {'cost':>6}")
    for r in rows:
        print(f"{r['scenario']:<18} {r['arm']:<10} {r['cli']:<7} {r['score']:>3}/{r['max']:<2} "
              f"{str(r['answered']):>8} {str(r['turns']):>5} {str(r['input_tokens']):>8} "
              f"{('%.2f' % r['cost_usd']) if r.get('cost_usd') else '-':>6}")
    # per-arm means
    print("\narm means over scenarios:")
    by_arm = {}
    for r in rows:
        by_arm.setdefault(r["arm"], []).append(r)
    for arm, rs in sorted(by_arm.items()):
        mean = sum(r["score"] for r in rs) / len(rs)
        turns = sum(r["turns"] or 0 for r in rs)
        answered = sum(1 for r in rs if r["answered"])
        print(f"  {arm:<10} mean={mean:5.1f}  answered={answered}/{len(rs)}  total_turns={turns}")
    (results / "scores.json").write_text(json.dumps(rows, indent=2))

if __name__ == "__main__":
    main()

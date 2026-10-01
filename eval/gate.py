#!/usr/bin/env python3
"""Never-worse gate: a candidate arm passes only if, on EVERY scenario, it
answers and scores at least the baseline arm — and burns no more than
1.5x baseline turns. Any single regression fails the gate."""
import argparse, json, sys
from pathlib import Path

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True, help="dir containing scores.json")
    ap.add_argument("--scenarios-dir", help="dir of scenario rubrics; every scenario must have rows on both sides")
    ap.add_argument("--candidate", required=True)
    ap.add_argument("--baseline", default="baseline")
    ap.add_argument("--turns-slack", type=float, default=1.5)
    args = ap.parse_args()
    rows = json.loads((Path(args.results) / "scores.json").read_text())
    expected = None
    if args.scenarios_dir:
        expected = sorted(p.stem for p in Path(args.scenarios_dir).glob("*.json"))
    base, cand = {}, {}
    for r in rows:
        if r["cli"] != "claude":
            continue
        if r["arm"] == args.baseline:
            base.setdefault(r["scenario"], []).append(r)
        elif r["arm"] == args.candidate:
            cand.setdefault(r["scenario"], []).append(r)
    failures = []
    print(f"gate: {args.candidate} vs {args.baseline} (turns slack x{args.turns_slack})")
    if not base:
        print(f"GATE FAIL: no {args.baseline} rows in scores.json — nothing to compare against")
        sys.exit(1)
    if not cand:
        print(f"GATE FAIL: no {args.candidate} rows in scores.json")
        sys.exit(1)
    for scenario in sorted(expected if expected is not None else set(base) | set(cand)):
        b = base.get(scenario, [])
        c = cand.get(scenario, [])
        if not b or not c:
            print(f"  {scenario:<18} missing comparison data "
                  f"(baseline rows: {len(b)}, candidate rows: {len(c)}) -> FAIL")
            failures.append(scenario)
            continue
        b_mean = sum(x["score"] for x in b) / len(b) if b else None
        c_mean = sum(x["score"] for x in c) / len(c) if c else None
        b_turns = sum(x["turns"] or 0 for x in b) / len(b) if b else None
        c_turns = sum(x["turns"] or 0 for x in c) / len(c) if c else None
        c_answered = all(x["answered"] for x in c) if c else False
        ok = (
            c is not None and c
            and c_answered
            and b_mean is not None and c_mean >= b_mean
            and b_turns and c_turns <= args.turns_slack * b_turns
        )
        print(f"  {scenario:<18} score {c_mean if c_mean is None else round(c_mean,1)} vs "
              f"{b_mean if b_mean is None else round(b_mean,1)} | turns "
              f"{c_turns if c_turns is None else round(c_turns,1)} vs "
              f"{b_turns if b_turns is None else round(b_turns,1)} | "
              f"answered={c_answered} -> {'PASS' if ok else 'FAIL'}")
        if not ok:
            failures.append(scenario)
    if failures:
        print(f"GATE FAIL: {', '.join(failures)}")
        sys.exit(1)
    print("GATE PASS")

if __name__ == "__main__":
    main()

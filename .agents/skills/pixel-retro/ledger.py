"""Append retro items to ~/.local/state/pixel-retro/seen.tsv from a JSON file.

Usage: python3 ledger.py <items.json>
The file holds a list of {"fingerprint": str, "verdict": str, "ref": str}.
Values stay data end to end: they are read from JSON, validated, and written
with tabs and newlines replaced, never pasted into shell or Python source.
"""
import datetime, json, os, re, sys

VERDICTS = {"suggested", "picked", "fixed", "wontfix", "not-pixel"}
REF = re.compile(r"^([a-z]+:[0-9a-f]{6,} #\d+|https://github\.com/\S+)$")


def clean(value):
    return re.sub(r"[\t\r\n]", " ", value)


items = json.load(open(sys.argv[1]))
for item in items:
    if item["verdict"] not in VERDICTS or not REF.match(item["ref"]):
        sys.exit(f"refused, nothing written: {item!r}")
path = os.path.expanduser("~/.local/state/pixel-retro/seen.tsv")
os.makedirs(os.path.dirname(path), exist_ok=True)
today = datetime.date.today().isoformat()
with open(path, "a") as f:
    for item in items:
        f.write(f"{today}\t{clean(item['fingerprint'])}\t{item['verdict']}\t{item['ref']}\n")
print(f"{len(items)} row(s) appended to {path}")

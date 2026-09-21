# Regenerating these fixtures

Exact commands that produced each committed case file, run from the repository
root. They are recorded because the generators' defaults do not reproduce every
file: the Ruby corpora need a wider truth-set window (see below), so a default
re-run would silently yield a different, smaller set.

`GITNEXUS_CLI` is only needed by the benchmark harnesses, not by the generators.

## Impact cases (caller ground truth)

```sh
python3 scripts/vs-gitnexus/gen-truth.py /path/to/pixel      rust       8 target,tests,fixtures        > impact-rust.json
python3 scripts/vs-gitnexus/gen-truth.py /path/to/GitNexus   typescript 8 dist,vendor,node_modules,test,tests,__tests__ > impact-typescript.json

# Ruby: MIN_FILES/MAX_FILES widened from the 3..8 default to 2..12.
# Ruby resolves far fewer names unambiguously -- the default window leaves
# only three candidates on dd-trace-rb -- so the committed sets hold truth
# sets of 2 and 10 files, outside the default range.
MIN_FILES=2 MAX_FILES=12 python3 scripts/vs-gitnexus/gen-truth.py /path/to/dd-trace-rb ruby 8 vendor,spec,test,sig > impact-ruby-ddtrace.json
MIN_FILES=2 MAX_FILES=12 python3 scripts/vs-gitnexus/gen-truth.py /path/to/alonetone   ruby 8 vendor,spec,test     > impact-ruby-alonetone.json
```

## Call-path cases

Derived from the Rust impact cases, so regenerate those first:

```sh
python3 scripts/vs-gitnexus/gen-path-cases.py /path/to/pixel impact-rust.json 6 > path-rust.json
```

## Change-mapping targets

`changes-rust.json` is hand-written, not generated: three functions in three
different crates, each identified by file and definition line. Refresh the line
numbers by hand if those definitions move.

---
title: "Pixel compared"
description: "Pixel against each tool a team weighs it against, one page each: a short answer, the figures side by side, how they were measured, and where the other tool wins."
---

<!-- The entries come from data/alternatives.toml, in its order; each page is content/vs/<slug>.md, prose only (the figures come from the data). crates/pixel/tests/cli/docs_drift.rs reads these files, so every `pixel <command>` quoted here must exist. -->

Each page says how its figures were obtained before it shows them: measured head to head on the same cases, set beside the other tool's published figure on a different sample, or not benchmarked at all. Each keeps the rows where the other tool wins, and every figure is on the [benchmarks page](../benchmarks/) first.

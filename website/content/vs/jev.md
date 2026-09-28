---
title: "Pixel vs Jev"
description: "Pixel's optional classify command against Jev on coding judgement calls: winnow:e4b on Ollaya's typed-decisions accuracy and local latency beside TypeSafe Jev's hosted figures."
tool: "jev"
---

## How they differ

Jev is a model built for one job: settling a coding question with a bounded set of answers. `pixel classify` puts the same kind of question to a model you configure and returns one probability per label. It is an optional add-on: nothing else in Pixel calls a model.

## Choosing

Pick Jev when its slightly higher typed-decisions accuracy matters more than local latency. Pick `pixel classify` to keep the choice of model, the bill and the data path in your hands, next to an index your agent already queries.

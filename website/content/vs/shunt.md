---
title: "Pixel vs shunt"
description: "Pixel and Spotify's shunt both cut what an agent reads from large files: Pixel from its index, shunt through a second model. Published figures, different samples."
tool: "shunt"
---

## How they differ

shunt keeps large reads out of the agent's context by handing them to a worker model, whose tokens are billed too. Pixel answers the same question, what does this file contain, with `pixel list-signatures`: the file's definitions from its local index, with no model involved.

## Choosing

Pick shunt if your team already runs Spotify Portal with AiKA and wants a worker that also drafts boilerplate. Pick Pixel for a single binary, no second model on the bill, and the rest of its index: callers, blast radius and Git history.

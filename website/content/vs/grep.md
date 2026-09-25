---
title: "Pixel vs grep"
description: "Grep finds the line; the cost is the file read that follows. Pixel's answer to the same question against that full read, measured on well-known large files."
tool: "grep"
---

## How they differ

Grep, ripgrep and an agent's built-in search return matching lines, and the agent then opens the file around the match to understand it. That read is what costs tokens. `pixel search-content` takes the same regular expressions and returns the matches from the index; `pixel list-signatures` gives what the file contains without reading it whole, and `pixel who-calls` answers the next question without a second search.

## Choosing

Keep grep for a quick look in a directory Pixel has not indexed, or at a file that did not exist a second ago. Use Pixel when the agent would otherwise open whole files to understand what it found.

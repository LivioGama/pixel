---
title: "Savings estimate"
description: "What Pixel could spare your team's agents each month on large-file reads, on your own numbers. An estimate, computed in this page; nothing leaves it."
layout: "savings"
# The estimator's inputs, in the order the form shows them. `key` is the
# share link's fragment key (#d=10&s=4…), so never rename one: old links
# would lose that value. layouts/_default/savings.html renders the defaults'
# result without JavaScript; the script recomputes on every change. A help
# text's {fullMin} and {fullMax} are the kept files' full-read sizes.
inputs:
  - key: "d"
    label: "Developers running agents"
    default: 10
    min: 1
    max: 100000
    step: 1
    unit: ""
    help: "Everyone whose agent works in your repositories."
  - key: "s"
    label: "Sessions per developer per day"
    default: 4
    min: 0
    max: 200
    step: 1
    unit: ""
    help: "Agent sessions started, not prompts: a session usually spans several turns."
  - key: "r"
    label: "Large-file reads per session"
    default: 5
    min: 0
    max: 1000
    step: 1
    unit: ""
    help: "Whole reads of a large file to learn what it holds. Short files and targeted line ranges do not count."
  - key: "t"
    label: "Tokens per full read"
    default: 10000
    min: 0
    max: 1000000
    step: 500
    unit: ""
    help: "Bytes divided by four. 10,000 is about 1,000 lines at the roughly ten tokens a line of the well-known files, which run {fullMin} to {fullMax} tokens each."
  - key: "p"
    label: "Input price"
    default: 3
    min: 0
    max: 1000
    step: 0.05
    unit: "$ per million tokens"
    help: "Your model's list price for uncached input tokens. The default is a round placeholder, not a quote."
  - key: "w"
    label: "Working days per month"
    default: 21
    min: 1
    max: 31
    step: 1
    unit: "days"
    help: "Five days a week, less a public holiday."
---

---
title: "About"
description: "Who makes Pixel, how the project runs, and how to reach the people behind it."
---

<!-- The people come from data/team.toml ({{% team %}}), which the home's JSON-LD reads too: change them there, never here. The contact channels restate SECURITY.md and CONTRIBUTING.md. -->

## Why Pixel exists

AI coding agents learn a codebase by reading it, and most of what they read is whole files they needed a few lines of. Pixel is a local code index they query instead: the signatures of a large file, the callers of a function, the commit that changed a line, each fitted to a token budget. The [benchmarks](/benchmarks/) measure what that saves, and say where it does not.

## Who makes it

{{% team %}}

Everyone who has contributed code is listed on the repository's [contributors page](https://github.com/LivioGama/pixel/graphs/contributors).

## How the project runs

- **Open source**: the code, its history and every review are public on [GitHub](https://github.com/LivioGama/pixel), under the [MIT License](https://github.com/LivioGama/pixel/blob/main/LICENSE).
- **Local by design**: the index stays in `.pixel/` on your machine, and the binary sends no telemetry ([Privacy Policy](/legal/privacy-policy/)).
- **Public releases**: each version is a tag on `main`, with its [changelog](https://github.com/LivioGama/pixel/blob/main/CHANGELOG.md) and a [release](https://github.com/LivioGama/pixel/releases) you can follow as a feed.
- **Contributions welcome**: [CONTRIBUTING.md](https://github.com/LivioGama/pixel/blob/main/CONTRIBUTING.md) lists what a pull request needs to be merged.

## Contact

| For | Where |
| --- | --- |
| a question, an idea, or feedback on how you use Pixel | [GitHub Discussions](https://github.com/LivioGama/pixel/discussions) |
| a bug, with the command and its output | [GitHub Issues](https://github.com/LivioGama/pixel/issues) |
| a security vulnerability, privately | [a private security advisory](https://github.com/LivioGama/pixel/security/advisories/new) ([SECURITY.md](https://github.com/LivioGama/pixel/blob/main/SECURITY.md)) |
| a request about your data | as the [Privacy Policy](/legal/privacy-policy/#who-is-responsible) says |

There is no contact form, so the site keeps nothing you send. Every channel above is on GitHub, where you can read past threads before you write.

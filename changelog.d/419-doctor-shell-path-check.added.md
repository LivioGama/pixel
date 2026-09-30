**doctor:** the new `binary.shell-path` check asks the resolved login shell to resolve `pixel` and goes yellow when it cannot — an agent harness that inherits an environment without `pixel` on `PATH` silently runs without it instead of failing loudly.
 ([#419](https://github.com/LivioGama/pixel/pull/419))

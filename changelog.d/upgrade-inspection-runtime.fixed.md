**cli:** `pixel self-update` checks the same daemon socket it requested to stop, so a runtime-directory failure cannot switch inspection to another path and falsely report a completed upgrade.

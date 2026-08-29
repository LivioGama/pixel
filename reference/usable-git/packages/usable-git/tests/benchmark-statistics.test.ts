import { describe, expect, test } from "bun:test";
import {
  bootstrapConfidenceInterval,
  percentile,
  summarizeMetric,
} from "../../../benchmarks/statistics.ts";

describe("benchmark statistics", () => {
  test("computes deterministic median, p95, and seeded bootstrap confidence intervals", () => {
    expect(percentile([1, 2, 3, 4], 0.5)).toBe(2.5);
    expect(percentile([1, 2, 3, 4], 0.95)).toBeCloseTo(3.85);

    const first = bootstrapConfidenceInterval([1, 2, 3, 4, 5], {
      seed: 12_345,
      resamples: 200,
      statistic: (values) => percentile(values, 0.5),
    });
    const second = bootstrapConfidenceInterval([1, 2, 3, 4, 5], {
      seed: 12_345,
      resamples: 200,
      statistic: (values) => percentile(values, 0.5),
    });

    expect(first).toEqual(second);
    expect(summarizeMetric([1, 2, 3, 4, 5], 12_345).median).toBe(3);
  });
});

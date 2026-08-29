import { expect, test } from "bun:test";
import {
  generatePublishPropertyCase,
  runPublishPropertyMatrix,
} from "./support/publish-property.ts";

const parsePositiveInteger = (name: string, fallback: number) => {
  const raw = process.env[name];
  if (raw === undefined) return fallback;
  const parsed = Number(raw);
  if (!Number.isSafeInteger(parsed) || parsed < 1) {
    throw new Error(`${name} must be a positive safe integer`);
  }
  return parsed;
};

const seed = parsePositiveInteger("USABLE_GIT_PROPERTY_SEED", 0x5eed1234);
const caseCount = parsePositiveInteger("USABLE_GIT_PROPERTY_CASES", 2);

test("derives deterministic, distinct publish cases from seed and index", () => {
  expect(generatePublishPropertyCase(seed, 7)).toEqual(
    generatePublishPropertyCase(seed, 7),
  );
  expect(generatePublishPropertyCase(seed, 7)).not.toEqual(
    generatePublishPropertyCase(seed, 8),
  );
});

test(
  `publishes exact paths across ${caseCount} seeded dirty repositories`,
  async () => {
    const summary = await runPublishPropertyMatrix({ seed, caseCount });

    expect(summary).toEqual({
      seed,
      cases: caseCount,
      failures: 0,
      oracleChecks: caseCount * summary.checksPerCase,
      checksPerCase: summary.checksPerCase,
    });
    expect(summary.checksPerCase).toBeGreaterThanOrEqual(12);
  },
  Math.max(30_000, caseCount * 5_000),
);

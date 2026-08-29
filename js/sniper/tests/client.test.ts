import { describe, expect, test } from "bun:test";
import { report, serializeValues, swallow } from "../src/client.ts";

describe("serializeValues", () => {
  test("passes through primitives", () => {
    expect(serializeValues(42)).toBe(42);
    expect(serializeValues("hi")).toBe("hi");
    expect(serializeValues(true)).toBe(true);
    expect(serializeValues(null)).toBe(null);
  });

  test("redacts secret-looking keys at every depth", () => {
    const out = serializeValues({
      apiToken: "sk-live-123",
      SECRET: "hush",
      password: "pw",
      publicKey: "also-redacted-by-key-match",
      nested: { sessionToken: "abc", ok: 1 },
      fine: "visible",
    }) as Record<string, unknown>;
    expect(out.apiToken).toBe("[redacted]");
    expect(out.SECRET).toBe("[redacted]");
    expect(out.password).toBe("[redacted]");
    expect(out.publicKey).toBe("[redacted]");
    expect((out.nested as Record<string, unknown>).sessionToken).toBe("[redacted]");
    expect((out.nested as Record<string, unknown>).ok).toBe(1);
    expect(out.fine).toBe("visible");
  });

  test("caps depth at 2", () => {
    const out = serializeValues({ a: { b: { c: { d: 1 } } } }) as Record<string, unknown>;
    const a = out.a as Record<string, unknown>;
    expect(a.b).toBe("[object Object]");
  });

  test("caps arrays and deep arrays", () => {
    const out = serializeValues({ a: { deep: [1, 2, 3] } }) as Record<string, unknown>;
    const a = out.a as Record<string, unknown>;
    expect(a.deep).toBe("[array 3]");
    const big = serializeValues(Array.from({ length: 50 }, (_, i) => i)) as unknown[];
    expect(big.length).toBe(20);
  });

  test("caps total size at 1KB", () => {
    const out = serializeValues({
      blob1: "x".repeat(300),
      blob2: "y".repeat(300),
      blob3: "z".repeat(300),
      blob4: "w".repeat(300),
      blob5: "v".repeat(300),
    }) as { truncated?: boolean; excerpt?: string };
    expect(out.truncated).toBe(true);
    expect((out.excerpt ?? "").length).toBeLessThanOrEqual(1024);
  });

  test("long strings are trimmed", () => {
    expect((serializeValues("a".repeat(500)) as string).length).toBe(257);
  });

  test("errors, functions, bigints, symbols, unserializable", () => {
    expect(serializeValues(new TypeError("boom"))).toEqual({ name: "TypeError", message: "boom" });
    expect(serializeValues(() => 1)).toMatch(/^\[function/);
    expect(serializeValues(10n)).toBe("10n");
    const cyclic: Record<string, unknown> = {};
    cyclic.self = cyclic;
    // depth cap breaks the cycle before JSON.stringify ever sees it
    expect(() => JSON.stringify(serializeValues(cyclic))).not.toThrow();
  });
});

describe("report / swallow outside a browser", () => {
  test("never throw without window/fetch endpoints", () => {
    expect(() => report(new Error("x"), { values: { a: 1 } })).not.toThrow();
    expect(() => swallow("string error", "label", { token: "t" })).not.toThrow();
    expect(() => report({ odd: "object" })).not.toThrow();
  });
});

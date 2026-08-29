/**
 * Browser-side sniper client — zero dependencies, every path never-throws.
 *
 * One import in any app root installs window 'error' + 'unhandledrejection'
 * listeners and a reentrancy-guarded console.error patch, tracks the HMR rev
 * via `import.meta.hot`, and exposes `report(err, {values})` plus
 * `swallow(err, label, values?)` — the empty-catch replacement.
 *
 * Everything POSTs same-origin to the vite plugin's dev-only ingest endpoint
 * (`POST /__sniper/report`) with `keepalive` so tab closes don't drop records.
 */

export const SNIPER_ENDPOINT = "/__sniper/report";

const REDACT_KEY = /secret|token|key|password/i;
const MAX_SERIALIZED_BYTES = 1024;
const MAX_DEPTH = 2;

/**
 * Depth-2, 1KB-capped, secret-redacting serialization of arbitrary values.
 * Never throws; the output is always JSON-safe.
 */
export const serializeValues = (value: unknown): unknown => {
  try {
    const serialized = serializeInner(value, 0);
    const encoded = JSON.stringify(serialized) ?? "null";
    if (encoded.length <= MAX_SERIALIZED_BYTES) return serialized;
    return { truncated: true, excerpt: encoded.slice(0, MAX_SERIALIZED_BYTES) };
  } catch {
    return { unserializable: true };
  }
};

const serializeInner = (value: unknown, depth: number): unknown => {
  if (value === null || value === undefined) return value ?? null;
  const kind = typeof value;
  if (kind === "string") {
    const s = value as string;
    return s.length > 256 ? `${s.slice(0, 256)}…` : s;
  }
  if (kind === "number" || kind === "boolean") return value;
  if (kind === "bigint") return `${String(value)}n`;
  if (kind === "function") return `[function ${(value as { name?: string }).name || "anonymous"}]`;
  if (kind === "symbol") return String(value);
  if (value instanceof Error) {
    return { name: value.name, message: value.message };
  }
  if (depth >= MAX_DEPTH) {
    if (Array.isArray(value)) return `[array ${value.length}]`;
    return `[object ${((value as object).constructor?.name as string) || "Object"}]`;
  }
  if (Array.isArray(value)) {
    return value.slice(0, 20).map((item) => serializeInner(item, depth + 1));
  }
  const out: Record<string, unknown> = {};
  let count = 0;
  for (const [key, entry] of Object.entries(value as Record<string, unknown>)) {
    if (count >= 20) {
      out["…"] = "more keys truncated";
      break;
    }
    out[key] = REDACT_KEY.test(key) ? "[redacted]" : serializeInner(entry, depth + 1);
    count += 1;
  }
  return out;
};

// ---------------------------------------------------------------------------
// Posting
// ---------------------------------------------------------------------------

interface ClientPayload {
  surface: string;
  message: string;
  kind?: string;
  stack_raw?: string;
  values?: unknown;
  extra?: Record<string, unknown>;
}

let hmrRev = 0;
let reporting = false;
let endpoint = SNIPER_ENDPOINT;

const post = (payload: ClientPayload): void => {
  try {
    const body = JSON.stringify({
      ...payload,
      extra: { ...(payload.extra ?? {}), hmrRev, href: safeHref() },
    });
    void fetch(endpoint, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body,
      keepalive: true,
    }).catch(() => {});
  } catch {
    /* never throw from the sink */
  }
};

const safeHref = (): string | undefined => {
  try {
    return typeof location !== "undefined" ? location.href : undefined;
  } catch {
    return undefined;
  }
};

const errorFields = (err: unknown): { message: string; kind?: string; stack_raw?: string } => {
  if (err instanceof Error) {
    return { message: err.message || String(err), kind: err.name, stack_raw: err.stack };
  }
  if (typeof err === "string") return { message: err };
  try {
    return { message: JSON.stringify(serializeValues(err)) };
  } catch {
    return { message: String(err) };
  }
};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/** Explicitly report an error (surface `reported`). Never throws. */
export const report = (err: unknown, opts?: { values?: unknown; label?: string }): void => {
  try {
    const fields = errorFields(err);
    post({
      surface: "reported",
      ...fields,
      values: opts?.values !== undefined ? serializeValues(opts.values) : undefined,
      extra: opts?.label ? { label: opts.label } : undefined,
    });
  } catch {
    /* never throw */
  }
};

/**
 * The empty-catch replacement: record the error the catch is about to eat,
 * with a label and optional values, then return (the catch still swallows).
 */
export const swallow = (err: unknown, label: string, values?: unknown): void => {
  try {
    const fields = errorFields(err);
    post({
      surface: "reported",
      ...fields,
      values: values !== undefined ? serializeValues(values) : undefined,
      extra: { label, swallowed: true },
    });
  } catch {
    /* never throw */
  }
};

/** Current HMR revision (bumps on every vite:afterUpdate). */
export const currentHmrRev = (): number => hmrRev;

// ---------------------------------------------------------------------------
// Install (side effect on import in a browser; no-op elsewhere)
// ---------------------------------------------------------------------------

let installed = false;

export const installSniperClient = (opts?: { endpoint?: string }): void => {
  try {
    if (installed || typeof window === "undefined") return;
    installed = true;
    if (opts?.endpoint) endpoint = opts.endpoint;

    window.addEventListener("error", (event) => {
      try {
        if (reporting) return;
        reporting = true;
        const err = event.error as unknown;
        const fields =
          err != null
            ? errorFields(err)
            : { message: event.message || "unknown window error" };
        post({
          surface: "browser-window",
          ...fields,
          extra: {
            filename: event.filename || undefined,
            lineno: event.lineno || undefined,
            colno: event.colno || undefined,
          },
        });
      } catch {
        /* never throw */
      } finally {
        reporting = false;
      }
    });

    window.addEventListener("unhandledrejection", (event) => {
      try {
        if (reporting) return;
        reporting = true;
        post({ surface: "browser-rejection", ...errorFields(event.reason) });
      } catch {
        /* never throw */
      } finally {
        reporting = false;
      }
    });

    const originalConsoleError = console.error.bind(console);
    console.error = (...args: unknown[]): void => {
      originalConsoleError(...args);
      try {
        if (reporting) return;
        reporting = true;
        const firstError = args.find((a) => a instanceof Error) as Error | undefined;
        const message = args
          .map((a) => (a instanceof Error ? a.message : typeof a === "string" ? a : safeString(a)))
          .join(" ")
          .slice(0, 2000);
        post({
          surface: "browser-console",
          message: message || "console.error",
          kind: firstError?.name,
          stack_raw: firstError?.stack,
        });
      } catch {
        /* never throw */
      } finally {
        reporting = false;
      }
    };

    try {
      // Present only under vite dev; optional-chained so plain builds no-op.
      const hot = (import.meta as { hot?: { on?: (e: string, cb: () => void) => void } }).hot;
      hot?.on?.("vite:afterUpdate", () => {
        hmrRev += 1;
      });
    } catch {
      /* not running under vite */
    }
  } catch {
    /* installation must never break the app */
  }
};

const safeString = (value: unknown): string => {
  try {
    return JSON.stringify(serializeValues(value)) ?? String(value);
  } catch {
    return String(value);
  }
};

installSniperClient();

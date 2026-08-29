import type { ErrorCode } from "./contracts/v1.ts";

export class UsableGitError extends Error {
  readonly code: ErrorCode;
  readonly details?: Record<string, unknown>;

  constructor(code: ErrorCode, message: string, details?: Record<string, unknown>) {
    super(message);
    this.name = "UsableGitError";
    this.code = code;
    this.details = details;
  }
}

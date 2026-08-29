import { createV1McpEnvelopeSchema, type operationSchema } from "../v1.ts";
import { branchResultSchema } from "./branch.ts";
import { diffResultSchema } from "./diff.ts";
import { historyResultSchema } from "./history.ts";
import { inspectResultSchema } from "./inspect.ts";
import { publishResultSchema } from "./publish.ts";
import { pushResultSchema } from "./push.ts";
import { reviewResultSchema } from "./review.ts";
import { searchResultSchema } from "./search.ts";
import { shipResultSchema } from "./ship.ts";
import { syncResultSchema } from "./sync.ts";
import { updateResultSchema } from "./update.ts";
import type { z } from "zod";

export const operationResultSchemas = {
  inspect: inspectResultSchema,
  review: reviewResultSchema,
  history: historyResultSchema,
  diff: diffResultSchema,
  publish: publishResultSchema,
  push: pushResultSchema,
  ship: shipResultSchema,
  branch: branchResultSchema,
  sync: syncResultSchema,
  update: updateResultSchema,
  search: searchResultSchema,
} as const;

export const operationMcpOutputSchemas = {
  inspect: createV1McpEnvelopeSchema(inspectResultSchema),
  review: createV1McpEnvelopeSchema(reviewResultSchema),
  history: createV1McpEnvelopeSchema(historyResultSchema),
  diff: createV1McpEnvelopeSchema(diffResultSchema),
  publish: createV1McpEnvelopeSchema(publishResultSchema),
  push: createV1McpEnvelopeSchema(pushResultSchema),
  ship: createV1McpEnvelopeSchema(shipResultSchema),
  branch: createV1McpEnvelopeSchema(branchResultSchema),
  sync: createV1McpEnvelopeSchema(syncResultSchema),
  update: createV1McpEnvelopeSchema(updateResultSchema),
  search: createV1McpEnvelopeSchema(searchResultSchema),
} as const;

export const parseOperationResult = (
  operation: z.infer<typeof operationSchema>,
  result: unknown,
) => operationResultSchemas[operation].parse(result);

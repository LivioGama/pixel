// Shared byte-capped patch pagination used by review and diff.

export const patchStatistics = (patch: string) => {
  let additions = 0;
  let deletions = 0;
  for (const line of patch.split("\n")) {
    if (line.startsWith("+") && !line.startsWith("+++")) additions += 1;
    if (line.startsWith("-") && !line.startsWith("---")) deletions += 1;
  }
  return { additions, deletions };
};

export type PatchCursor = { item: number; character: number };

const sliceWithinBytes = (value: string, character: number, cap: number) => {
  let end = character;
  let bytes = 0;
  for (const codePoint of value.slice(character)) {
    const size = Buffer.byteLength(codePoint);
    if (bytes + size > cap) break;
    bytes += size;
    end += codePoint.length;
  }
  return { value: value.slice(character, end), end, bytes };
};

export const paginatePatches = <T extends { patch: string; truncated: boolean }>(
  items: T[],
  cursor: PatchCursor,
  byteCap: number,
): { items: T[]; bytes: number; next?: PatchCursor } => {
  if (cursor.item > items.length || (cursor.item === items.length && cursor.character !== 0)) {
    throw new Error("Invalid pagination cursor");
  }
  const selected: T[] = [];
  let bytes = 0;
  let itemIndex = cursor.item;
  let character = cursor.character;

  while (itemIndex < items.length && bytes < byteCap) {
    const item = items[itemIndex]!;
    if (character > item.patch.length) throw new Error("Invalid pagination cursor");
    const slice = sliceWithinBytes(item.patch, character, byteCap - bytes);
    if (slice.end === character && item.patch.length > character) break;
    const complete = slice.end === item.patch.length;
    selected.push({ ...item, patch: slice.value, truncated: !complete });
    bytes += slice.bytes;
    if (!complete) {
      character = slice.end;
      break;
    }
    itemIndex += 1;
    character = 0;
  }

  const hasMore = itemIndex < items.length;
  return {
    items: selected,
    bytes,
    ...(hasMore ? { next: { item: itemIndex, character } } : {}),
  };
};

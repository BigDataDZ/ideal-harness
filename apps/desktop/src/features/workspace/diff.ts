import type { DiffLine } from "./types.ts";

const MAX_DIFF_LINES = 400;

export function buildLineDiff(before: string | null, after: string | null): DiffLine[] {
  if (before === null || after === null) return [];
  const left = before.split("\n").slice(0, MAX_DIFF_LINES);
  const right = after.split("\n").slice(0, MAX_DIFF_LINES);
  const prefix = commonPrefix(left, right);
  const suffix = commonSuffix(left, right, prefix);
  const rows: DiffLine[] = [];
  for (let index = 0; index < prefix; index += 1) rows.push(context(index, index, left[index] ?? ""));
  for (let index = prefix; index < left.length - suffix; index += 1) {
    rows.push({ kind: "removed", beforeLine: index + 1, afterLine: null, text: left[index] ?? "" });
  }
  for (let index = prefix; index < right.length - suffix; index += 1) {
    rows.push({ kind: "added", beforeLine: null, afterLine: index + 1, text: right[index] ?? "" });
  }
  for (let offset = suffix; offset > 0; offset -= 1) {
    const leftIndex = left.length - offset;
    const rightIndex = right.length - offset;
    rows.push(context(leftIndex, rightIndex, left[leftIndex] ?? ""));
  }
  return rows;
}

function context(before: number, after: number, text: string): DiffLine {
  return { kind: "context", beforeLine: before + 1, afterLine: after + 1, text };
}

function commonPrefix(left: readonly string[], right: readonly string[]): number {
  let index = 0;
  while (index < left.length && index < right.length && left[index] === right[index]) index += 1;
  return index;
}

function commonSuffix(left: readonly string[], right: readonly string[], prefix: number): number {
  let count = 0;
  while (
    count < left.length - prefix &&
    count < right.length - prefix &&
    left[left.length - count - 1] === right[right.length - count - 1]
  ) count += 1;
  return count;
}

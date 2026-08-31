import { parseEventFrame, ProjectionProtocolError } from "./adapters.ts";
import type { SessionEventFrameDto } from "./types.ts";

export function parseSseBlock(block: string): SessionEventFrameDto {
  let id: string | null = null;
  let eventName = "message";
  const data: string[] = [];
  for (const rawLine of block.replaceAll("\r\n", "\n").split("\n")) {
    if (rawLine === "" || rawLine.startsWith(":")) {
      continue;
    }
    const separator = rawLine.indexOf(":");
    const field = separator === -1 ? rawLine : rawLine.slice(0, separator);
    let value = separator === -1 ? "" : rawLine.slice(separator + 1);
    if (value.startsWith(" ")) {
      value = value.slice(1);
    }
    if (field === "id") {
      if (id !== null || value === "" || value.includes("\0")) {
        throw new ProjectionProtocolError("SSE id is missing or duplicated");
      }
      id = value;
    } else if (field === "event") {
      eventName = value;
    } else if (field === "data") {
      data.push(value);
    }
  }
  if (eventName !== "session_event" || id === null || data.length === 0) {
    throw new ProjectionProtocolError("SSE block is not a session_event frame");
  }
  let decoded: unknown;
  try {
    decoded = JSON.parse(data.join("\n"));
  } catch {
    throw new ProjectionProtocolError("SSE data is not valid JSON");
  }
  const frame = parseEventFrame(decoded);
  if (String(frame.record.seq) !== id) {
    throw new ProjectionProtocolError("SSE id does not match record.seq");
  }
  return frame;
}

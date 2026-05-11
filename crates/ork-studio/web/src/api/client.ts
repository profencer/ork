// ADR-0055 §`Studio API (introspection-only)`: wraps fetch + envelope
// unwrapping. Every `/studio/api/*` response carries
// `{ studio_api_version, data }`; the SPA decides what to do when the
// version drifts.

export const STUDIO_API_VERSION = 1;

export interface Envelope<T> {
  studio_api_version: number;
  data: T;
  error?: string;
}

export class VersionMismatch extends Error {
  constructor(public server: number, public client: number) {
    super(
      `Studio bundle is older than the server (server=${server}, client=${client}); reload to upgrade.`,
    );
  }
}

export interface FetchOptions {
  signal?: AbortSignal;
  method?: string;
  body?: unknown;
}

export async function studioFetch<T>(
  path: string,
  opts: FetchOptions = {},
): Promise<T> {
  const method = opts.method ?? "GET";
  const init: RequestInit = {
    method,
    headers: opts.body
      ? { "content-type": "application/json" }
      : undefined,
    body: opts.body ? JSON.stringify(opts.body) : undefined,
    signal: opts.signal,
  };
  const resp = await fetch(path, init);
  const text = await resp.text();
  let parsed: Envelope<T>;
  try {
    parsed = text.length > 0 ? (JSON.parse(text) as Envelope<T>) : ({} as Envelope<T>);
  } catch {
    throw new Error(
      `studioFetch: non-JSON response from ${path}: ${text.slice(0, 200)}`,
    );
  }
  if (
    parsed.studio_api_version &&
    parsed.studio_api_version > STUDIO_API_VERSION
  ) {
    throw new VersionMismatch(parsed.studio_api_version, STUDIO_API_VERSION);
  }
  if (!resp.ok) {
    throw new Error(parsed.error ?? `${method} ${path}: ${resp.status}`);
  }
  return parsed.data as T;
}

// SSE consumer for the agent stream — ADR-0056 §`Streaming`.
export interface SseEvent {
  event: string;
  data: unknown;
}

export async function* streamAgent(
  agentId: string,
  body: unknown,
  signal?: AbortSignal,
): AsyncGenerator<SseEvent> {
  const resp = await fetch(`/api/agents/${encodeURIComponent(agentId)}/stream`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
    signal,
  });
  if (!resp.ok || !resp.body) {
    throw new Error(`agent stream: ${resp.status}`);
  }
  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  for (;;) {
    const { value, done } = await reader.read();
    if (done) {
      return;
    }
    buf += decoder.decode(value, { stream: true });
    let nl;
    while ((nl = buf.indexOf("\n\n")) !== -1) {
      const block = buf.slice(0, nl);
      buf = buf.slice(nl + 2);
      const ev = parseSseBlock(block);
      if (ev) {
        yield ev;
      }
    }
  }
}

function parseSseBlock(block: string): SseEvent | null {
  let event = "message";
  let data = "";
  for (const line of block.split("\n")) {
    if (line.startsWith("event:")) {
      event = line.slice(6).trim();
    } else if (line.startsWith("data:")) {
      data += line.slice(5).trim();
    }
  }
  if (!data) {
    return null;
  }
  let payload: unknown = data;
  try {
    payload = JSON.parse(data);
  } catch {
    /* keep as string */
  }
  return { event, data: payload };
}

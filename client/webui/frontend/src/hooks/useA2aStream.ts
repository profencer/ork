import { useCallback, useState } from "react";

const apiBase = () => import.meta.env.VITE_API_BASE ?? "";

type StreamChunk = { line: string; done: boolean };

/**
 * Post a full JSON-RPC body to `/webui/api/conversations/{id}/messages` and read SSE/bytes.
 * ADR-0017: pairs with the Rust proxy that streams A2A `message/stream` events.
 */
export function useA2aStream(token: string | null) {
  const [lastError, setLastError] = useState<string | null>(null);

  const startStream = useCallback(
    async function* startStream(
      conversationId: string,
      agentId: string,
      jsonrpc: unknown
    ): AsyncGenerator<StreamChunk, void, undefined> {
      if (!token) {
        setLastError("not authenticated");
        return;
      }
      setLastError(null);
      const url = `${apiBase()}/webui/api/conversations/${conversationId}/messages`;
      const res = await fetch(url, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${token}`,
        },
        body: JSON.stringify({ agent_id: agentId, jsonrpc }),
      });
      if (!res.ok) {
        const t = await res.text();
        setLastError(t || res.statusText);
        return;
      }
      const body = res.body;
      if (!body) {
        return;
      }
      const reader = body.getReader();
      const dec = new TextDecoder();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) {
          break;
        }
        if (value) {
          yield { line: dec.decode(value, { stream: true }), done: false };
        }
      }
    },
    [token]
  );

  return { startStream, lastError };
}

import { useState } from "react";
import { streamAgent } from "../api/client";

// `crypto.randomUUID()` is available in every browser Studio targets (Chrome,
// Safari, Firefox 95+) and on Node 19+. The conditional fallback keeps
// the panel honest if it ever gets rendered in an older test runtime.
function randomUuid(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  const rnd = () => Math.floor(Math.random() * 0xffff).toString(16).padStart(4, "0");
  return `${rnd()}${rnd()}-${rnd()}-4${rnd().slice(1)}-${rnd()}-${rnd()}${rnd()}${rnd()}`;
}

interface ToolCall {
  id: string;
  name: string;
  args?: unknown;
}

interface ChatProps {
  agentId: string;
  agents?: string[];
  onPickAgent?: (id: string) => void;
}

export function Chat({ agentId, agents, onPickAgent }: ChatProps) {
  const [input, setInput] = useState("");
  const [response, setResponse] = useState("");
  const [toolCalls, setToolCalls] = useState<ToolCall[]>([]);
  const [streaming, setStreaming] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function send() {
    setResponse("");
    setToolCalls([]);
    setError(null);
    setStreaming(true);
    try {
      // ADR-0003 A2A Message wire shape (crates/ork-a2a/src/types.rs:156):
      // role, parts (typed by `kind`), message_id are all required.
      const body = {
        message: {
          role: "user",
          parts: [{ kind: "text", text: input }],
          message_id: randomUuid(),
        },
      };
      for await (const ev of streamAgent(agentId, body)) {
        const data = ev.data as { kind?: string; text?: string; name?: string; id?: string; args?: unknown };
        switch (ev.event) {
          case "delta":
            if (typeof data.text === "string") {
              setResponse((r) => r + data.text);
            }
            break;
          case "tool_call":
            if (data.id && data.name) {
              setToolCalls((t) => [...t, { id: data.id!, name: data.name!, args: data.args }]);
            }
            break;
          case "completed":
            break;
        }
      }
    } catch (e) {
      setError(String(e));
    } finally {
      setStreaming(false);
    }
  }

  const showPicker = agents && agents.length > 1 && onPickAgent;

  return (
    <section aria-label="Chat" className="p-4 space-y-3">
      <div className="flex items-baseline gap-3">
        <h1 className="text-2xl font-semibold">Chat — {agentId}</h1>
        {showPicker && (
          <select
            aria-label="agent-picker"
            value={agentId}
            onChange={(e) => onPickAgent!(e.target.value)}
            className="border rounded px-2 py-1 text-sm"
          >
            {agents!.map((id) => (
              <option key={id} value={id}>
                {id}
              </option>
            ))}
          </select>
        )}
      </div>

      <div className="flex gap-2">
        <input
          aria-label="prompt"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder="Say something to the agent…"
          className="flex-1 border rounded px-2 py-1"
        />
        <button
          type="button"
          disabled={streaming || input.length === 0}
          onClick={send}
          className="px-3 py-1 rounded bg-blue-600 text-white disabled:opacity-50"
        >
          Send
        </button>
      </div>

      {error && (
        <div role="alert" className="text-red-700">
          {error}
        </div>
      )}

      <div>
        <h2 className="text-sm uppercase tracking-wide text-gray-500">Response</h2>
        <pre
          aria-label="response"
          data-streaming={streaming ? "true" : "false"}
          className="whitespace-pre-wrap bg-gray-50 p-2 rounded min-h-[3rem]"
        >
          {response}
        </pre>
      </div>

      <div>
        <h2 className="text-sm uppercase tracking-wide text-gray-500">Tool calls</h2>
        {toolCalls.length === 0 ? (
          <p className="text-gray-400">— none —</p>
        ) : (
          <ul aria-label="tool-calls" className="space-y-1">
            {toolCalls.map((t) => (
              <li
                key={t.id}
                data-tool-call-id={t.id}
                className="px-2 py-1 rounded bg-amber-50 font-mono text-sm"
              >
                {t.name}
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}

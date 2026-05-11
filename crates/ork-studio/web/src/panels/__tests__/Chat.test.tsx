import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { Chat } from "../Chat";

// ADR-0055 AC #5 (Chat panel): "sends a message, receives a streamed
// response, and renders the tool-call list when the agent makes a call."
//
// The SSE shape follows ADR-0003 / ADR-0056 §`Streaming`:
//
//   event: delta\ndata: {"kind":"delta","text":"Hello "}
//   event: tool_call\ndata: {"kind":"tool_call","id":"call_1","name":"lookup","args":{}}
//   event: delta\ndata: {"kind":"delta","text":"world."}
//   event: completed\ndata: {"kind":"completed"}

function sseBody() {
  const blocks = [
    `event: delta\ndata: ${JSON.stringify({ kind: "delta", text: "Hello " })}\n\n`,
    `event: tool_call\ndata: ${JSON.stringify({ kind: "tool_call", id: "call_1", name: "weather.lookup", args: { city: "SF" } })}\n\n`,
    `event: delta\ndata: ${JSON.stringify({ kind: "delta", text: "world." })}\n\n`,
    `event: completed\ndata: ${JSON.stringify({ kind: "completed" })}\n\n`,
  ];
  return new ReadableStream<Uint8Array>({
    start(controller) {
      const enc = new TextEncoder();
      for (const b of blocks) {
        controller.enqueue(enc.encode(b));
      }
      controller.close();
    },
  });
}

describe("Chat", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url === "/api/agents/weather/stream") {
          return new Response(sseBody(), {
            status: 200,
            headers: { "content-type": "text/event-stream" },
          });
        }
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );
  });
  afterEach(() => vi.unstubAllGlobals());

  it("streams deltas and renders the tool-call chip", async () => {
    render(<Chat agentId="weather" />);
    const input = screen.getByLabelText<HTMLInputElement>("prompt");
    fireEvent.change(input, { target: { value: "hi" } });
    fireEvent.click(screen.getByText("Send"));

    await waitFor(() => {
      const r = screen.getByLabelText("response");
      expect(r.textContent).toBe("Hello world.");
    });

    const toolCalls = screen.getByLabelText("tool-calls");
    expect(toolCalls.textContent).toMatch(/weather\.lookup/);
    expect(toolCalls.querySelector('[data-tool-call-id="call_1"]')).not.toBeNull();
  });
});

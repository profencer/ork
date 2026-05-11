import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { Memory } from "../Memory";

describe("Memory", () => {
  const threads = [
    {
      thread_id: "t-1",
      last_message_at: "2026-05-10T12:00:00Z",
      message_count: 3,
    },
  ];

  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        if (url.startsWith("/studio/api/memory?")) {
          return new Response(
            JSON.stringify({
              studio_api_version: 1,
              data: { working: null, threads, recent_recall: [] },
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        if (
          url.startsWith("/studio/api/memory/threads/t-1") &&
          init?.method === "DELETE"
        ) {
          threads.length = 0;
          return new Response(
            JSON.stringify({ studio_api_version: 1, data: { ok: true } }),
            { status: 200 },
          );
        }
        throw new Error(`unexpected fetch: ${url} ${init?.method ?? "GET"}`);
      }),
    );
  });
  afterEach(() => vi.unstubAllGlobals());

  it("lists threads and removes one when delete is clicked", async () => {
    render(<Memory resource="00000000-0000-0000-0000-000000000000" />);
    await waitFor(() => screen.getByText(/t-1/));
    fireEvent.click(screen.getByLabelText("delete-thread-t-1"));
    await waitFor(() => {
      const list = screen.queryByLabelText("threads");
      expect(list).toBeNull();
    });
  });
});

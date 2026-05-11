import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { Workflows } from "../Workflows";

describe("Workflows", () => {
  let postedBody: unknown = null;
  let postCount = 0;
  beforeEach(() => {
    postedBody = null;
    postCount = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        const method = init?.method ?? "GET";
        // ADR-0056 M1 reviewer-deferred: GET /runs is not mounted by
        // the auto-router. Studio relies purely on POST responses.
        if (url === "/api/workflows/weather/run" && method === "POST") {
          postedBody = init?.body ? JSON.parse(String(init.body)) : null;
          postCount += 1;
          return new Response(
            JSON.stringify({
              studio_api_version: 1,
              data: { run_id: `run-${postCount}` },
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        throw new Error(`unexpected fetch: ${method} ${url}`);
      }),
    );
  });
  afterEach(() => vi.unstubAllGlobals());

  it("submits the input JSON and appends the new run to the panel-local list", async () => {
    render(<Workflows workflowId="weather" />);
    fireEvent.change(screen.getByLabelText("workflow-input"), {
      target: { value: '{"city":"SF"}' },
    });
    fireEvent.click(screen.getByText("Run"));
    await waitFor(() => {
      const list = screen.getByLabelText("runs");
      expect(list.textContent).toMatch(/run-1/);
    });
    // ADR-0056 `WorkflowRunInput`: the wire body must wrap the user
    // JSON inside `{ input: ... }`. Regression guard for the bug where
    // Studio posted the raw input verbatim and the auto-router rejected
    // with "missing field `input`".
    expect(postedBody).toEqual({ input: { city: "SF" } });
  });

  it("does NOT call GET /api/workflows/:id/runs (route is deferred to ADR-0056 M1)", async () => {
    // Test passes by virtue of beforeEach's fetch mock throwing on any
    // unexpected URL. If a future refactor reintroduces the GET, the
    // initial render or any panel action will explode.
    render(<Workflows workflowId="weather" />);
    // Give the component a chance to fire any effects.
    await new Promise((r) => setTimeout(r, 0));
    expect(postCount).toBe(0);
  });
});

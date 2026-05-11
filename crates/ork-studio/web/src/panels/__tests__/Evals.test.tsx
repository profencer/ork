import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { Evals } from "../Evals";

describe("Evals", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string, init?: RequestInit) => {
        if (url === "/studio/api/evals/run" && init?.method === "POST") {
          return new Response(
            JSON.stringify({
              studio_api_version: 1,
              data: {
                report: {
                  examples: 3,
                  passed: 2,
                  failed: 1,
                  by_scorer: {
                    exact_match: { mean: 0.66, passed: 2, failed: 1 },
                  },
                  regressions: [],
                  raw_path: "report.json",
                },
                fail_on_hit: null,
              },
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        throw new Error(`unexpected fetch: ${url} ${init?.method ?? "GET"}`);
      }),
    );
  });
  afterEach(() => vi.unstubAllGlobals());

  it("submits the form and renders the EvalReport headline", async () => {
    render(<Evals />);
    fireEvent.change(screen.getByLabelText("dataset"), {
      target: { value: "/tmp/weather.jsonl" },
    });
    fireEvent.change(screen.getByLabelText("agent"), {
      target: { value: "weather" },
    });
    fireEvent.click(screen.getByText("Run"));
    await waitFor(() => screen.getByLabelText("eval-report"));
    expect(screen.getByTestId("passed").textContent).toBe("2");
    expect(screen.getByTestId("failed").textContent).toBe("1");
    expect(screen.getByTestId("regressions").textContent).toBe("0");
  });
});

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { Scorers } from "../Scorers";

describe("Scorers", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url.startsWith("/studio/api/scorers/aggregate")) {
          return new Response(
            JSON.stringify({
              studio_api_version: 1,
              data: {
                rows: [
                  {
                    scorer_id: "exact_match",
                    sample_count: 10,
                    pass_rate: 0.8,
                    p50: 1,
                    p95: 1,
                    regression_count: 2,
                  },
                ],
              },
            }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );
  });
  afterEach(() => vi.unstubAllGlobals());

  it("renders the aggregate table with pass-rate and regression counts", async () => {
    render(<Scorers />);
    await waitFor(() => screen.getByLabelText("scorer-aggregate"));
    const row = screen.getByText("exact_match").closest("tr")!;
    expect(row.textContent).toMatch(/80\.0%/);
    // The regression_count is the final cell; sample_count is "10",
    // so an anchored "2" tail is the load-bearing check.
    const cells = row.querySelectorAll("td");
    expect(cells[cells.length - 1].textContent).toBe("2");
  });
});

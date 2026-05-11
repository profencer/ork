import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { Overview } from "../Overview";

const manifest = {
  environment: "development",
  agents: [
    { id: "weather", description: "weather agent", card_name: "weather" },
  ],
  workflows: [{ id: "daily", description: "daily summary" }],
  tools: [{ id: "lookup", description: "" }],
  mcp_servers: [{ id: "docs", transport: { transport: "deferred" } }],
  memory: { name: "libsql" },
  vectors: null,
  scorers: [{ scorer_id: "exact_match", target: null }],
  server: { host: "127.0.0.1", port: 4111, tls_enabled: false, auth_mode: null },
  ork_version: "0.1.0",
  built_at: "2026-05-10T00:00:00Z",
};

describe("Overview", () => {
  beforeEach(() => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string) => {
        if (url === "/studio/api/manifest") {
          return new Response(
            JSON.stringify({ studio_api_version: 1, data: manifest }),
            { status: 200, headers: { "content-type": "application/json" } },
          );
        }
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );
  });
  afterEach(() => vi.unstubAllGlobals());

  it("renders agents, workflows, tools, memory backend from the manifest", async () => {
    render(<Overview />);
    await waitFor(() => screen.getByText("weather"));
    expect(screen.getByText("daily")).toBeInTheDocument();
    expect(screen.getByText("lookup")).toBeInTheDocument();
    expect(screen.getByText("docs")).toBeInTheDocument();
    expect(screen.getByText("libsql")).toBeInTheDocument();
    expect(screen.getByText("exact_match")).toBeInTheDocument();
  });
});

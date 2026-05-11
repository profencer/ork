import { useEffect, useState } from "react";
import { studioFetch } from "../api/client";

interface Manifest {
  environment: string;
  agents: Array<{ id: string; description: string }>;
  workflows: Array<{ id: string; description: string }>;
  tools: Array<{ id: string; description: string }>;
  mcp_servers: Array<{ id: string }>;
  memory: { name: string } | null;
  scorers: Array<{ scorer_id: string }>;
  server: { host: string; port: number };
  ork_version: string;
}

export function Overview() {
  const [manifest, setManifest] = useState<Manifest | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const ac = new AbortController();
    studioFetch<Manifest>("/studio/api/manifest", { signal: ac.signal })
      .then(setManifest)
      .catch((e) => setError(String(e)));
    return () => ac.abort();
  }, []);

  if (error) {
    return (
      <div role="alert" className="p-4 text-red-700">
        Overview: failed to load manifest — {error}
      </div>
    );
  }
  if (!manifest) {
    return <div className="p-4">Loading manifest…</div>;
  }

  return (
    <section aria-label="Overview" className="p-4 space-y-4">
      <header>
        <h1 className="text-2xl font-semibold">ork Studio</h1>
        <p className="text-sm text-gray-600">
          {manifest.environment} · ork v{manifest.ork_version} ·{" "}
          {manifest.server.host}:{manifest.server.port}
        </p>
      </header>
      <SummaryRow label="Agents" items={manifest.agents.map((a) => a.id)} />
      <SummaryRow label="Workflows" items={manifest.workflows.map((w) => w.id)} />
      <SummaryRow label="Tools" items={manifest.tools.map((t) => t.id)} />
      <SummaryRow label="MCP servers" items={manifest.mcp_servers.map((m) => m.id)} />
      <SummaryRow
        label="Memory backend"
        items={manifest.memory ? [manifest.memory.name] : []}
      />
      <SummaryRow
        label="Scorers"
        items={manifest.scorers.map((s) => s.scorer_id)}
      />
    </section>
  );
}

function SummaryRow({ label, items }: { label: string; items: string[] }) {
  return (
    <div>
      <h2 className="text-sm uppercase tracking-wide text-gray-500">{label}</h2>
      {items.length === 0 ? (
        <p className="text-gray-400">— none registered</p>
      ) : (
        <ul className="flex flex-wrap gap-2 mt-1">
          {items.map((id) => (
            <li
              key={id}
              className="px-2 py-1 rounded bg-gray-100 text-sm font-mono"
            >
              {id}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

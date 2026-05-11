import { useEffect, useState } from "react";
import { studioFetch } from "./api/client";
import { Overview } from "./panels/Overview";
import { Chat } from "./panels/Chat";
import { Workflows } from "./panels/Workflows";
import { Memory } from "./panels/Memory";
import { Scorers } from "./panels/Scorers";
import { Evals } from "./panels/Evals";

type Panel = "overview" | "chat" | "workflows" | "memory" | "scorers" | "evals";

interface ManifestShape {
  agents: Array<{ id: string }>;
  workflows: Array<{ id: string }>;
}

export function App() {
  const [panel, setPanel] = useState<Panel>("overview");
  const [agentId, setAgentId] = useState<string | null>(null);
  const [workflowId, setWorkflowId] = useState<string | null>(null);
  const [agents, setAgents] = useState<string[]>([]);
  const [workflows, setWorkflows] = useState<string[]>([]);

  useEffect(() => {
    const ac = new AbortController();
    studioFetch<ManifestShape>("/studio/api/manifest", { signal: ac.signal })
      .then((m) => {
        const a = m.agents.map((x) => x.id);
        const w = m.workflows.map((x) => x.id);
        setAgents(a);
        setWorkflows(w);
        if (a.length > 0) setAgentId(a[0]);
        if (w.length > 0) setWorkflowId(w[0]);
      })
      .catch(() => {
        /* surfaced by individual panels */
      });
    return () => ac.abort();
  }, []);

  return (
    <div className="min-h-screen flex">
      <nav
        aria-label="panels"
        className="w-48 bg-gray-100 p-3 flex flex-col gap-1"
      >
        {(["overview", "chat", "workflows", "memory", "scorers", "evals"] as Panel[]).map(
          (p) => (
            <button
              key={p}
              type="button"
              onClick={() => setPanel(p)}
              aria-pressed={panel === p}
              className={`text-left px-2 py-1 rounded ${
                panel === p ? "bg-blue-600 text-white" : "hover:bg-gray-200"
              }`}
            >
              {p}
            </button>
          ),
        )}
        <div className="mt-auto text-xs text-gray-400 pt-4 border-t">
          Studio · ADR-0055
        </div>
      </nav>
      <main className="flex-1">
        {panel === "overview" && <Overview />}
        {panel === "chat" && (
          agentId ? (
            <Chat
              agentId={agentId}
              agents={agents}
              onPickAgent={setAgentId}
            />
          ) : (
            <EmptyPanel kind="agents" />
          )
        )}
        {panel === "workflows" && (
          workflowId ? (
            <Workflows
              workflowId={workflowId}
              workflows={workflows}
              onPickWorkflow={setWorkflowId}
            />
          ) : (
            <EmptyPanel kind="workflows" />
          )
        )}
        {panel === "memory" && (
          <Memory resource="00000000-0000-0000-0000-000000000000" />
        )}
        {panel === "scorers" && <Scorers />}
        {panel === "evals" && <Evals />}
      </main>
    </div>
  );
}

function EmptyPanel({ kind }: { kind: "agents" | "workflows" }) {
  return (
    <div className="p-4 text-gray-500">
      <h1 className="text-xl font-semibold">No {kind} registered</h1>
      <p>
        Register at least one {kind === "agents" ? "agent" : "workflow"} on
        your `OrkApp` builder, then reload.
      </p>
    </div>
  );
}

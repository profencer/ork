import { useState } from "react";

interface RunSummary {
  run_id: string;
  status: string;
  started_at: string;
}

interface WorkflowsProps {
  workflowId: string;
  workflows?: string[];
  onPickWorkflow?: (id: string) => void;
}

export function Workflows({ workflowId, workflows, onPickWorkflow }: WorkflowsProps) {
  const [input, setInput] = useState("{}");
  const [runs, setRuns] = useState<RunSummary[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const showPicker = workflows && workflows.length > 1 && onPickWorkflow;

  async function run() {
    setBusy(true);
    setError(null);
    try {
      let parsed: unknown = {};
      try {
        parsed = JSON.parse(input);
      } catch (e) {
        setError(`invalid JSON input: ${String(e)}`);
        setBusy(false);
        return;
      }
      // ADR-0056 `WorkflowRunInput` envelope: `{ input: <user JSON> }`.
      // Studio's textarea is the workflow's own input shape; the wire
      // wraps it so the auto-router can carry future fields (idempotency
      // keys, trigger metadata) without a breaking change.
      const resp = await fetch(
        `/api/workflows/${encodeURIComponent(workflowId)}/run`,
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify({ input: parsed }),
        },
      );
      if (!resp.ok) {
        setError(`POST /run failed: ${resp.status}`);
        return;
      }
      // ADR-0056 reviewer-deferred Major finding M1: the auto-router
      // doesn't ship `/api/workflows/:id/runs` yet (it needs a per-run
      // snapshot table). Studio tracks runs in panel-local state from
      // each POST response so the "Past runs" list still works for
      // demo + dev-loop scenarios. The full server-side history is a
      // follow-up.
      const body = await resp.json();
      const runId =
        body?.data?.run_id ?? body?.run_id ?? `run-${Date.now()}`;
      setRuns((prev) => [
        { run_id: runId, status: "submitted", started_at: new Date().toISOString() },
        ...prev,
      ]);
    } finally {
      setBusy(false);
    }
  }

  return (
    <section aria-label="Workflows" className="p-4 space-y-3">
      <div className="flex items-baseline gap-3">
        <h1 className="text-2xl font-semibold">Workflow — {workflowId}</h1>
        {showPicker && (
          <select
            aria-label="workflow-picker"
            value={workflowId}
            onChange={(e) => onPickWorkflow!(e.target.value)}
            className="border rounded px-2 py-1 text-sm"
          >
            {workflows!.map((id) => (
              <option key={id} value={id}>
                {id}
              </option>
            ))}
          </select>
        )}
      </div>
      <label className="block">
        <span className="text-sm uppercase tracking-wide text-gray-500">Input (JSON)</span>
        <textarea
          aria-label="workflow-input"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          rows={6}
          className="block w-full font-mono text-sm border rounded p-2 mt-1"
        />
      </label>
      <button
        type="button"
        onClick={run}
        disabled={busy}
        className="px-3 py-1 rounded bg-blue-600 text-white disabled:opacity-50"
      >
        Run
      </button>
      {error && (
        <div role="alert" className="text-red-700">
          {error}
        </div>
      )}
      <div>
        <h2 className="text-sm uppercase tracking-wide text-gray-500">Past runs</h2>
        {runs.length === 0 ? (
          <p className="text-gray-400">
            — no runs yet — click Run to add one. Server-side history is
            deferred (ADR-0056 M1); the panel lists runs started in this
            session.
          </p>
        ) : (
          <ul aria-label="runs" className="space-y-1">
            {runs.map((r) => (
              <li key={r.run_id} className="font-mono text-sm">
                {r.run_id} · {r.status} ·{" "}
                {new Date(r.started_at).toLocaleTimeString()}
              </li>
            ))}
          </ul>
        )}
      </div>
    </section>
  );
}

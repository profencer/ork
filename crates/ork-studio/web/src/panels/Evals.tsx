import { useState } from "react";
import { studioFetch } from "../api/client";

interface RunReport {
  examples: number;
  passed: number;
  failed: number;
  by_scorer: Record<string, { mean: number; passed: number; failed: number }>;
  regressions: Array<{ scorer_id: string; delta: number }>;
}

interface RunResponse {
  report: RunReport;
  fail_on_hit: number | null;
}

export function Evals() {
  const [dataset, setDataset] = useState("");
  const [agent, setAgent] = useState("");
  const [echoFrom, setEchoFrom] = useState("answer");
  const [scorer, setScorer] = useState("exact_match=answer");
  const [report, setReport] = useState<RunReport | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function run() {
    setBusy(true);
    setError(null);
    setReport(null);
    try {
      const resp = await studioFetch<RunResponse>("/studio/api/evals/run", {
        method: "POST",
        body: {
          dataset,
          agent,
          echo_from: echoFrom,
          scorers: scorer ? [scorer] : [],
        },
      });
      setReport(resp.report);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <section aria-label="Evals" className="p-4 space-y-3">
      <h1 className="text-2xl font-semibold">Evals</h1>
      <fieldset className="grid grid-cols-2 gap-2">
        <label>
          <span className="text-xs uppercase text-gray-500">Dataset path</span>
          <input
            aria-label="dataset"
            value={dataset}
            onChange={(e) => setDataset(e.target.value)}
            className="block w-full border rounded px-2 py-1"
          />
        </label>
        <label>
          <span className="text-xs uppercase text-gray-500">Agent id</span>
          <input
            aria-label="agent"
            value={agent}
            onChange={(e) => setAgent(e.target.value)}
            className="block w-full border rounded px-2 py-1"
          />
        </label>
        <label>
          <span className="text-xs uppercase text-gray-500">Echo from</span>
          <input
            aria-label="echo_from"
            value={echoFrom}
            onChange={(e) => setEchoFrom(e.target.value)}
            className="block w-full border rounded px-2 py-1"
          />
        </label>
        <label>
          <span className="text-xs uppercase text-gray-500">Scorer spec</span>
          <input
            aria-label="scorer"
            value={scorer}
            onChange={(e) => setScorer(e.target.value)}
            className="block w-full border rounded px-2 py-1"
          />
        </label>
      </fieldset>
      <button
        type="button"
        onClick={run}
        disabled={busy || !dataset || !agent}
        className="px-3 py-1 rounded bg-blue-600 text-white disabled:opacity-50"
      >
        Run
      </button>
      {error && (
        <div role="alert" className="text-red-700">
          {error}
        </div>
      )}
      {report && (
        <div aria-label="eval-report" className="text-sm space-y-1">
          <p>
            <strong>Examples:</strong> {report.examples} · passed{" "}
            <span data-testid="passed">{report.passed}</span> · failed{" "}
            <span data-testid="failed">{report.failed}</span> · regressions{" "}
            <span data-testid="regressions">{report.regressions.length}</span>
          </p>
        </div>
      )}
    </section>
  );
}

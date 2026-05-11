import { useEffect, useState } from "react";
import { studioFetch } from "../api/client";

interface AggregateRow {
  scorer_id: string;
  sample_count: number;
  pass_rate: number;
  p50: number;
  p95: number;
  regression_count: number;
}

interface AggregateResponse {
  rows: AggregateRow[];
}

export function Scorers() {
  const [rows, setRows] = useState<AggregateRow[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const ac = new AbortController();
    studioFetch<AggregateResponse>("/studio/api/scorers/aggregate", {
      signal: ac.signal,
    })
      .then((r) => setRows(r.rows ?? []))
      .catch((e) => setError(String(e)));
    return () => ac.abort();
  }, []);

  if (error) {
    return (
      <div role="alert" className="p-4 text-red-700">
        Scorers: {error}
      </div>
    );
  }

  return (
    <section aria-label="Scorers" className="p-4 space-y-3">
      <h1 className="text-2xl font-semibold">Scorers</h1>
      {rows.length === 0 ? (
        <p className="text-gray-400">— no scorer rows yet —</p>
      ) : (
        <table aria-label="scorer-aggregate" className="w-full text-sm">
          <thead className="text-left text-gray-500 uppercase tracking-wide">
            <tr>
              <th>Scorer</th>
              <th>Samples</th>
              <th>Pass rate</th>
              <th>p50</th>
              <th>p95</th>
              <th>Regressions</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.scorer_id} data-scorer-id={r.scorer_id}>
                <td className="font-mono">{r.scorer_id}</td>
                <td>{r.sample_count}</td>
                <td>{(r.pass_rate * 100).toFixed(1)}%</td>
                <td>{r.p50.toFixed(2)}</td>
                <td>{r.p95.toFixed(2)}</td>
                <td>{r.regression_count}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}

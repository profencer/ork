import { useEffect, useState } from "react";

type Props = {
  token: string;
  apiBase: string;
};

/**
 * Lists files under `/api/artifacts` proxy (same JWT). Preview is a raw fetch for a wire ref
 * the user can paste, or a future per-conversation index from ADR-0016.
 */
export function ArtifactPanel({ token, apiBase }: Props) {
  const [refWire, setRefWire] = useState("");
  const [text, setText] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    setText(null);
  }, [refWire]);

  const load = async () => {
    setErr(null);
    if (!refWire) {
      return;
    }
    const enc = encodeURIComponent(refWire);
    const r = await fetch(`${apiBase}/api/artifacts/${enc}`, {
      headers: { Authorization: `Bearer ${token}` },
    });
    if (!r.ok) {
      setErr(await r.text());
      return;
    }
    const t = await r.text();
    setText(t.slice(0, 8_000));
  };

  return (
    <div>
      <p className="mb-2 text-slate-400">
        Enter an artifact `wire` ref to preview (JSON/text) via the public proxy.
      </p>
      <div className="mb-2 flex flex-wrap gap-2">
        <input
          className="min-w-0 flex-1 rounded border border-slate-700 bg-slate-900 px-2 py-1 font-mono text-sm"
          onChange={(e) => setRefWire(e.target.value)}
          placeholder="fs:…"
          value={refWire}
        />
        <button
          className="rounded bg-slate-700 px-3 py-1"
          onClick={() => void load()}
          type="button"
        >
          Load
        </button>
      </div>
      {err && <p className="text-rose-400">{err}</p>}
      <pre className="max-h-80 overflow-auto rounded border border-slate-800 p-2 text-xs">
        {text ?? "(enter a wire and click Load)"}
      </pre>
    </div>
  );
}

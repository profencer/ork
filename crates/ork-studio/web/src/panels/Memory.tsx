import { useEffect, useState } from "react";
import { studioFetch } from "../api/client";

interface MemoryView {
  working: unknown;
  threads: Array<{
    thread_id: string;
    last_message_at: string;
    message_count: number;
  }>;
  recent_recall: Array<{
    message_id: string;
    thread_id: string;
    content: string;
    score: number;
  }>;
}

export function Memory({ resource }: { resource: string }) {
  const [view, setView] = useState<MemoryView | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function refresh() {
    setError(null);
    try {
      const v = await studioFetch<MemoryView>(
        `/studio/api/memory?resource=${encodeURIComponent(resource)}`,
      );
      setView(v);
    } catch (e) {
      setError(String(e));
    }
  }

  useEffect(() => {
    void refresh();
  }, [resource]);

  async function deleteThread(thread_id: string) {
    setError(null);
    const resp = await fetch(
      `/studio/api/memory/threads/${encodeURIComponent(thread_id)}?resource=${encodeURIComponent(resource)}`,
      { method: "DELETE" },
    );
    if (!resp.ok) {
      setError(`DELETE thread failed: ${resp.status}`);
      return;
    }
    await refresh();
  }

  if (error) {
    return (
      <div role="alert" className="p-4 text-red-700">
        Memory: {error}
      </div>
    );
  }

  return (
    <section aria-label="Memory" className="p-4 space-y-3">
      <h1 className="text-2xl font-semibold">Memory · {resource}</h1>
      <div>
        <h2 className="text-sm uppercase tracking-wide text-gray-500">Threads</h2>
        {!view || view.threads.length === 0 ? (
          <p className="text-gray-400">— no threads —</p>
        ) : (
          <ul aria-label="threads" className="space-y-1">
            {view.threads.map((t) => (
              <li
                key={t.thread_id}
                className="flex items-center gap-2 font-mono text-sm"
              >
                <span className="flex-1">
                  {t.thread_id} · {t.message_count} msg ·{" "}
                  {new Date(t.last_message_at).toLocaleString()}
                </span>
                <button
                  type="button"
                  className="px-2 py-0.5 rounded bg-red-100 text-red-800 text-xs"
                  aria-label={`delete-thread-${t.thread_id}`}
                  onClick={() => deleteThread(t.thread_id)}
                >
                  Delete
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
      <div>
        <h2 className="text-sm uppercase tracking-wide text-gray-500">Working memory</h2>
        <pre
          aria-label="working-memory"
          className="whitespace-pre-wrap bg-gray-50 p-2 rounded text-sm"
        >
          {view?.working ? JSON.stringify(view.working, null, 2) : "—"}
        </pre>
      </div>
    </section>
  );
}

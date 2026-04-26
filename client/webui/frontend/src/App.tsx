import { useCallback, useEffect, useState } from "react";
import { useA2aStream } from "./hooks/useA2aStream";
import { AgentPicker } from "./views/AgentPicker";
import type { AgentCard } from "./views/types";
import { ArtifactPanel } from "./views/ArtifactPanel";
import { ProjectSidebar } from "./views/ProjectSidebar";
import { SettingsView } from "./views/SettingsView";

const TOKEN_KEY = "ork.webui.jwt";
const API = import.meta.env.VITE_API_BASE ?? "";

type Tab = "chat" | "artifacts" | "settings";
type Me = { user_id: string; tenant_id: string; scopes: string[] };
type Project = { id: string; label: string };
type Conversation = {
  id: string;
  context_id: string;
  project_id: string | null;
  label: string;
};

export function App() {
  const [token, setToken] = useState<string | null>(
    () => localStorage.getItem(TOKEN_KEY)
  );
  const [view, setView] = useState<Tab>("chat");
  const [me, setMe] = useState<Me | null>(null);
  const [agents, setAgents] = useState<AgentCard[]>([]);
  const [agentId, setAgentId] = useState("");
  const [projects, setProjects] = useState<Project[]>([]);
  const [selectedProject, setSelectedProject] = useState<string | null>(null);
  const [conversations, setConversations] = useState<Conversation[]>([]);
  const [conv, setConv] = useState<Conversation | null>(null);
  const [message, setMessage] = useState("Hello from Web UI");
  const [out, setOut] = useState("");
  const { startStream, lastError } = useA2aStream(token);

  const authed = useCallback(
    async (path: string, init: RequestInit = {}) => {
      if (!token) {
        return null;
      }
      return fetch(`${API}${path}`, {
        ...init,
        headers: {
          "Content-Type": "application/json",
          Authorization: `Bearer ${token}`,
          ...(init.headers || {}),
        },
      });
    },
    [token]
  );

  useEffect(() => {
    if (!token) {
      return;
    }
    void (async () => {
      const r = await authed("/webui/api/me");
      if (r?.ok) {
        setMe((await r.json()) as Me);
      }
    })();
  }, [token, authed]);

  useEffect(() => {
    if (!token) {
      return;
    }
    void (async () => {
      const r = await authed("/webui/api/agents");
      if (r?.ok) {
        const a = (await r.json()) as AgentCard[];
        setAgents(a);
        if (!agentId && a[0]) {
          setAgentId(a[0].id);
        }
      }
    })();
  }, [token, authed, agentId]);

  const refreshProjects = useCallback(async () => {
    const r = await authed("/webui/api/projects");
    if (r?.ok) {
      setProjects((await r.json()) as Project[]);
    }
  }, [authed]);

  const refreshConvs = useCallback(async () => {
    const q = selectedProject
      ? `?project_id=${encodeURIComponent(selectedProject)}`
      : "";
    const r = await authed(`/webui/api/conversations${q}`);
    if (r?.ok) {
      setConversations((await r.json()) as Conversation[]);
    }
  }, [authed, selectedProject]);

  useEffect(() => {
    if (token) {
      void refreshProjects();
    }
  }, [token, refreshProjects]);

  useEffect(() => {
    if (token) {
      void refreshConvs();
    }
  }, [token, selectedProject, refreshConvs]);

  const saveToken = (t: string) => {
    const v = t.trim();
    if (!v) {
      return;
    }
    localStorage.setItem(TOKEN_KEY, v);
    setToken(v);
  };

  const startChat = useCallback(async () => {
    if (!token || !agentId) {
      return;
    }
    if (!message.trim()) {
      return;
    }
    setOut("");
    const params = {
      message: {
        message_id: crypto.randomUUID(),
        role: "user" as const,
        parts: [{ kind: "text" as const, text: message.trim() }],
        task_id: null,
        context_id: conv ? conv.context_id : null,
        metadata: null,
      },
    };
    const jsonrpc = {
      jsonrpc: "2.0",
      id: 1,
      method: "message/stream",
      params,
    };
    if (!conv) {
      setOut("Select or create a conversation first");
      return;
    }
    let all = "";
    for await (const c of startStream(
      conv.id,
      agentId,
      jsonrpc
    )) {
      all += c.line;
      setOut(all);
    }
  }, [token, agentId, conv, message, startStream]);

  if (!token) {
    return (
      <div className="mx-auto max-w-md p-6">
        <h1 className="mb-4 text-2xl font-semibold">ork Web UI</h1>
        <p className="mb-2 text-slate-400">Paste a JWT (bearer) for the API.</p>
        <form
          onSubmit={(e) => {
            e.preventDefault();
            const t = (e.currentTarget.elements.namedItem("jwt") as HTMLInputElement)
              .value;
            saveToken(t);
          }}
        >
          <input
            className="mb-2 w-full rounded border border-slate-700 bg-slate-900 px-3 py-2"
            name="jwt"
            type="password"
            placeholder="JWT"
          />
          <button
            className="rounded bg-emerald-600 px-4 py-2 text-white"
            type="submit"
          >
            Save
          </button>
        </form>
      </div>
    );
  }

  return (
    <div className="mx-auto flex max-w-5xl min-h-screen flex-col gap-4 p-4 md:flex-row">
      <ProjectSidebar
        projects={projects}
        onDelete={async (id) => {
          if (!confirm("Delete project?")) {
            return;
          }
          const r = await authed(`/webui/api/projects/${id}`, { method: "DELETE" });
          if (r?.ok) {
            void refreshProjects();
            void refreshConvs();
          }
        }}
        onNew={async (label) => {
          const r = await authed("/webui/api/projects", {
            method: "POST",
            body: JSON.stringify({ label }),
          });
          if (r?.ok) {
            void refreshProjects();
          }
        }}
        onSelect={setSelectedProject}
        selected={selectedProject}
      />
      <div className="min-w-0 flex-1">
        <nav className="mb-4 flex gap-2 border-b border-slate-800 pb-2">
          {(
            [
              ["chat", "Chat"],
              ["artifacts", "Artifacts"],
              ["settings", "Settings"],
            ] as const
          ).map(([k, l]) => (
            <button
              key={k}
              className={
                view === k
                  ? "rounded-t bg-slate-800 px-3 py-1"
                  : "px-3 py-1 text-slate-400"
              }
              onClick={() => setView(k)}
              type="button"
            >
              {l}
            </button>
          ))}
          <div className="ml-auto text-sm text-slate-500">ADR-0017</div>
        </nav>

        {view === "settings" && <SettingsView me={me} />}
        {view === "artifacts" && <ArtifactPanel apiBase={API} token={token} />}

        {view === "chat" && (
          <div>
            <AgentPicker
              agentId={agentId}
              agents={agents}
              onChange={setAgentId}
            />
            <div className="mb-2 flex flex-wrap items-center gap-2">
              <span className="text-sm text-slate-400">Conversations</span>
              <button
                className="text-sm text-emerald-400"
                onClick={async () => {
                  const ctx = crypto.randomUUID();
                  const r = await authed("/webui/api/conversations", {
                    method: "POST",
                    body: JSON.stringify({
                      project_id: selectedProject,
                      context_id: ctx,
                      label: "chat",
                    }),
                  });
                  if (r?.ok) {
                    void refreshConvs();
                    const c = (await r.json()) as Conversation;
                    setConv(c);
                  }
                }}
                type="button"
              >
                + New
              </button>
            </div>
            <select
              className="mb-4 w-full max-w-md rounded border border-slate-700 bg-slate-900 px-2 py-1"
              onChange={(e) => {
                const c = conversations.find((x) => x.id === e.target.value) ?? null;
                setConv(c);
              }}
              value={conv?.id ?? ""}
            >
              <option value="">(none)</option>
              {conversations.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.label} ({c.id.slice(0, 8)}…)
                </option>
              ))}
            </select>
            {lastError && (
              <p className="mb-2 text-rose-400">
                stream error: {lastError}
              </p>
            )}
            <label className="mb-1 block text-sm text-slate-400" htmlFor="msg">
              Message
            </label>
            <textarea
              className="mb-2 w-full min-h-24 max-w-2xl rounded border border-slate-700 bg-slate-900 px-3 py-2 text-sm"
              id="msg"
              onChange={(e) => setMessage(e.target.value)}
              value={message}
            />
            <div className="mb-2 flex gap-2">
              <button
                className="rounded bg-slate-700 px-3 py-1"
                onClick={() => void startChat()}
                type="button"
              >
                Send (message/stream)
              </button>
            </div>
            <pre className="max-h-96 overflow-auto rounded border border-slate-800 p-2 text-xs text-slate-300">
              {out || "(no output yet)"}
            </pre>
          </div>
        )}
      </div>
    </div>
  );
}

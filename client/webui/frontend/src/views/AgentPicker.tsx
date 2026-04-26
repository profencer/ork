import type { AgentCard } from "./types";
export type { AgentCard } from "./types";

type Props = {
  agents: AgentCard[];
  agentId: string;
  onChange: (id: string) => void;
};

export function AgentPicker({ agents, agentId, onChange }: Props) {
  return (
    <div className="mb-4">
      <label className="text-sm text-slate-400" htmlFor="agent">
        Agent
      </label>
      <select
        className="ml-2 rounded border border-slate-700 bg-slate-900 px-2 py-1"
        id="agent"
        onChange={(e) => onChange(e.target.value)}
        value={agentId}
      >
        {agents.map((a) => (
          <option key={a.id} value={a.id}>
            {a.name}
          </option>
        ))}
      </select>
    </div>
  );
}


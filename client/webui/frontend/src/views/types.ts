/** Row from `GET /webui/api/agents` — `id` is the A2A path segment; `name` is display-only. */
export type AgentCard = {
  id: string;
  name: string;
  description: string;
  version: string;
};

-- ADR-0017: Web UI projects and conversation metadata (A2A context_id linkage).

CREATE TABLE webui_projects (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX webui_projects_tenant_idx ON webui_projects(tenant_id);

CREATE TABLE webui_conversations (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    project_id UUID REFERENCES webui_projects(id) ON DELETE SET NULL,
    context_id UUID NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX webui_conversations_tenant_idx ON webui_conversations(tenant_id);
CREATE INDEX webui_conversations_project_idx ON webui_conversations(project_id);

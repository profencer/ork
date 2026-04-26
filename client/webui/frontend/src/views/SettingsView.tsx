type Me = { user_id: string; tenant_id: string; scopes: string[] } | null;

type Props = { me: Me };

export function SettingsView({ me }: Props) {
  return (
    <div>
      <h2 className="mb-2 text-lg">Settings</h2>
      {me == null && <p className="text-slate-500">Loading /me…</p>}
      {me != null && (
        <dl className="space-y-1 text-sm">
          <div>
            <dt className="text-slate-500">user_id</dt>
            <dd className="font-mono">{me.user_id}</dd>
          </div>
          <div>
            <dt className="text-slate-500">tenant_id</dt>
            <dd className="font-mono">{me.tenant_id}</dd>
          </div>
          <div>
            <dt className="text-slate-500">scopes</dt>
            <dd className="font-mono">{me.scopes.join(", ") || "(none)"}</dd>
          </div>
        </dl>
      )}
      <p className="mt-4 text-slate-500 text-xs">
        LLM provider and API key rotation are CLI/admin operations (see ADR-0012).
      </p>
    </div>
  );
}

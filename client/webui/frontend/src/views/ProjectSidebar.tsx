import { useState } from "react";

type Project = { id: string; label: string };

type Props = {
  projects: Project[];
  selected: string | null;
  onSelect: (id: string | null) => void;
  onNew: (label: string) => void;
  onDelete: (id: string) => void;
};

export function ProjectSidebar({
  projects,
  selected,
  onSelect,
  onNew,
  onDelete,
}: Props) {
  const [label, setLabel] = useState("");
  return (
    <aside className="w-56 shrink-0 border-r border-slate-800 pr-2">
      <h2 className="mb-2 font-medium">Projects</h2>
      <div className="mb-2 flex gap-1">
        <input
          className="w-full min-w-0 rounded border border-slate-700 bg-slate-900 px-2 py-1 text-sm"
          onChange={(e) => setLabel(e.target.value)}
          placeholder="new label"
          value={label}
        />
        <button
          className="shrink-0 rounded bg-slate-700 px-2 py-1 text-sm"
          onClick={() => {
            if (label) {
              onNew(label);
              setLabel("");
            }
          }}
          type="button"
        >
          +
        </button>
      </div>
      <ul className="space-y-1 text-sm">
        <li>
          <button
            className={selected == null ? "text-emerald-400" : "text-slate-400"}
            onClick={() => onSelect(null)}
            type="button"
          >
            All
          </button>
        </li>
        {projects.map((p) => (
          <li className="flex items-center justify-between gap-1" key={p.id}>
            <button
              className={
                selected === p.id ? "text-left text-emerald-400" : "text-left"
              }
              onClick={() => onSelect(p.id)}
              type="button"
            >
              {p.label}
            </button>
            <button
              className="text-rose-400"
              onClick={() => onDelete(p.id)}
              type="button"
            >
              ×
            </button>
          </li>
        ))}
      </ul>
    </aside>
  );
}

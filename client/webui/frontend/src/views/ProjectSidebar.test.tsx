import { render, screen } from "@testing-library/react";
import "@testing-library/jest-dom";
import { expect, it, vi } from "vitest";
import { ProjectSidebar } from "./ProjectSidebar";

it("lists projects", () => {
  render(
    <ProjectSidebar
      projects={[{ id: "p1", label: "Alpha" }]}
      selected={null}
      onSelect={vi.fn()}
      onNew={vi.fn()}
      onDelete={vi.fn()}
    />,
  );
  expect(screen.getByText("Projects")).toBeInTheDocument();
  expect(screen.getByText("Alpha")).toBeInTheDocument();
});

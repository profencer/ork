import { render, screen } from "@testing-library/react";
import "@testing-library/jest-dom";
import { expect, it } from "vitest";
import { SettingsView } from "./SettingsView";

it("renders tenant fields from /me", () => {
  render(
    <SettingsView
      me={{ user_id: "u1", tenant_id: "t1", scopes: ["a2a"] }}
    />,
  );
  expect(screen.getByText("Settings")).toBeInTheDocument();
  expect(screen.getByText("u1")).toBeInTheDocument();
});

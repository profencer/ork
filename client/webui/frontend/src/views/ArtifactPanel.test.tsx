import { render, screen } from "@testing-library/react";
import "@testing-library/jest-dom";
import { expect, it } from "vitest";
import { ArtifactPanel } from "./ArtifactPanel";

it("shows artifact fetch UI", () => {
  render(<ArtifactPanel token="t" apiBase="http://x" />);
  expect(screen.getByPlaceholderText(/fs:/i)).toBeInTheDocument();
});

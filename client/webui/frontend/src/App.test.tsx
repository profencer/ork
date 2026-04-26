import { render, screen } from "@testing-library/react";
import "@testing-library/jest-dom";
import { expect, it } from "vitest";
import { App } from "./App";

it("asks for a JWT when no token in localStorage", () => {
  localStorage.removeItem("ork.webui.jwt");
  render(<App />);
  expect(screen.getByText(/ork Web UI/i)).toBeInTheDocument();
  expect(screen.getByPlaceholderText(/JWT/i)).toBeInTheDocument();
});

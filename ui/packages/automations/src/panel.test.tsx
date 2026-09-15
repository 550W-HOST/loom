// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AutomationsPanel } from "../app.js";
import { createUnavailableAutomationsClient } from "./client.js";

afterEach(cleanup);

describe("Automations panel boundary", () => {
  it("renders an explicit error when the loom client is unavailable", async () => {
    const navigation = {
      toCompose: vi.fn(),
      toThread: vi.fn(),
      toPanel: vi.fn(),
    };

    render(
      <AutomationsPanel
        subPath=""
        client={createUnavailableAutomationsClient()}
        navigation={navigation}
      />,
    );

    expect(await screen.findByText("Couldn't load automations.")).toBeTruthy();
    expect(navigation.toCompose).not.toHaveBeenCalled();
    expect(navigation.toThread).not.toHaveBeenCalled();
    expect(navigation.toPanel).not.toHaveBeenCalled();
  });
});

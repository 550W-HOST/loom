import { describe, expect, it } from "vitest";
import {
  createEmptyFixedPanelTabsState,
  FIXED_PANEL_TABS_STATE_STORAGE_VERSION,
  parseFixedPanelTabsState,
} from "../src/index";

describe("source-port fixed panel persistence", () => {
  it("prunes removed browser and generic plugin tabs", () => {
    const storedValue = JSON.stringify({
      version: FIXED_PANEL_TABS_STATE_STORAGE_VERSION,
      secondary: {
        tabs: [
          {
            id: "browser:one:none",
            kind: "browser",
            environmentId: null,
            title: null,
            url: "https://example.com",
          },
          {
            id: "plugin-page-fixed:one:none",
            kind: "plugin-page-fixed",
            fixedTabId: "one",
            pageId: "page",
            pluginId: "example",
          },
          {
            id: "plugin-panel:one:none",
            kind: "plugin-panel",
            actionId: "open",
            paramsJson: null,
            pluginId: "example",
            title: "Example",
          },
          { id: "thread-info:thread-info:none", kind: "thread-info" },
        ],
        activeTabId: "plugin-panel:one:none",
        isOpen: true,
      },
      lastUsedAt: 100,
    });

    const state = parseFixedPanelTabsState({
      initialValue: createEmptyFixedPanelTabsState(),
      now: 100,
      storedValue,
    });

    expect(state.secondary.tabs).toEqual([
      { id: "thread-info:thread-info:none", kind: "thread-info" },
    ]);
    expect(state.secondary.activeTabId).toBe("thread-info:thread-info:none");
  });
});

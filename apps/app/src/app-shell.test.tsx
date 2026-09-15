import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { App, PRODUCT_NAV_ITEMS } from "./App.js";

describe("product app shell", () => {
  it("exposes the supported workspace sections in navigation order", () => {
    expect(PRODUCT_NAV_ITEMS.map((item) => item.id)).toEqual([
      "overview",
      "threads",
      "projects",
      "environments",
      "settings",
    ]);
  });

  it("renders a non-empty first-viewport shell without server data", () => {
    const markup = renderToStaticMarkup(<App />);
    expect(markup).toContain('data-testid="product-app"');
    expect(markup).toContain("What are you working on?");
    expect(markup).toContain("No threads yet");
    expect(markup).toContain("No projects connected");
  });
});

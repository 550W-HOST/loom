import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const appRoot = path.resolve(import.meta.dirname, "../..");

function read(relativePath: string): string {
  return fs.readFileSync(path.join(appRoot, relativePath), "utf8");
}

function sourceFiles(directory: string): string[] {
  return fs.readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const absolutePath = path.join(directory, entry.name);
    if (entry.isDirectory()) return sourceFiles(absolutePath);
    return /\.[cm]?[jt]sx?$/u.test(entry.name) &&
      !/\.(?:test|stories)\.[cm]?[jt]sx?$/u.test(entry.name)
      ? [absolutePath]
      : [];
  });
}

describe("loom product composition", () => {
  it("keeps the generic plugin runtime outside the product entrypoint", () => {
    const packageJson = JSON.parse(read("package.json")) as {
      dependencies?: Record<string, string>;
    };
    const appSource = read("src/App.tsx");

    expect(packageJson.dependencies).not.toHaveProperty("@get-bb/plugin-sdk");
    expect(appSource).not.toContain("usePluginFrontendBoot");
    expect(appSource).not.toContain("PluginAppOverlays");
    expect(appSource).not.toContain("PluginsView");
    expect(appSource).not.toContain("SkillsView");
    const forbiddenImports = sourceFiles(path.join(appRoot, "src")).filter(
      (file) => fs.readFileSync(file, "utf8").includes("@get-bb/plugin-sdk"),
    );
    expect(forbiddenImports).toEqual([]);
  });

  it("keeps browser-safe first-party Settings while excluding generic surfaces", () => {
    const settingsSource = read("src/views/SettingsView.tsx");
    const navigationSource = read("src/components/settings/settings-nav.tsx");

    expect(settingsSource).toContain("GeneralSettingsSection");
    expect(settingsSource).toContain("ProvidersSettingsSection");
    expect(settingsSource).toContain("AppearanceSettingsSection");
    expect(settingsSource).toContain("KeyboardSettingsSection");
    expect(settingsSource).not.toContain("PluginsOverview");
    expect(settingsSource).not.toContain("BrowserSettingsSection");
    expect(settingsSource).not.toContain("MarketplacesSettingsSection");
    expect(navigationSource).toContain('"browser"');
    expect(navigationSource).toContain('"plugins"');
    expect(navigationSource).toContain('"marketplaces"');
  });

  it("keeps Automations as a direct first-party product route", () => {
    const appSource = read("src/App.tsx");
    const automationsSource = read("src/views/AutomationsView.tsx");

    expect(appSource).toContain("AUTOMATIONS_ROUTE_PATH");
    expect(appSource).toContain("AUTOMATION_DETAIL_ROUTE_PATH");
    expect(appSource).toContain("<AutomationsView />");
    expect(automationsSource).toContain("AutomationsPanel");
    expect(automationsSource).toContain("createUnavailableAutomationsClient");
    expect(automationsSource).toContain("getThreadRoutePath");
  });

  it("redirects unsupported generic product routes to the composer", () => {
    const appSource = read("src/App.tsx");

    for (const route of [
      "/plugins/*",
      "/skills/*",
      "/extensions/*",
      "/tools/*",
    ]) {
      expect(appSource).toContain(`path=\"${route}\"`);
    }
  });
});

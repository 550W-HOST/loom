import { matchPath } from "react-router-dom";
import {
  REGISTRY_SKILL_DETAIL_ROUTE_PATH,
  REGISTRY_SKILLS_ROUTE_PATH,
  ROUTE_PATTERNS,
  SKILL_DETAIL_ROUTE_PATH,
  SKILLS_ROUTE_PATH,
  TOOLS_ROUTE_PATH,
  stripRoutePathSuffix,
} from "@bb/client-core";

export {
  APP_ROOT_ROUTE_PATH,
  AUTH_CALLBACK_ROUTE_PATH,
  SETTINGS_ROUTE_PATH,
  SETTINGS_SECTION_ROUTE_PATH,
  SETTINGS_MACHINE_ROUTE_PATH,
  SETTINGS_PROJECT_ROUTE_PATH,
  SKILLS_ROUTE_PATH,
  SKILL_DETAIL_ROUTE_PATH,
  REGISTRY_SKILLS_ROUTE_PATH,
  REGISTRY_SKILL_DETAIL_ROUTE_PATH,
  TOOLS_ROUTE_PATH,
  TOOLS_SKILLS_ROUTE_PATH,
  TOOLS_SKILL_DETAIL_ROUTE_PATH,
  LEGACY_TOOLS_SKILL_DETAIL_ROUTE_PATH,
  TOOLS_REGISTRY_SKILLS_ROUTE_PATH,
  TOOLS_REGISTRY_SKILL_DETAIL_ROUTE_PATH,
  LEGACY_TOOLS_PREFIX_ROUTE_PATH,
  LEGACY_TOOLS_SPLAT_ROUTE_PATH,
  LEGACY_TOOLS_AUTOMATIONS_ROUTE_PATH,
  LEGACY_TOOLS_AUTOMATION_BROWSE_ROUTE_PATH,
  LEGACY_TOOLS_AUTOMATION_DETAIL_ROUTE_PATH,
  LEGACY_TOOLS_AUTOMATION_EDIT_ROUTE_PATH,
  LEGACY_SKILLS_ROUTE_PATH,
  LEGACY_AUTOMATIONS_ROUTE_PATH,
  LEGACY_AUTOMATION_DETAIL_ROUTE_PATH,
  AUTOMATIONS_ROUTE_PATH,
  AUTOMATIONS_BROWSE_ROUTE_PATH,
  AUTOMATION_DETAIL_ROUTE_PATH,
  AUTOMATION_EDIT_ROUTE_PATH,
  LEGACY_PROJECT_COMPOSE_ROUTE_PATH,
  PROJECTLESS_ARCHIVED_ROUTE_PATH,
  LEGACY_PROJECT_SETTINGS_ROUTE_PATH,
  PROJECT_ARCHIVED_ROUTE_PATH,
  isProjectlessProjectId,
  getRootComposeRoutePath,
  getLegacyProjectComposeRoutePath,
  getProjectComposeRoutePath,
  getSettingsRoutePath,
  getSettingsMachineRoutePath,
  getSettingsProjectRoutePath,
  getSkillsRoutePath,
  getRegistrySkillsRoutePath,
  getSkillDetailRoutePath,
  getRegistrySkillDetailRoutePath,
  getAutomationsRoutePath,
  getAutomationDetailRoutePath,
  getAutomationEditRoutePath,
  getThreadRoutePath,
} from "@bb/client-core";
export type { ThreadRoutePathArgs } from "@bb/client-core";

// Compatibility-only paths for persisted legacy navigation. The product router
// does not register these generic plugin surfaces.
export const SETTINGS_PLUGINS_ROUTE_PATH = "/settings/plugins";
export const SETTINGS_PLUGIN_ROUTE_PATH = "/settings/plugins/:pluginId";
export const PLUGINS_ROUTE_PATH = "/plugins";
export const PLUGIN_DETAIL_ROUTE_PATH = "/plugins/:pluginId";
export const TOOLS_PLUGINS_ROUTE_PATH = "/extensions/plugins";
export const TOOLS_PLUGIN_BROWSE_ROUTE_PATH = "/extensions/plugins/browse";
export const TOOLS_PLUGIN_DETAIL_ROUTE_PATH = "/extensions/plugins/:pluginId";
export const PLUGIN_PANEL_ROUTE_PATH = "/plugins/:pluginId/:panelPath/*";
export const AUTOMATIONS_PLUGIN_ID = "automations";
export const AUTOMATIONS_PLUGIN_PANEL_PATH = "automations";

export function getPluginPanelRoutePluginId(pathname: string): string | null {
  return matchPath(PLUGIN_PANEL_ROUTE_PATH, pathname)?.params.pluginId ?? null;
}

export function getPluginsRoutePath(): string {
  return PLUGINS_ROUTE_PATH;
}

interface PluginDetailRoutePathArgs {
  pluginId: string;
  view?: "installed";
}

export function getPluginDetailRoutePath({
  pluginId,
  view,
}: PluginDetailRoutePathArgs): string {
  const encoded = encodeURIComponent(pluginId);
  return view === "installed"
    ? `${SETTINGS_PLUGINS_ROUTE_PATH}/${encoded}?view=installed`
    : `${PLUGINS_ROUTE_PATH}/${encoded}`;
}

export function getPluginConfigurationRoutePath({
  pluginId,
}: PluginDetailRoutePathArgs): string {
  return `${SETTINGS_PLUGINS_ROUTE_PATH}/${encodeURIComponent(pluginId)}`;
}

interface PluginPanelRoutePathArgs {
  pluginId: string;
  path: string;
  subPath?: string;
}

export function getPluginPanelRoutePath({
  pluginId,
  path,
  subPath,
}: PluginPanelRoutePathArgs): string {
  const root = `/plugins/${encodeURIComponent(pluginId)}/${encodeURIComponent(path)}`;
  if (!subPath) return root;
  const encoded = subPath
    .split("/")
    .filter(Boolean)
    .map((segment) => encodeURIComponent(segment))
    .join("/");
  return encoded ? `${root}/${encoded}` : root;
}

interface IsRoutePathArgs {
  path: string;
}

interface ResolveRouteHrefArgs {
  currentOrigin: string;
  href: string;
}

interface RouteHrefResolution {
  path: string;
}

export function isToolsRoutePath(pathname: string): boolean {
  return (
    isSkillsRoutePath(pathname) ||
    pathname === TOOLS_ROUTE_PATH ||
    matchPath(`${TOOLS_ROUTE_PATH}/*`, pathname) !== null
  );
}

export function isPluginsRoutePath(_pathname: string): boolean {
  return false;
}

export function isSkillsRoutePath(pathname: string): boolean {
  return (
    matchPath(SKILLS_ROUTE_PATH, pathname) !== null ||
    matchPath(REGISTRY_SKILLS_ROUTE_PATH, pathname) !== null ||
    matchPath(SKILL_DETAIL_ROUTE_PATH, pathname) !== null ||
    matchPath(REGISTRY_SKILL_DETAIL_ROUTE_PATH, pathname) !== null
  );
}

const ABSOLUTE_HTTP_URL_PATTERN = /^https?:\/\//iu;

export function isRoutePath({ path }: IsRoutePathArgs): boolean {
  const pathname = stripRoutePathSuffix(path);
  return ROUTE_PATTERNS.some(
    (pattern) => matchPath(pattern, pathname) !== null,
  );
}

export function resolveRouteHref({
  currentOrigin,
  href,
}: ResolveRouteHrefArgs): RouteHrefResolution | null {
  if (
    href.length === 0 ||
    href.startsWith("//") ||
    (!href.startsWith("/") && !ABSOLUTE_HTTP_URL_PATTERN.test(href))
  ) {
    return null;
  }

  let url: URL;
  try {
    url = new URL(href, currentOrigin);
  } catch {
    return null;
  }

  if (url.origin !== currentOrigin || !isRoutePath({ path: url.pathname })) {
    return null;
  }

  return { path: `${url.pathname}${url.search}${url.hash}` };
}

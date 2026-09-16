import { useMemo } from "react";
import { matchPath, useLocation } from "react-router-dom";
import { useHostDaemon, useLocalHostDaemonAccess } from "@/hooks/useHostDaemon";
import {
  SETTINGS_MACHINE_ROUTE_PATH,
  SETTINGS_PROJECT_ROUTE_PATH,
  SETTINGS_SECTION_ROUTE_PATH,
} from "@/lib/route-paths";
import {
  isSettingsSectionId,
  SETTINGS_NAV_SECTIONS,
  type SettingsNavSection,
  type SettingsSectionId,
} from "./settings-sections";

const UNSUPPORTED_SETTINGS_SECTIONS = new Set<SettingsSectionId>([
  "browser",
  "plugins",
  "marketplaces",
]);

export interface SettingsNavState {
  activeSection: SettingsSectionId | null;
  hasUnknownSection: boolean;
  activePluginId: null;
  pluginEntries: readonly never[];
  sections: readonly SettingsNavSection[];
}

export function useSettingsNavSections(
  _fileOpeners: readonly unknown[] = [],
): readonly SettingsNavSection[] {
  const { hasDaemon } = useHostDaemon();
  const { accessState } = useLocalHostDaemonAccess();

  return useMemo(
    () =>
      SETTINGS_NAV_SECTIONS.filter(
        (section) =>
          !UNSUPPORTED_SETTINGS_SECTIONS.has(section.id) &&
          (section.id !== "files" ||
            hasDaemon ||
            accessState !== "unavailable"),
      ),
    [accessState, hasDaemon],
  );
}

export function useSettingsNavState(): SettingsNavState {
  const location = useLocation();
  const sections = useSettingsNavSections();
  const sectionMatch = matchPath(
    SETTINGS_SECTION_ROUTE_PATH,
    location.pathname,
  );
  const machineMatch = matchPath(
    SETTINGS_MACHINE_ROUTE_PATH,
    location.pathname,
  );
  const projectMatch = matchPath(
    SETTINGS_PROJECT_ROUTE_PATH,
    location.pathname,
  );
  const sectionParam = sectionMatch?.params.section;
  const sectionSupported =
    sectionParam !== undefined &&
    isSettingsSectionId(sectionParam) &&
    !UNSUPPORTED_SETTINGS_SECTIONS.has(sectionParam);
  const hasUnknownSection = sectionParam !== undefined && !sectionSupported;
  const activeSection: SettingsSectionId | null =
    machineMatch !== null
      ? "machines"
      : projectMatch !== null
        ? "projects"
        : sectionSupported
          ? sectionParam
          : "general";

  return {
    activePluginId: null,
    activeSection,
    hasUnknownSection,
    pluginEntries: [],
    sections,
  };
}

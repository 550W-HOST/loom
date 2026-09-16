import { lazy, Suspense, useEffect } from "react";
import {
  Navigate,
  Route,
  Routes,
  useLocation,
  useParams,
} from "react-router-dom";
import { AppLayout } from "./components/layout/AppLayout";
import { QuickCreateProjectProvider } from "./hooks/useQuickCreateProject";
import { RouteNavigationProvider } from "./components/ui/app-route-anchor";
import { RouteNavigationIndicator } from "./components/ui/route-navigation-indicator";
import { UiPreferencesSync } from "@/lib/ui-preferences/UiPreferencesSync";
import { useAppTheme } from "./hooks/useAppTheme";
import { useFaviconColorSync } from "./lib/favicon-color-preference";
import { markRouteContentPainted } from "./lib/route-content-paint";
import {
  AUTOMATION_DETAIL_ROUTE_PATH,
  AUTOMATION_EDIT_ROUTE_PATH,
  AUTOMATIONS_BROWSE_ROUTE_PATH,
  AUTOMATIONS_ROUTE_PATH,
  LEGACY_AUTOMATION_DETAIL_ROUTE_PATH,
  LEGACY_AUTOMATIONS_ROUTE_PATH,
  LEGACY_PROJECT_SETTINGS_ROUTE_PATH,
  LEGACY_TOOLS_AUTOMATION_BROWSE_ROUTE_PATH,
  LEGACY_TOOLS_AUTOMATION_DETAIL_ROUTE_PATH,
  LEGACY_TOOLS_AUTOMATION_EDIT_ROUTE_PATH,
  LEGACY_TOOLS_AUTOMATIONS_ROUTE_PATH,
  PROJECT_ARCHIVED_ROUTE_PATH,
  PROJECTLESS_ARCHIVED_ROUTE_PATH,
  SETTINGS_MACHINE_ROUTE_PATH,
  SETTINGS_PROJECT_ROUTE_PATH,
  SETTINGS_ROUTE_PATH,
  SETTINGS_SECTION_ROUTE_PATH,
  getAutomationDetailRoutePath,
  getAutomationEditRoutePath,
  getAutomationsRoutePath,
  getSettingsProjectRoutePath,
  getSettingsRoutePath,
} from "./lib/route-paths";
import { AppCommandProvider } from "./components/commands/AppCommandProvider";
import { RouteLoadingSkeleton } from "./components/ui/route-loading-skeleton";

const SettingsView = lazy(() =>
  import("./views/SettingsView").then((module) => ({
    default: module.SettingsView,
  })),
);
const ProjectDetailSettingsView = lazy(() =>
  import("./views/ProjectDetailSettingsView").then((module) => ({
    default: module.ProjectDetailSettingsView,
  })),
);
const MachineSettingsView = lazy(() =>
  import("./views/MachineSettingsView").then((module) => ({
    default: module.MachineSettingsView,
  })),
);
const AutomationsView = lazy(() =>
  import("./views/AutomationsView").then((module) => ({
    default: module.AutomationsView,
  })),
);
const splitWorkspaceRouteModule = import("./views/SplitWorkspaceRoute");
splitWorkspaceRouteModule.catch(() => {});
const SplitWorkspaceRoute = lazy(() => splitWorkspaceRouteModule);

function LegacyProjectSettingsRedirect() {
  const { projectId } = useParams<{ projectId: string }>();
  const { search, hash } = useLocation();
  return (
    <Navigate
      to={{
        pathname: projectId
          ? getSettingsProjectRoutePath(projectId)
          : getSettingsRoutePath("projects"),
        search,
        hash,
      }}
      replace
    />
  );
}

function LegacyAutomationDetailRedirect() {
  const location = useLocation();
  const { projectId, automationId } = useParams<{
    projectId?: string;
    automationId?: string;
  }>();
  if (!projectId || !automationId) {
    return <Navigate to={getAutomationsRoutePath()} replace />;
  }
  return (
    <Navigate
      to={
        location.pathname.endsWith("/edit")
          ? getAutomationEditRoutePath({ projectId, automationId })
          : getAutomationDetailRoutePath({ projectId, automationId })
      }
      replace
    />
  );
}

function LegacyAutomationCollectionRedirect() {
  const location = useLocation();
  const browse =
    location.pathname.endsWith("/browse") ||
    new URLSearchParams(location.search).get("view") === "browse";
  return (
    <Navigate
      to={browse ? AUTOMATIONS_BROWSE_ROUTE_PATH : AUTOMATIONS_ROUTE_PATH}
      replace
    />
  );
}

function hashTargetId(hash: string): string | null {
  if (hash.length <= 1) return null;
  try {
    return decodeURIComponent(hash.slice(1));
  } catch {
    return hash.slice(1);
  }
}

const HASH_NAVIGATION_WAIT_MS = 2_000;

export function HashNavigationScroll() {
  const location = useLocation();
  useEffect(() => {
    const targetId = hashTargetId(location.hash);
    if (targetId === null) return;

    const scrollToTarget = (): boolean => {
      const target = document.getElementById(targetId);
      if (target === null) return false;
      if (target.tabIndex < 0 && !target.hasAttribute("tabindex")) {
        target.tabIndex = -1;
      }
      target.focus({ preventScroll: true });
      target.scrollIntoView({ block: "start", inline: "nearest" });
      return true;
    };

    if (scrollToTarget()) return;

    let observer: MutationObserver | null = null;
    let timeoutId: number | null = null;
    const stopWaiting = () => {
      observer?.disconnect();
      observer = null;
      if (timeoutId !== null) {
        window.clearTimeout(timeoutId);
        timeoutId = null;
      }
    };
    observer = new MutationObserver(() => {
      if (scrollToTarget()) stopWaiting();
    });
    observer.observe(document.body, { childList: true, subtree: true });
    timeoutId = window.setTimeout(stopWaiting, HASH_NAVIGATION_WAIT_MS);
    return stopWaiting;
  }, [location.hash, location.key]);
  return null;
}

function AppRoutes() {
  return (
    <AppLayout>
      <Suspense fallback={null}>
        <Routes>
          <Route path={SETTINGS_ROUTE_PATH} element={<SettingsView />} />
          <Route
            path={SETTINGS_SECTION_ROUTE_PATH}
            element={<SettingsView />}
          />
          <Route
            path={SETTINGS_MACHINE_ROUTE_PATH}
            element={<MachineSettingsView />}
          />
          <Route
            path={SETTINGS_PROJECT_ROUTE_PATH}
            element={<ProjectDetailSettingsView />}
          />
          <Route
            path={LEGACY_PROJECT_SETTINGS_ROUTE_PATH}
            element={<LegacyProjectSettingsRedirect />}
          />
          <Route
            path={PROJECT_ARCHIVED_ROUTE_PATH}
            element={<Navigate to={getSettingsRoutePath("archived")} replace />}
          />
          <Route
            path={PROJECTLESS_ARCHIVED_ROUTE_PATH}
            element={<Navigate to={getSettingsRoutePath("archived")} replace />}
          />
          <Route
            path={LEGACY_TOOLS_AUTOMATIONS_ROUTE_PATH}
            element={<LegacyAutomationCollectionRedirect />}
          />
          <Route
            path={LEGACY_TOOLS_AUTOMATION_BROWSE_ROUTE_PATH}
            element={<LegacyAutomationCollectionRedirect />}
          />
          <Route
            path={LEGACY_TOOLS_AUTOMATION_DETAIL_ROUTE_PATH}
            element={<LegacyAutomationDetailRedirect />}
          />
          <Route
            path={LEGACY_TOOLS_AUTOMATION_EDIT_ROUTE_PATH}
            element={<LegacyAutomationDetailRedirect />}
          />
          <Route
            path={LEGACY_AUTOMATIONS_ROUTE_PATH}
            element={<AutomationsView />}
          />
          <Route
            path={LEGACY_AUTOMATION_DETAIL_ROUTE_PATH}
            element={<AutomationsView />}
          />
          <Route path={AUTOMATIONS_ROUTE_PATH} element={<AutomationsView />} />
          <Route
            path={AUTOMATIONS_BROWSE_ROUTE_PATH}
            element={<AutomationsView />}
          />
          <Route
            path={AUTOMATION_DETAIL_ROUTE_PATH}
            element={<AutomationsView />}
          />
          <Route
            path={AUTOMATION_EDIT_ROUTE_PATH}
            element={<AutomationsView />}
          />
          <Route path="/plugins/*" element={<Navigate to="/" replace />} />
          <Route path="/skills/*" element={<Navigate to="/" replace />} />
          <Route path="/extensions/*" element={<Navigate to="/" replace />} />
          <Route path="/tools/*" element={<Navigate to="/" replace />} />
          <Route
            path="*"
            element={
              <Suspense
                fallback={<RouteLoadingSkeleton isBoundedPane={false} />}
              >
                <SplitWorkspaceRoute />
              </Suspense>
            }
          />
        </Routes>
        <RouteContentPaintSignal />
      </Suspense>
    </AppLayout>
  );
}

function RouteContentPaintSignal() {
  useEffect(() => {
    markRouteContentPainted();
  }, []);
  return null;
}

export function App() {
  useAppTheme();
  useFaviconColorSync();
  return (
    <QuickCreateProjectProvider>
      <AppCommandProvider>
        <RouteNavigationProvider>
          <RouteNavigationIndicator />
          <HashNavigationScroll />
          <UiPreferencesSync />
          <AppRoutes />
        </RouteNavigationProvider>
      </AppCommandProvider>
    </QuickCreateProjectProvider>
  );
}

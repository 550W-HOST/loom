import { type MouseEvent as ReactMouseEvent, type ReactNode } from "react";
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { flushSync } from "react-dom";
import { atom, useAtom, useAtomValue, useStore } from "jotai";
import { atomWithStorage } from "jotai/utils";
import { Link, matchPath, useLocation, useNavigate } from "react-router-dom";
import type { ProjectResponse } from "@bb/server-contract";
import { Icon } from "@bb/shared-ui/icon";
import { RESOURCE_ROUTE_LABEL_EVENT } from "@bb/shared-ui/resource-route-label";
import {
  SidebarInset,
  SidebarProvider,
  SidebarTrigger,
} from "@/components/ui/sidebar.js";
import {
  ThreadTitleMentionResourcesProvider,
  useSidebarThreadTitleMentionResources,
} from "@/components/thread/ThreadTitleMentions";
import { AppCommandShortcutHint } from "@/components/commands/AppCommandShortcutHint";
import { CommandPalette } from "@/components/commands/CommandPalette";
import { NotificationCenter } from "@/components/notifications/NotificationCenter";
import { resolveAutomationBreadcrumbs } from "@/components/tools/tools-navigation";
import { AppBreadcrumbs } from "./AppBreadcrumbs";
import { resourceRouteLabelAtom } from "./resourceRouteLabelAtom";
import { AppPageHeader, HEADER_ICON_BUTTON_CLASS } from "./AppPageHeader";
import { stripProjectThreads } from "@/hooks/queries/project-queries";
import { useSidebarNavigation } from "@/hooks/queries/sidebar-navigation-query";
import {
  didThreadDetailBootstrapRefreshAfterMount,
  getLatestPendingInteraction,
  useThread,
  useThreadDetailBootstrap,
  useThreadPendingInteractions,
} from "@/hooks/queries/thread-queries";
import { useRouteState } from "@/hooks/useRouteState";
import { getThreadDisplayTitle } from "@/lib/thread-title";
import { cn } from "@bb/shared-ui/lib/utils";
import { APP_OVERLAY_LAYER } from "@/components/ui/app-overlay-layers";
import {
  getCompactSecondaryPanelPresentation,
  subscribeCompactSecondaryPanelShelfShowing,
} from "@/components/ui/secondary-panel-shelf-visibility";
import { ProjectPathDialog } from "@/components/dialogs/ProjectPathDialog";
import { ProjectActionsMenu } from "@/components/project/ProjectActionsMenu";
import { ProjectActionsProvider } from "@/components/project/ProjectActionsProvider";
import { ThreadActionsProvider } from "@/components/thread/ThreadActionsProvider";
import { createLocalStorageSyncStorage } from "@/lib/browser-storage";
import {
  BROWSER_SIDEBAR_TRIGGER_INSET_CLASS,
  CHROME_ROW_CLASS,
} from "@/lib/bb-desktop";
import {
  getLegacyProjectComposeRoutePath,
  getSettingsProjectRoutePath,
  getRootComposeRoutePath,
  isProjectlessProjectId,
  SETTINGS_ROUTE_PATH,
} from "@/lib/route-paths";
import { useQuickCreateProjectController } from "@/hooks/useQuickCreateProject";
import { IframeDragGuardOverlay } from "@/lib/iframe-drag-guard";
import { useFaviconBadge } from "@/lib/favicon-color-preference";
import { shouldShowFaviconAttentionDot } from "./faviconAttentionDot";
import { AppLayoutSidebar } from "./AppLayoutSidebar";
import {
  useAppCommandHandler,
  useAppCommandShortcut,
} from "@/components/commands/AppCommandProvider";
import { useIsCompactViewport } from "@bb/shared-ui/hooks/use-compact-viewport";
import {
  shouldRestoreIOSViewportOnKeyboardDismissal,
  useMobileVisualViewportHeight,
} from "./useMobileVisualViewportHeight";
import { useAppSettingsRouteMemory } from "@/hooks/useAppSettingsRouteMemory";
import { useSetRootComposeProjectId } from "@/lib/root-compose-selection";
import { BackToAppCommandHandler } from "./BackToAppCommandHandler";

const SIDEBAR_WIDTH_KEY = "bb.sidebar.width";
const SIDEBAR_OPEN_KEY = "bb.sidebar.open";
const SIDEBAR_MIN_WIDTH = 240;
const SIDEBAR_MAX_WIDTH = 460;
const SIDEBAR_DEFAULT_WIDTH = 320;

function clampSidebarWidth(value: number) {
  return Math.min(SIDEBAR_MAX_WIDTH, Math.max(SIDEBAR_MIN_WIDTH, value));
}

const sidebarWidthStorage = createLocalStorageSyncStorage<number>({
  parse: (storedValue, initialValue) => {
    if (storedValue === null) {
      return initialValue;
    }
    const parsedValue = Number(storedValue);
    if (!Number.isFinite(parsedValue)) {
      return initialValue;
    }
    return clampSidebarWidth(parsedValue);
  },
  serialize: (value) => String(clampSidebarWidth(value)),
});
const sidebarWidthAtom = atomWithStorage<number>(
  SIDEBAR_WIDTH_KEY,
  SIDEBAR_DEFAULT_WIDTH,
  sidebarWidthStorage,
  { getOnInit: true },
);
const sidebarLiveWidthAtom = atom<number | null>(null);

const sidebarOpenStorage = createLocalStorageSyncStorage<boolean>({
  parse: (storedValue, initialValue) => {
    if (storedValue === "true") return true;
    if (storedValue === "false") return false;
    return initialValue;
  },
  serialize: (value) => String(value),
});
const sidebarOpenAtom = atomWithStorage<boolean>(
  SIDEBAR_OPEN_KEY,
  true,
  sidebarOpenStorage,
  { getOnInit: true },
);

interface SidebarStateBridgeProps {
  children: ReactNode;
}

type SidebarResizeMouseEvent = ReactMouseEvent<HTMLDivElement>;
type SidebarOpenChangeHandler = (open: boolean) => void;

function SidebarStateBridge({ children }: SidebarStateBridgeProps) {
  const [open, setOpen] = useAtom(sidebarOpenAtom);
  const sidebarWidth = useAtomValue(sidebarWidthAtom);
  const sidebarLiveWidth = useAtomValue(sidebarLiveWidthAtom);
  const handleOpenChange = useCallback<SidebarOpenChangeHandler>(
    (nextOpen) => {
      setOpen(nextOpen);
    },
    [setOpen],
  );
  useAppCommandHandler("sidebar.toggle", () => {
    handleOpenChange(!open);
    return true;
  });
  return (
    <SidebarProvider
      width={`${sidebarLiveWidth ?? sidebarWidth}px`}
      data-testid="app-layout-root"
      open={open}
      onOpenChange={handleOpenChange}
    >
      {children}
    </SidebarProvider>
  );
}

function resetSidebarResizeDocumentState(): void {
  document.body.classList.remove("sidebar-resizing");
}

function SidebarTriggerOverlay() {
  const isCompactViewport = useIsCompactViewport();
  const compactSecondaryPanelPresentation = useSyncExternalStore(
    subscribeCompactSecondaryPanelShelfShowing,
    getCompactSecondaryPanelPresentation,
    () => "closed",
  );
  const shortcut = useAppCommandShortcut("sidebar.toggle");
  if (isCompactViewport && compactSecondaryPanelPresentation !== "closed") {
    return null;
  }
  const triggerProps = {
    "aria-label": shortcut
      ? `Toggle sidebar (${shortcut.label})`
      : "Toggle sidebar",
    "aria-keyshortcuts": shortcut?.ariaKeyshortcuts,
  };
  return (
    <div
      data-testid="app-sidebar-trigger-overlay"
      style={{ zIndex: APP_OVERLAY_LAYER.sidebarTrigger }}
      className={cn(
        "fixed top-[env(safe-area-inset-top)] left-[env(safe-area-inset-left)]",
        CHROME_ROW_CLASS,
        BROWSER_SIDEBAR_TRIGGER_INSET_CLASS,
      )}
    >
      <SidebarTrigger {...triggerProps} />
      <AppCommandShortcutHint
        shortcut={shortcut}
        className="absolute left-full ml-1"
      />
    </div>
  );
}

const routeTitles: Record<string, { title: string }> = {
  "/": { title: "bb" },
  "/settings": { title: "Settings" },
  "/automations": { title: "Automations" },
};

function resolveRouteTitle(pathname: string): { title: string } | undefined {
  if (matchPath(`${SETTINGS_ROUTE_PATH}/*`, pathname)) {
    return routeTitles[SETTINGS_ROUTE_PATH];
  }
  return routeTitles[pathname];
}

interface AppHeaderProps {
  usesProjectChromeStyle: boolean;
  projectId?: string;
  project?: ProjectResponse;
  meta: {
    title: string;
    breadcrumbs?: Array<{ label: string; to?: string }>;
  };
}

function AppHeader({
  usesProjectChromeStyle,
  projectId,
  project,
  meta,
}: AppHeaderProps) {
  const headerBreadcrumbs = meta.breadcrumbs;
  const headerTitle =
    headerBreadcrumbs || usesProjectChromeStyle ? undefined : meta.title;

  const hasCenterContent = Boolean(headerBreadcrumbs) || Boolean(headerTitle);

  const center = headerBreadcrumbs ? (
    <div className="min-w-0 flex-1">
      <AppBreadcrumbs
        breadcrumbs={headerBreadcrumbs}
        usesDesktopChrome={false}
      />
    </div>
  ) : hasCenterContent ? (
    <div className="min-w-0 flex-1">
      {headerTitle ? (
        <p className="truncate text-sm font-semibold">{headerTitle}</p>
      ) : null}
    </div>
  ) : null;

  const actions =
    usesProjectChromeStyle &&
    projectId &&
    !isProjectlessProjectId(projectId) ? (
      <>
        <Link
          to={getSettingsProjectRoutePath(projectId)}
          className={cn(
            HEADER_ICON_BUTTON_CLASS,
            "inline-flex items-center justify-center transition-colors",
            "text-muted-foreground hover:bg-state-hover hover:text-foreground",
          )}
          aria-label="Project settings"
        >
          <Icon name="Settings" />
        </Link>
        {project ? (
          <ProjectActionsMenu
            project={project}
            triggerClassName={HEADER_ICON_BUTTON_CLASS}
          />
        ) : null}
      </>
    ) : null;

  return <AppPageHeader center={center} actions={actions} />;
}

interface AppLayoutProps {
  children: ReactNode;
}

export function AppLayout({ children }: AppLayoutProps) {
  const quickCreateProject = useQuickCreateProjectController();
  const isCompactViewport = useIsCompactViewport();
  const store = useStore();
  const contentShellRef = useRef<HTMLDivElement>(null);
  const restoreIOSViewportOnKeyboardDismissal = useMemo(
    () => shouldRestoreIOSViewportOnKeyboardDismissal(navigator),
    [],
  );
  useMobileVisualViewportHeight(
    contentShellRef,
    isCompactViewport,
    restoreIOSViewportOnKeyboardDismissal,
  );
  const location = useLocation();
  const { projectId, threadId, isThreadView, isArchivedView, isRootView } =
    useRouteState();
  const [resourceRouteLabel, setResourceRouteLabel] = useAtom(
    resourceRouteLabelAtom,
  );
  useEffect(() => {
    setResourceRouteLabel(null);
    function handleResourceRouteLabel(event: Event) {
      if (!(event instanceof CustomEvent)) return;
      const detail = event.detail;
      if (
        typeof detail !== "object" ||
        detail === null ||
        !("label" in detail) ||
        (typeof detail.label !== "string" && detail.label !== null)
      ) {
        return;
      }
      setResourceRouteLabel(detail.label);
    }
    window.addEventListener(
      RESOURCE_ROUTE_LABEL_EVENT,
      handleResourceRouteLabel,
    );
    return () => {
      window.removeEventListener(
        RESOURCE_ROUTE_LABEL_EVENT,
        handleResourceRouteLabel,
      );
    };
  }, [location.pathname, setResourceRouteLabel]);
  const navigate = useNavigate();
  const { appRoutePath, settingsRoutePath } = useAppSettingsRouteMemory();
  const setRootComposeProjectId = useSetRootComposeProjectId();
  useAppCommandHandler("thread.new", () => {
    if (projectId !== undefined) {
      setRootComposeProjectId(projectId);
    }
    void navigate(getRootComposeRoutePath(), {
      state: { focusPrompt: true },
    });
    return true;
  });
  useAppCommandHandler("settings.open", () => {
    void navigate(settingsRoutePath);
    return true;
  });
  useAppCommandHandler("settings.openServers", () => {
    void navigate(`${SETTINGS_ROUTE_PATH}/servers`);
    return true;
  });
  const archivedSectionId = isArchivedView
    ? new URLSearchParams(location.search).get("sectionId")
    : null;
  const isGlobalSettingsView =
    matchPath(`${SETTINGS_ROUTE_PATH}/*`, location.pathname) !== null;
  const backToAppRoutePath = isGlobalSettingsView ? appRoutePath : null;
  const sidebarNavigationQuery = useSidebarNavigation();
  const projects = useMemo(
    () => sidebarNavigationQuery.data?.projects.map(stripProjectThreads),
    [sidebarNavigationQuery.data],
  );
  const sidebarThreads = useMemo(() => {
    const sidebarNavigation = sidebarNavigationQuery.data;
    if (!sidebarNavigation) {
      return [];
    }
    return [
      ...sidebarNavigation.projects.flatMap((project) => project.threads),
      ...sidebarNavigation.personalProject.threads,
    ];
  }, [sidebarNavigationQuery.data]);
  const titleMentionResources = useSidebarThreadTitleMentionResources(
    sidebarNavigationQuery.data,
  );
  const threadDetailBootstrapQuery = useThreadDetailBootstrap(threadId ?? "", {
    enabled: isThreadView && Boolean(threadId),
    timelinePrefetch: isThreadView && Boolean(threadId),
  });
  const hasThreadDetailBootstrapSettled =
    threadDetailBootstrapQuery.isSuccess || threadDetailBootstrapQuery.isError;
  const [isSidebarResizing, setIsSidebarResizing] = useState(false);
  const startXRef = useRef(0);
  const startWidthRef = useRef(0);
  const liveWidthRef = useRef(0);
  const animationFrameRef = useRef<number | null>(null);
  const showHeader = !isThreadView && !isRootView;
  const project = projectId
    ? projects?.find((candidate) => candidate.id === projectId)
    : undefined;
  const archivedSectionName = archivedSectionId
    ? (sidebarNavigationQuery.data?.sections.find(
        (section) => section.id === archivedSectionId,
      )?.name ?? archivedSectionId)
    : null;
  const projectName = projectId ? project?.name : undefined;
  const projectLabel = projectName ?? (projectId ? projectId : undefined);
  const { data: thread } = useThread(threadId ?? "", {
    enabled:
      Boolean(threadId) && (!isThreadView || hasThreadDetailBootstrapSettled),
    refetchOnMount:
      isThreadView &&
      didThreadDetailBootstrapRefreshAfterMount(threadDetailBootstrapQuery)
        ? false
        : "always",
  });
  const threadDisplayTitle = thread
    ? getThreadDisplayTitle(thread)
    : threadId
      ? `Thread ${threadId.slice(0, 8)}`
      : "Thread";
  const automationBreadcrumbs = resolveAutomationBreadcrumbs(
    location.pathname,
    resourceRouteLabel,
  );
  const documentTitleBreadcrumbs = automationBreadcrumbs;
  const meta =
    automationBreadcrumbs !== null
      ? { title: "", breadcrumbs: automationBreadcrumbs }
      : isArchivedView && projectId
        ? isProjectlessProjectId(projectId)
          ? {
              title: "",
              breadcrumbs: [
                { label: "Threads", to: getRootComposeRoutePath() },
                ...(archivedSectionName
                  ? [{ label: archivedSectionName }]
                  : []),
                { label: "Archived" },
              ],
            }
          : {
              title: "",
              breadcrumbs: [
                {
                  label: projectLabel ?? projectId,
                  to: getLegacyProjectComposeRoutePath(projectId),
                },
                { label: "Archived" },
              ],
            }
        : projectId
          ? {
              title: projectLabel ?? projectId,
            }
          : (resolveRouteTitle(location.pathname) ?? { title: "" });

  const documentTitle = (() => {
    if (isThreadView) {
      return threadDisplayTitle;
    }
    if (documentTitleBreadcrumbs) {
      const sectionLabel = documentTitleBreadcrumbs[0]?.label ?? "BB";
      const pageLabel = documentTitleBreadcrumbs.at(-1)?.label ?? sectionLabel;
      return pageLabel === sectionLabel
        ? sectionLabel
        : `${pageLabel} · ${sectionLabel}`;
    }
    if (isArchivedView && projectId) {
      if (isProjectlessProjectId(projectId)) {
        return archivedSectionName
          ? `${archivedSectionName} · Archived`
          : "Threads · Archived";
      }
      return `${projectLabel ?? projectId} · Archived`;
    }
    if (projectId) {
      return projectLabel ?? projectId;
    }
    const routeTitle = resolveRouteTitle(location.pathname)?.title;
    return routeTitle && routeTitle.length > 0 ? routeTitle : "BB";
  })();
  const currentThreadPendingInteractionsQuery = useThreadPendingInteractions(
    threadId ?? "",
    { enabled: isThreadView && Boolean(threadId) },
  );
  const currentThreadHasPendingInteraction =
    getLatestPendingInteraction(currentThreadPendingInteractionsQuery.data) !==
    null;
  const faviconBadge = shouldShowFaviconAttentionDot({
    currentThreadHasPendingInteraction,
    currentThreadId: threadId,
    isThreadView,
    sidebarThreads,
    thread,
  })
    ? "unread"
    : "none";
  useFaviconBadge(faviconBadge);

  const handleResizeMouseDown = useCallback(
    (event: SidebarResizeMouseEvent) => {
      event.preventDefault();
      setIsSidebarResizing(true);
      startXRef.current = event.clientX;
      startWidthRef.current = store.get(sidebarWidthAtom);
      liveWidthRef.current = startWidthRef.current;
      document.body.classList.add("sidebar-resizing");
    },
    [store],
  );

  const finishSidebarResize = useCallback(() => {
    if (animationFrameRef.current !== null) {
      window.cancelAnimationFrame(animationFrameRef.current);
      animationFrameRef.current = null;
    }
    flushSync(() => {
      store.set(sidebarWidthAtom, liveWidthRef.current);
      store.set(sidebarLiveWidthAtom, null);
    });
    setIsSidebarResizing(false);
    resetSidebarResizeDocumentState();
  }, [store]);

  useEffect(() => {
    if (!isSidebarResizing) return;

    const applyLiveWidth = () => {
      animationFrameRef.current = null;
      flushSync(() => {
        store.set(sidebarLiveWidthAtom, liveWidthRef.current);
      });
    };

    const handleMouseMove = (event: MouseEvent) => {
      const delta = event.clientX - startXRef.current;
      liveWidthRef.current = clampSidebarWidth(startWidthRef.current + delta);
      if (animationFrameRef.current === null) {
        animationFrameRef.current =
          window.requestAnimationFrame(applyLiveWidth);
      }
    };

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        finishSidebarResize();
      }
    };

    window.addEventListener("mousemove", handleMouseMove);
    window.addEventListener("mouseup", finishSidebarResize);
    window.addEventListener("blur", finishSidebarResize);
    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.removeEventListener("mousemove", handleMouseMove);
      window.removeEventListener("mouseup", finishSidebarResize);
      window.removeEventListener("blur", finishSidebarResize);
      window.removeEventListener("keydown", handleKeyDown);
      if (animationFrameRef.current !== null) {
        window.cancelAnimationFrame(animationFrameRef.current);
        animationFrameRef.current = null;
      }
      store.set(sidebarLiveWidthAtom, null);
      resetSidebarResizeDocumentState();
    };
  }, [finishSidebarResize, isSidebarResizing, store]);

  useEffect(() => {
    if (typeof document === "undefined") return;
    document.title = documentTitle;
  }, [documentTitle]);

  return (
    <ProjectActionsProvider>
      <ThreadTitleMentionResourcesProvider {...titleMentionResources}>
        <ThreadActionsProvider>
          <SidebarStateBridge>
            {backToAppRoutePath !== null && !isSidebarResizing ? (
              <BackToAppCommandHandler routePath={backToAppRoutePath} />
            ) : null}
            <AppLayoutSidebar
              mode={isGlobalSettingsView ? "settings" : "app"}
              onResizeMouseDown={handleResizeMouseDown}
              isResizing={isSidebarResizing}
              appRoutePath={appRoutePath}
              settingsRoutePath={settingsRoutePath}
            />
            <SidebarInset>
              <div
                ref={contentShellRef}
                data-testid="app-layout-content-shell"
                className="relative flex h-full min-h-0 min-w-0 w-full flex-col pt-[env(safe-area-inset-top)] pr-[env(safe-area-inset-right)] pb-[var(--bb-safe-area-bottom,env(safe-area-inset-bottom))] pl-[env(safe-area-inset-left)]"
              >
                {showHeader ? (
                  <AppHeader
                    usesProjectChromeStyle={isRootView || isArchivedView}
                    projectId={projectId}
                    project={project}
                    meta={meta}
                  />
                ) : null}
                <main className="flex min-h-0 flex-1 flex-col p-4 md:p-5">
                  {children}
                </main>
              </div>
            </SidebarInset>
            <SidebarTriggerOverlay />
          </SidebarStateBridge>
          <IframeDragGuardOverlay
            active={isSidebarResizing}
            cursor="col-resize"
          />
          <CommandPalette
            threadId={threadId ?? null}
            projectId={projectId ?? null}
          />
          <NotificationCenter />
          <ProjectPathDialog
            target={quickCreateProject.projectPathDialog.target}
            pending={quickCreateProject.isCreating}
            platform={quickCreateProject.platform}
            hostId={quickCreateProject.hostId}
            hostName={quickCreateProject.hostName}
            hosts={quickCreateProject.hosts}
            onOpenChange={quickCreateProject.projectPathDialog.onOpenChange}
            onSubmit={quickCreateProject.submitProjectPath}
          />
        </ThreadActionsProvider>
      </ThreadTitleMentionResourcesProvider>
    </ProjectActionsProvider>
  );
}

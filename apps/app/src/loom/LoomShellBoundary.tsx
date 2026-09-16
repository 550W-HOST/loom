import type { ReactNode } from "react";
import { Button } from "@bb/shared-ui/button";
import { RouteLoadingSkeleton } from "@/components/ui/route-loading-skeleton";
import {
  useShellHealth,
  useShellSidebarBootstrap,
  useShellSystemConfig,
} from "@/lib/loom-shell";
import { readCachedSidebarBootstrap } from "@/lib/sidebar-bootstrap-cache";

/**
 * The product app's input boundary.
 *
 * Everything below this component may assume the loom server is reachable:
 * liveness, the sidebar bootstrap and system config have all been read
 * same-origin, and the failure modes have been rendered explicitly.
 *
 * Each required resource is judged on its own. In particular a cached sidebar
 * must never mask a failed liveness or config read: the sidebar has an offline
 * replay cache, so "we have sidebar data" says nothing about whether the server
 * is answering. Collapsing these into one check is how a dead server renders a
 * fully populated app.
 *
 * Nothing is fabricated on failure: there is no placeholder project, thread, or
 * config value here.
 */

interface LoomShellBoundaryProps {
  children: ReactNode;
}

function ShellLoading() {
  return (
    <div className="h-dvh w-full bg-background text-foreground">
      <RouteLoadingSkeleton isBoundedPane={false} />
    </div>
  );
}

interface ShellFailureProps {
  detail: string;
  onRetry: () => void;
  title?: string;
  description?: string;
}

function ShellFailure({
  detail,
  onRetry,
  title = "Cannot reach the loom server",
  description = "The app is served by the same origin as its API, so this is a server or connection problem — not a configuration one. Reload once the server is back.",
}: ShellFailureProps) {
  return (
    <div
      className="flex h-dvh w-full items-center justify-center bg-background p-6 text-foreground"
      data-testid="loom-shell-error"
    >
      <div className="w-full max-w-md rounded-lg border border-border bg-card p-6">
        <h1 className="text-base font-medium">{title}</h1>
        <p className="mt-2 text-sm text-muted-foreground">{description}</p>
        <p className="mt-3 rounded-md border border-border bg-muted/40 p-3 text-xs text-muted-foreground">
          {detail}
        </p>
        <button
          type="button"
          className="mt-4 w-full cursor-pointer rounded-md bg-primary px-3 py-2 text-sm text-primary-foreground"
          onClick={onRetry}
        >
          Retry
        </button>
      </div>
    </div>
  );
}

/**
 * A persistent, actionable banner for a degraded durability backend.
 *
 * `health.backend_error` means the relay is serving from memory and the frame it
 * just accepted may not survive a restart. Reads keep working by design, so this
 * is a warning that stays on screen — not a toast that disappears before the
 * user can act on it — and it does not block the app.
 */
function ShellDegradedBanner({
  detail,
  onRetry,
}: {
  detail: string;
  onRetry: () => void;
}) {
  return (
    <div
      role="status"
      aria-live="polite"
      data-testid="loom-shell-degraded"
      className="flex flex-wrap items-center gap-2 border-b border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-foreground"
    >
      <span className="font-medium">Storage is degraded.</span>
      <span className="min-w-0 flex-1 text-muted-foreground">
        Reads still work, but new messages may not survive a server restart.{" "}
        <span className="font-mono">{detail}</span>
      </span>
      <Button
        type="button"
        size="sm"
        variant="outline"
        className="h-7 shrink-0 px-2 text-xs"
        onClick={onRetry}
      >
        Retry
      </Button>
    </div>
  );
}

/** The stale-sidebar notice for a replay cache that could not be refreshed. */
function ShellStaleDataNotice() {
  return (
    <div
      role="status"
      aria-live="polite"
      data-testid="loom-shell-stale"
      className="border-b border-border bg-muted/40 px-3 py-2 text-xs text-muted-foreground"
    >
      Showing saved projects and threads — the server could not be reached to
      refresh them.
    </div>
  );
}

export function LoomShellBoundary({ children }: LoomShellBoundaryProps) {
  const health = useShellHealth();
  const sidebar = useShellSidebarBootstrap();
  const systemConfig = useShellSystemConfig();

  const retryAll = () => {
    void health.refetch();
    void sidebar.refetch();
    void systemConfig.refetch();
  };

  const describe = (error: unknown): string =>
    error instanceof Error ? error.message : String(error);

  // Liveness decides reachability, so it is answered first and the shell waits
  // for it. Rendering the config failure while liveness is still undecided made
  // a total outage show "settings" first and then flip to "unreachable".
  if (health.isError) {
    return (
      <ShellFailure detail={describe(health.error)} onRetry={retryAll} />
    );
  }

  const hasSidebarData = sidebar.data !== undefined;
  // On an error React Query drops the placeholder, so the offline copy has to be
  // read from the cache itself: otherwise a failed refresh would look like "no
  // data at all" and discard a perfectly good saved sidebar.
  const hasOfflineCopy = hasSidebarData || readCachedSidebarBootstrap() !== null;

  if (health.isPending || systemConfig.isPending) {
    return <ShellLoading />;
  }

  // The server answered but its settings could not be read. This is not an
  // offline condition, so saved sidebar data must not stand in for it.
  if (systemConfig.isError) {
    return (
      <ShellFailure
        detail={describe(systemConfig.error)}
        onRetry={retryAll}
        title="Cannot load server settings"
        description="The server answered, but the app could not read the settings it needs to render. This is not an offline condition, so saved data is not shown."
      />
    );
  }

  // The sidebar has a replay cache. A failed read is only fatal when there is
  // nothing to replay; otherwise the app renders with an explicit stale notice.
  if (sidebar.isError && !hasOfflineCopy) {
    return (
      <ShellFailure
        detail={describe(sidebar.error)}
        onRetry={retryAll}
        title="Cannot load your projects"
        description="The server is reachable, but the project and thread list could not be read, and there is no saved copy to show."
      />
    );
  }

  const backendError = health.data?.backend_error;

  return (
    <div className="flex h-dvh min-h-0 w-full flex-col">
      {backendError ? (
        <ShellDegradedBanner detail={backendError} onRetry={retryAll} />
      ) : null}
      {sidebar.isError ? <ShellStaleDataNotice /> : null}
      <div className="min-h-0 flex-1">{children}</div>
    </div>
  );
}

/** True when the server answered but has no projects or threads to show. */
export function isShellEmpty(
  sidebar: ReturnType<typeof useShellSidebarBootstrap>,
): boolean {
  const data = sidebar.data;
  if (!data) return false;
  return (
    data.projects.length === 0 &&
    data.personalProject.threads.length === 0 &&
    data.sections.length === 0
  );
}

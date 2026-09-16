import type { ReactNode } from "react";
import { RouteLoadingSkeleton } from "@/components/ui/route-loading-skeleton";
import {
  useShellHealth,
  useShellSidebarBootstrap,
  useShellSystemConfig,
} from "@/lib/loom-shell";

/**
 * The product app's input boundary.
 *
 * Everything below this component may assume the loom server is reachable:
 * liveness, the sidebar bootstrap and system config have all been read
 * same-origin, and the failure modes have been rendered explicitly.
 *
 * The three states are deliberately distinct — loading, unreachable, and
 * empty — because collapsing them is how a UI ends up showing an empty sidebar
 * that actually means "the server rejected us". Nothing is fabricated on
 * failure: there is no placeholder project, thread, or config value here.
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
}

function ShellFailure({ detail, onRetry }: ShellFailureProps) {
  return (
    <div
      className="flex h-dvh w-full items-center justify-center bg-background p-6 text-foreground"
      data-testid="loom-shell-error"
    >
      <div className="w-full max-w-md rounded-lg border border-border bg-card p-6">
        <h1 className="text-base font-medium">Cannot reach the loom server</h1>
        <p className="mt-2 text-sm text-muted-foreground">
          The app is served by the same origin as its API, so this is a server
          or connection problem — not a configuration one. Reload once the
          server is back.
        </p>
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

export function LoomShellBoundary({ children }: LoomShellBoundaryProps) {
  const health = useShellHealth();
  const sidebar = useShellSidebarBootstrap();
  const systemConfig = useShellSystemConfig();

  const failure = health.error ?? sidebar.error ?? systemConfig.error;
  if (failure && !sidebar.data) {
    return (
      <ShellFailure
        detail={failure instanceof Error ? failure.message : String(failure)}
        onRetry={() => {
          void health.refetch();
          void sidebar.refetch();
          void systemConfig.refetch();
        }}
      />
    );
  }

  if (health.isPending || systemConfig.isPending) {
    return <ShellLoading />;
  }

  // A degraded durability backend still serves reads; the shell renders, and
  // the operator learns about it from health rather than from a blank page.
  return <>{children}</>;
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

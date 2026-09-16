import type { PointerEventHandler } from "react";
import { Link, useLocation } from "react-router-dom";
import { Icon } from "@bb/shared-ui/icon";
import { Button } from "@bb/shared-ui/button";
import { cn } from "@bb/shared-ui/lib/utils";
import { useAppCommandRunner } from "@/components/commands/AppCommandProvider";
import { AUTOMATIONS_ROUTE_PATH } from "@/lib/route-paths";
import {
  PROJECT_LIST_ACTION_BUTTON_CLASS,
  ProjectListNewThreadAction,
  ProjectListSearchThreadsAction,
} from "./ProjectList";

export interface SidebarNavigationRegionProps {
  compactCustomizeMode?: boolean;
  onCompactCustomizeModeChange?: (active: boolean) => void;
  onNavigate?: () => void;
  splitEnabled?: boolean;
  newThreadSplit?: {
    onPointerDown?: PointerEventHandler<HTMLElement>;
    openInSplit(): void;
  };
  onNewChat?: () => void;
  onSearchThreads?: () => void;
}

export function SidebarNavigationRegion({
  compactCustomizeMode,
  onNavigate,
  splitEnabled,
  newThreadSplit,
  onNewChat,
  onSearchThreads,
}: SidebarNavigationRegionProps) {
  const location = useLocation();
  const commandRunner = useAppCommandRunner();
  const automationsActive = location.pathname.startsWith(
    AUTOMATIONS_ROUTE_PATH,
  );

  return (
    <nav
      aria-label="Sidebar navigation"
      data-testid="sidebar-navigation-region"
      className={cn(
        "space-y-1",
        compactCustomizeMode && "flex min-h-0 flex-1 flex-col",
      )}
    >
      <ProjectListNewThreadAction
        splitEnabled={splitEnabled}
        newThreadSplit={newThreadSplit}
        onNewChat={onNewChat}
      />
      <ProjectListSearchThreadsAction
        onSearchThreads={() => {
          onSearchThreads?.();
          commandRunner.dispatch("thread.search", null);
        }}
      />
      <Button
        asChild
        type="button"
        size="sm"
        variant="ghost"
        className={cn(
          PROJECT_LIST_ACTION_BUTTON_CLASS,
          "w-full justify-start",
          automationsActive &&
            "bg-sidebar-accent text-sidebar-accent-foreground",
        )}
      >
        <Link to={AUTOMATIONS_ROUTE_PATH} onClick={onNavigate}>
          <Icon name="Repeat" className="size-4" aria-hidden />
          <span>Automations</span>
        </Link>
      </Button>
    </nav>
  );
}

import { useMemo } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { AutomationsPanel } from "bb-plugin-automations/panel";
import type { AutomationsNavigation } from "bb-plugin-automations/runtime";
import { createLoomAutomationsClient } from "@/lib/loom-automations-client";
import {
  AUTOMATIONS_ROUTE_PATH,
  getRootComposeRoutePath,
  getThreadRoutePath,
} from "@/lib/route-paths";

const automationsClient = createLoomAutomationsClient();

function automationsSubPath(pathname: string): string {
  if (!pathname.startsWith(AUTOMATIONS_ROUTE_PATH)) return "";
  return pathname.slice(AUTOMATIONS_ROUTE_PATH.length).replace(/^\/+/, "");
}

export function AutomationsView() {
  const location = useLocation();
  const navigate = useNavigate();
  const navigation = useMemo<AutomationsNavigation>(
    () => ({
      toCompose({ focusPrompt, initialPrompt }) {
        void navigate(getRootComposeRoutePath(), {
          state: { focusPrompt, initialPrompt },
        });
      },
      toThread(threadId, projectId) {
        void navigate(getThreadRoutePath({ projectId, threadId }));
      },
      toPanel(subPath) {
        void navigate(
          subPath.length === 0
            ? AUTOMATIONS_ROUTE_PATH
            : `${AUTOMATIONS_ROUTE_PATH}/${subPath}`,
        );
      },
    }),
    [navigate],
  );

  return (
    <AutomationsPanel
      subPath={automationsSubPath(location.pathname)}
      client={automationsClient}
      navigation={navigation}
    />
  );
}

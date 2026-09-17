import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter } from "react-router-dom";
import { App } from "./App";
import { AppErrorBoundary } from "./components/AppErrorBoundary";
import { AppToaster } from "./components/AppToaster";
import { LoomShellBoundary } from "./loom/LoomShellBoundary";
import { registerProviderCliInstallQueryClient } from "./components/provider-cli/provider-cli-install-store";
import { initializePreferredTheme } from "./hooks/useTheme";
import { useWebSocket } from "./hooks/useWebSocket";
import { initializeFavicon } from "./lib/favicon-color-preference";
import { installForeignDomMutationGuard } from "./lib/foreign-dom-mutation-guard";
import { installAppQueryClientBrowserEvents } from "./lib/query-client";
import { appQueryClient } from "./lib/app-query-client";
import { applyCachedAppThemeCss } from "./lib/themes";
import "./app.css";

installForeignDomMutationGuard();

Error.stackTraceLimit = 50;

installAppQueryClientBrowserEvents(appQueryClient);
registerProviderCliInstallQueryClient(appQueryClient);

initializePreferredTheme();
applyCachedAppThemeCss();
initializeFavicon();

function LoomRealtimeConnection() {
  useWebSocket();
  return null;
}

createRoot(document.getElementById("root")!, {
  onUncaughtError: (error, errorInfo) => {
    console.error(
      "[bb] uncaught render error — the app root was torn down",
      error,
      errorInfo.componentStack,
    );
  },
}).render(
  <StrictMode>
    {}
    <AppErrorBoundary>
      <QueryClientProvider client={appQueryClient}>
        <BrowserRouter>
          <LoomRealtimeConnection />
          <LoomShellBoundary>
            <App />
            <AppToaster position="bottom-right" />
          </LoomShellBoundary>
        </BrowserRouter>
      </QueryClientProvider>
    </AppErrorBoundary>
  </StrictMode>,
);

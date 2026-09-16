import { useEffect, useRef, useState } from "react";
import { useMutation } from "@tanstack/react-query";
import type { Host } from "@bb/domain";
import { Button } from "@bb/shared-ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@bb/shared-ui/dialog";
import { Icon } from "@bb/shared-ui/icon";
import { MachineStatusDot } from "@/components/machines/MachineStatusDot";
import { useHosts } from "@/hooks/queries/host-queries";
import { useClipboardCopy } from "@/lib/clipboard";
import { getMutationErrorMessage } from "@/lib/mutation-errors";
import {
  buildLoomPairingCommand,
  createLoomJoinCode,
  type LoomJoinCode,
  type LoomPairingCommand,
} from "@/lib/loom-machine-pairing";

interface AddMachineDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  serverUrl: string | null;
}

export function AddMachineDialog({
  open,
  onOpenChange,
  serverUrl,
}: AddMachineDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        {open ? (
          <AddMachineDialogContent
            onOpenChange={onOpenChange}
            serverUrl={serverUrl}
          />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

function formatCountdown(remainingMs: number): string {
  const totalSeconds = Math.max(0, Math.floor(remainingMs / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

function pairingCommand(
  joinCode: LoomJoinCode,
  configuredServerUrl: string | null,
): LoomPairingCommand {
  return buildPairingCommandPreview({ joinCode, configuredServerUrl });
}

function buildPairingCommandPreview(args: {
  joinCode: LoomJoinCode;
  configuredServerUrl: string | null;
}): LoomPairingCommand {
  return buildLoomPairingCommand({
    joinCode: args.joinCode.joinCode,
    hostId: args.joinCode.hostId,
    configuredServerUrl: args.configuredServerUrl,
  });
}

const REMOTE_ACCESS_NOT_CONFIGURED_MESSAGE =
  "This server's address only resolves on this machine, so a command run elsewhere cannot reach it. Open the app from an address other machines can reach (or configure remote access), then come back here.";

function UnreachableServerNotice({ serverUrl }: { serverUrl: string }) {
  return (
    <div
      role="status"
      aria-live="polite"
      className="space-y-2 rounded-md border border-border bg-muted/40 p-3"
    >
      <p className="text-sm text-foreground">
        Another machine cannot use this address.
      </p>
      <p className="text-xs text-subtle-foreground">
        The pairing command would target{" "}
        <span className="font-mono">{serverUrl}</span>, which points to the
        machine that runs it, not to this server.{" "}
        {REMOTE_ACCESS_NOT_CONFIGURED_MESSAGE}
      </p>
    </div>
  );
}

function AddMachineDialogContent({
  onOpenChange,
  serverUrl,
}: {
  onOpenChange: (open: boolean) => void;
  serverUrl: string | null;
}) {
  const hostsQuery = useHosts();
  const mintJoinCode = useMutation({
    meta: { showErrorToast: false },
    mutationFn: () => createLoomJoinCode(),
  });
  const mint = mintJoinCode.mutate;
  useEffect(() => {
    mint();
  }, [mint]);

  const baselineHostIds = useRef<Set<string> | null>(null);
  if (baselineHostIds.current === null && hostsQuery.data !== undefined) {
    baselineHostIds.current = new Set(hostsQuery.data.map((host) => host.id));
  }
  const connectedNewHost: Host | null =
    (baselineHostIds.current !== null
      ? hostsQuery.data?.find(
          (host) =>
            host.status === "connected" &&
            !baselineHostIds.current?.has(host.id),
        )
      : undefined) ?? null;

  const joinCode = mintJoinCode.data ?? null;
  const expiresAt = joinCode === null ? null : joinCode.expiresAt;
  // The command is derived from a reachable address, never from an empty
  // `system.config.serverUrl`: `resolvePairingServerUrl` prefers the configured
  // URL only when it is a real absolute URL and otherwise falls back to the
  // origin the app is served from.
  const pairing =
    joinCode === null
      ? null
      : pairingCommand(joinCode, serverUrl);
  const unreachable =
    pairing?.kind === "unreachable" ? { serverUrl: pairing.serverUrl } : null;
  const command = pairing?.kind === "ready" ? pairing.command : null;
  // `no-server-address` means neither the configured URL nor the origin is a
  // usable absolute address. Nothing is printed in that case, and the dialog
  // says so rather than rendering an empty command box.
  const hasNoServerAddress = pairing?.kind === "no-server-address";

  const [now, setNow] = useState(() => Date.now());
  const hasCountdown = command !== null && expiresAt !== null;
  useEffect(() => {
    if (!hasCountdown) return;
    const interval = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(interval);
  }, [hasCountdown]);
  const remainingMs =
    hasCountdown && expiresAt !== null ? expiresAt - now : null;
  const expired = remainingMs !== null && remainingMs <= 0;
  const { copied, copy } = useClipboardCopy({ text: command ?? "" });

  return (
    <>
      <DialogHeader>
        <DialogTitle>Add a machine</DialogTitle>
        <DialogDescription>
          {unreachable !== null
            ? "Pair a machine to run projects and threads on it."
            : "Run this command on the machine you want to add. It installs loom and keeps the machine connected to this server."}
        </DialogDescription>
      </DialogHeader>
      <div className="space-y-3">
        {mintJoinCode.isError ? (
          <div className="space-y-2">
            <p className="text-sm text-destructive">
              {getMutationErrorMessage({
                error: mintJoinCode.error,
                fallbackMessage: "Couldn't create a join code.",
              })}
            </p>
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => mintJoinCode.mutate()}
            >
              Try again
            </Button>
          </div>
        ) : unreachable !== null ? (
          <UnreachableServerNotice serverUrl={unreachable.serverUrl} />
        ) : hasNoServerAddress ? (
          <p role="status" className="text-sm text-muted-foreground">
            This app does not know a server address that another machine could
            use, so there is no pairing command to show.
          </p>
        ) : command !== null ? (
          <div
            data-add-machine-command
            className="overflow-hidden rounded-md border border-border bg-muted/30"
          >
            <pre className="overflow-x-auto whitespace-pre-wrap break-all p-3 font-mono text-xs text-foreground">
              {command}
            </pre>
            <div className="flex flex-wrap items-center gap-2 border-t border-border px-3 py-2">
              {expired ? (
                <>
                  <span className="text-xs text-subtle-foreground">
                    Code expired
                  </span>
                  <Button
                    type="button"
                    size="sm"
                    variant="ghost"
                    className="h-7 px-2 text-xs"
                    disabled={mintJoinCode.isPending}
                    onClick={() => mintJoinCode.mutate()}
                  >
                    Generate a new code
                  </Button>
                </>
              ) : remainingMs !== null ? (
                <span className="text-xs tabular-nums text-subtle-foreground">
                  Code expires in {formatCountdown(remainingMs)}
                </span>
              ) : null}
              <Button
                type="button"
                size="sm"
                variant="outline"
                className="ml-auto h-7 px-2.5 text-xs"
                disabled={expired}
                onClick={() => void copy()}
              >
                {copied ? "Copied" : "Copy"}
              </Button>
            </div>
          </div>
        ) : (
          <p className="flex items-center gap-2 text-sm text-muted-foreground">
            <Icon name="Spinner" className="size-4 shrink-0 animate-spin" />
            Creating a join code…
          </p>
        )}
        {unreachable !== null ? null : (
          <div className="flex items-center gap-2.5 rounded-md bg-muted/40 px-3 py-2.5">
            {connectedNewHost !== null ? (
              <>
                <MachineStatusDot connected />
                <span className="min-w-0 flex-1 truncate text-sm text-foreground">
                  {connectedNewHost.name} connected
                </span>
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  className="h-7 shrink-0 px-2 text-xs"
                  onClick={() => onOpenChange(false)}
                >
                  Set up a project on it →
                </Button>
              </>
            ) : (
              <>
                <Icon
                  name="Spinner"
                  className="size-4 shrink-0 animate-spin text-muted-foreground"
                />
                <span className="text-sm text-muted-foreground">
                  Waiting for the machine to connect…
                </span>
              </>
            )}
          </div>
        )}
        {hostsQuery.isError && unreachable === null ? (
          <p role="alert" className="text-xs text-destructive">
            {
              "Couldn't check whether the machine connected. The pairing command above is unaffected."
            }
          </p>
        ) : null}
      </div>
      <DialogFooter>
        <Button
          type="button"
          variant="ghost"
          onClick={() => onOpenChange(false)}
        >
          Done
        </Button>
      </DialogFooter>
    </>
  );
}

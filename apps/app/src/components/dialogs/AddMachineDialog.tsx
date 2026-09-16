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
import { isLocalOnlyUrl } from "@/lib/loopback-hostname";
import { getMutationErrorMessage } from "@/lib/mutation-errors";
import {
  createLoomJoinCode,
  createLoomMachineCode,
  type LoomMachineCode,
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
  joinCode: string,
  hostId: string,
  machineCode: LoomMachineCode | null,
  directServerUrl: string | null,
): string | null {
  const serverUrl = machineCode?.serverUrl ?? directServerUrl;
  if (serverUrl === null) return null;
  const machineFlag =
    machineCode === null ? "" : ` --machine-code ${machineCode.code}`;
  return `curl -fL --progress-meter --connect-timeout 10 --max-time 60 --retry 2 ${serverUrl}/install.sh | sh -s -- --join-code ${joinCode} --host-id ${hostId} --server ${serverUrl}${machineFlag}`;
}

const REMOTE_ACCESS_NOT_CONFIGURED_MESSAGE =
  "Remote access is not configured on this loom server, so the pairing command only works from a machine that can reach the address below.";

function UnreachableServerNotice({
  serverUrl,
}: {
  serverUrl: string;
}) {
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
    mutationFn: async () => {
      const [join, machine] = await Promise.all([
        createLoomJoinCode(),
        createLoomMachineCode(),
      ]);
      return { join, machine };
    },
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

  const joinCode = mintJoinCode.data?.join ?? null;
  const machineCodeResult = mintJoinCode.data?.machine ?? null;
  const machineCode =
    machineCodeResult?.kind === "issued" ? machineCodeResult.code : null;
  const expiresAt =
    joinCode === null
      ? null
      : Math.min(joinCode.expiresAt, machineCode?.expiresAt ?? Infinity);
  const localOnlyServerUrl =
    serverUrl !== null && isLocalOnlyUrl(serverUrl) ? serverUrl : null;
  // loom has no `connect` plugin, so a machine code is never issued. A
  // local-only server URL therefore cannot be paired from another machine, and
  // the dialog says so instead of presenting a command that cannot work.
  const unreachable =
    machineCodeResult?.kind === "unavailable" && localOnlyServerUrl !== null
      ? { serverUrl: localOnlyServerUrl }
      : null;
  const showCommand = joinCode !== null && unreachable === null;

  const [now, setNow] = useState(() => Date.now());
  const hasCountdown = showCommand && expiresAt !== null;
  useEffect(() => {
    if (!hasCountdown) return;
    const interval = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(interval);
  }, [hasCountdown]);
  const remainingMs =
    hasCountdown && expiresAt !== null ? expiresAt - now : null;
  const expired = remainingMs !== null && remainingMs <= 0;
  const command =
    showCommand && joinCode !== null
      ? pairingCommand(
          joinCode.joinCode,
          joinCode.hostId,
          machineCode,
          serverUrl,
        )
      : null;
  const { copied, copy } = useClipboardCopy({ text: command ?? "" });

  return (
    <>
      <DialogHeader>
        <DialogTitle>Add a machine</DialogTitle>
        <DialogDescription>
          {unreachable !== null
            ? "Pair a machine to run projects and threads on it."
            : "Run this command on the machine you want to add. It installs bb and keeps the machine connected to this server."}
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

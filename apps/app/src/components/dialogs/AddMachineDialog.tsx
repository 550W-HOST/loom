import { useEffect, useState } from "react";
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
  createLoomJoinCode,
  LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON,
  resolveLoomPairingState,
} from "@/lib/loom-machine-pairing";

/** Shown before a join code exists, so the notice never renders empty. */
const LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON_FALLBACK =
  LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON;

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

function AddMachineDialogContent({
  onOpenChange,
  serverUrl: _serverUrl,
}: {
  onOpenChange: (open: boolean) => void;
  serverUrl: string | null;
}) {
  const hostsQuery = useHosts();
  const mintJoinCode = useMutation({
    meta: { showErrorToast: false },
    // The join code is real and contract-backed, so it is still minted. What is
    // not real is the install command, which this dialog refuses to invent.
    mutationFn: () => createLoomJoinCode(),
  });
  const mint = mintJoinCode.mutate;
  useEffect(() => {
    mint();
  }, [mint]);

  // Hosts are read once, on demand, and only to answer "is a new machine here
  // yet?" when the user asks. There is no subscription and no polling: before
  // W-587's realtime work lands, watching for a connection would mean either
  // spinning forever or implying a live view that does not exist.
  const [knownHostIds, setKnownHostIds] = useState<ReadonlySet<string> | null>(
    null,
  );
  const hostsData = hostsQuery.data;
  useEffect(() => {
    if (hostsData === undefined) return;
    setKnownHostIds((previous) => {
      if (previous === null) return new Set(hostsData.map((host) => host.id));
      return previous;
    });
  }, [hostsData]);

  const pairedNewHost: Host | null =
    knownHostIds === null
      ? null
      : (hostsData?.find(
          (host) => host.status === "connected" && !knownHostIds.has(host.id),
        ) ?? null);

  const pairing = mintJoinCode.data
    ? resolveLoomPairingState(mintJoinCode.data)
    : null;

  const [now, setNow] = useState(() => Date.now());
  const hasCountdown = mintJoinCode.data !== undefined;
  useEffect(() => {
    if (!hasCountdown) return;
    const interval = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(interval);
  }, [hasCountdown]);
  const expiresAt = mintJoinCode.data?.expiresAt ?? null;
  const remainingMs = expiresAt !== null ? expiresAt - now : null;
  const expired = remainingMs !== null && remainingMs <= 0;

  const { copied, copy } = useClipboardCopy({
    text: mintJoinCode.data?.joinCode ?? "",
  });

  return (
    <>
      <DialogHeader>
        <DialogTitle>Add a machine</DialogTitle>
        <DialogDescription>
          Pair a machine to run projects and threads on it.
        </DialogDescription>
      </DialogHeader>
      <div className="space-y-3">
        <div
          role="status"
          data-testid="loom-machine-install-unavailable"
          className="space-y-2 rounded-md border border-border bg-muted/40 p-3"
        >
          <p className="text-sm text-foreground">
            Automatic machine setup isn&rsquo;t available yet.
          </p>
          <p className="text-xs text-subtle-foreground">
            {pairing?.reason ?? LOOM_MACHINE_INSTALL_UNAVAILABLE_REASON_FALLBACK}
          </p>
        </div>

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
        ) : mintJoinCode.data === undefined ? (
          <p className="flex items-center gap-2 text-sm text-muted-foreground">
            <Icon name="Spinner" className="size-4 shrink-0 animate-spin" />
            Creating a join code…
          </p>
        ) : (
          <div
            data-add-machine-join-code
            className="space-y-2 rounded-md border border-border bg-muted/30 p-3"
          >
            <p className="text-xs text-subtle-foreground">
              This server&rsquo;s join code, for an operator who already has the
              binaries:
            </p>
            <pre className="overflow-x-auto whitespace-pre-wrap break-all font-mono text-xs text-foreground">
              {mintJoinCode.data.joinCode}
            </pre>
            <div className="flex flex-wrap items-center gap-2">
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
                {copied ? "Copied" : "Copy code"}
              </Button>
            </div>
          </div>
        )}

        {/* Explicitly manual: no auto-detection and no indefinite spinner. */}
        <div className="flex items-center gap-2.5 rounded-md bg-muted/40 px-3 py-2.5">
          {pairedNewHost !== null ? (
            <>
              <MachineStatusDot connected />
              <span className="min-w-0 flex-1 truncate text-sm text-foreground">
                {pairedNewHost.name} is connected
              </span>
            </>
          ) : (
            <span className="min-w-0 flex-1 text-sm text-muted-foreground">
              Check whether a machine has joined.
            </span>
          )}
          <Button
            type="button"
            size="sm"
            variant="outline"
            className="h-7 shrink-0 px-2.5 text-xs"
            disabled={hostsQuery.isFetching}
            onClick={() => void hostsQuery.refetch()}
          >
            {hostsQuery.isFetching ? "Checking…" : "Refresh"}
          </Button>
        </div>
        {hostsQuery.isError ? (
          <p role="alert" className="text-xs text-destructive">
            Couldn&rsquo;t check connected machines. The join code above is
            unaffected.
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

import { useCallback, useState } from "react";
import { Button } from "@bb/shared-ui/button";
import type { JsonValue, PendingInteraction } from "@bb/domain";
import {
  PendingInteractionShell,
  type PendingInteractionSourceThread,
} from "@/components/thread/pending-interactions/PendingInteractionShell";
import { useStopThread } from "@/hooks/mutations/thread-runtime-mutations";
import { sdk } from "@/lib/sdk";

export interface PluginPendingInteractionRequest {
  pluginId: string;
  rendererId: string;
  title: string;
  data: JsonValue;
}

interface PluginPendingInteractionComposerProps {
  interaction: Pick<
    PendingInteraction,
    "id" | "threadId" | "createdAt" | "expiresAt"
  >;
  request: PluginPendingInteractionRequest;
  dismissal: "cancel" | "stop-turn";
  sourceThread?: PendingInteractionSourceThread;
}

export function PluginPendingInteractionComposer({
  interaction,
  request,
  dismissal,
  sourceThread,
}: PluginPendingInteractionComposerProps) {
  const stopThread = useStopThread();
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const dismissLabel = dismissal === "cancel" ? "Cancel" : "Stop turn";
  const cancel = useCallback(async () => {
    setSubmitting(true);
    setError(null);
    try {
      if (dismissal === "stop-turn") {
        await stopThread.mutateAsync(interaction.threadId);
      } else {
        await sdk.threads.interactions.cancel({
          interactionId: interaction.id,
          threadId: interaction.threadId,
        });
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSubmitting(false);
    }
  }, [dismissal, interaction.id, interaction.threadId, stopThread]);

  return (
    <PendingInteractionShell
      key={interaction.id}
      label={request.title}
      initiallyExpanded
      errorMessage={error}
      sourceThread={sourceThread}
      testId="unavailable-interaction-shell"
    >
      {() => (
        <div className="space-y-3">
          <p className="text-sm text-muted-foreground">
            This interaction is unavailable.
          </p>
          <Button
            type="button"
            variant="outline"
            onClick={() => void cancel()}
            disabled={submitting}
          >
            {dismissLabel}
          </Button>
        </div>
      )}
    </PendingInteractionShell>
  );
}

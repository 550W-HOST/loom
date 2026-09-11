import type { ThreadTimelineGoal } from "@bb/domain";
import type { ThreadEventWithMeta } from "./build-event-projection.js";
import { getOrderedThreadEvents } from "./group-event-projection-turns.js";

export function extractThreadTimelineGoal(
  events: readonly ThreadEventWithMeta[],
): ThreadTimelineGoal | null {
  let goal: ThreadTimelineGoal | null = null;
  for (const { event, meta } of getOrderedThreadEvents(events)) {
    switch (event.type) {
      case "thread/goal/updated":
        goal = {
          sourceSeq: meta.seq,
          updatedAt: meta.createdAt,
          objective: event.objective,
          status: event.status,
          tokenBudget: event.tokenBudget,
          tokensUsed: event.tokensUsed,
          timeUsedSeconds: event.timeUsedSeconds,
        };
        break;
      case "thread/goal/cleared":
        goal = null;
        break;
    }
  }
  return goal;
}

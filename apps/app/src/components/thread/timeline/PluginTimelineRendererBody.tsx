import type { TimelineViewWorkRow } from "@bb/thread-view";
import type { PluginTimelineRendererSlot } from "@/lib/plugin-slots";

export type PluginRenderableWorkRow = Extract<
  TimelineViewWorkRow,
  { workKind: "tool" }
>;

export function isPluginRenderableWorkRow(
  _row: TimelineViewWorkRow,
): _row is PluginRenderableWorkRow {
  return false;
}

export function usePluginTimelineRenderer(
  _row: TimelineViewWorkRow | null,
): PluginTimelineRendererSlot | null {
  return null;
}

interface PluginTimelineRendererBodyProps {
  row: PluginRenderableWorkRow;
  slot: PluginTimelineRendererSlot;
  original: () => React.ReactElement | null;
}

export function PluginTimelineRendererBody({
  original,
}: PluginTimelineRendererBodyProps) {
  return original();
}

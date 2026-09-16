import type { ReactNode } from "react";
import type { ComposerView } from "./plugin-composer-host";

export function ComposerBannersSlot({
  children,
}: {
  view?: ComposerView;
  children?: ReactNode;
  ownerPlacement?: "before" | "after";
}) {
  return children ?? null;
}

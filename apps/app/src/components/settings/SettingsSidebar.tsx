import type { MouseEvent as ReactMouseEvent } from "react";
import {
  SectionSidebar,
  SectionSidebarIcon,
  SectionSidebarLabel,
  SectionSidebarRow,
} from "@/components/sidebar/SectionSidebar";
import { useSettingsNavState } from "./settings-nav";
import { getSettingsSectionRoutePath } from "./settings-sections";

interface SettingsSidebarProps {
  onResizeMouseDown: (event: ReactMouseEvent<HTMLDivElement>) => void;
  isResizing: boolean;
  showTopReserve: boolean;
  appRoutePath: string;
  mobileHosted?: boolean;
}

export function SettingsSidebar({
  onResizeMouseDown,
  isResizing,
  showTopReserve,
  appRoutePath,
  mobileHosted,
}: SettingsSidebarProps) {
  const { activeSection, sections } = useSettingsNavState();
  const primarySections = sections.filter(
    (section) => section.id !== "archived",
  );
  const archivedSection = sections.find((section) => section.id === "archived");

  return (
    <SectionSidebar
      backLabel="Back to app"
      backTo={appRoutePath}
      isResizing={isResizing}
      mobileHosted={mobileHosted}
      onResizeMouseDown={onResizeMouseDown}
      showTopReserve={showTopReserve}
      testIdPrefix="settings"
    >
      <SectionSidebarLabel>Settings</SectionSidebarLabel>
      <div className="mt-1 space-y-0.5">
        {primarySections.map((section) => (
          <SectionSidebarRow
            key={section.id}
            active={activeSection === section.id}
            label={section.label}
            to={getSettingsSectionRoutePath(section.id)}
          >
            <SectionSidebarIcon name={section.icon} />
          </SectionSidebarRow>
        ))}
      </div>
      {archivedSection === undefined ? null : (
        <>
          <div className="mt-4">
            <SectionSidebarLabel>Archived</SectionSidebarLabel>
          </div>
          <div className="mt-1 space-y-0.5">
            <SectionSidebarRow
              active={activeSection === archivedSection.id}
              label={archivedSection.label}
              to={getSettingsSectionRoutePath(archivedSection.id)}
            >
              <SectionSidebarIcon name={archivedSection.icon} />
            </SectionSidebarRow>
          </div>
        </>
      )}
    </SectionSidebar>
  );
}

import type { ThreadRuntimeDisplayStatus } from "@bb/domain";
import {
  ArrowUpRight,
  Check,
  Command,
  Folder,
  GitBranch,
  LayoutDashboard,
  Menu,
  MessageSquare,
  Moon,
  PanelLeftClose,
  Plus,
  Server,
  Settings2,
  Sparkles,
  SquarePen,
  Sun,
  X,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useState } from "react";

type Theme = "light" | "dark";
export type ProductSection =
  | "overview"
  | "threads"
  | "projects"
  | "environments"
  | "settings";

interface NavigationItem {
  id: ProductSection;
  label: string;
  icon: LucideIcon;
}

export const PRODUCT_NAV_ITEMS: readonly NavigationItem[] = [
  { id: "overview", label: "Overview", icon: LayoutDashboard },
  { id: "threads", label: "Threads", icon: MessageSquare },
  { id: "projects", label: "Projects", icon: Folder },
  { id: "environments", label: "Environments", icon: GitBranch },
  { id: "settings", label: "Settings", icon: Settings2 },
];

const EMPTY_STATUS: ThreadRuntimeDisplayStatus = "idle";

function initialTheme(): Theme {
  if (typeof window === "undefined") return "light";
  const stored = window.localStorage.getItem("loom.product.theme");
  if (stored === "dark" || stored === "light") return stored;
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

function sectionLabel(section: ProductSection): string {
  return PRODUCT_NAV_ITEMS.find((item) => item.id === section)?.label ?? "Overview";
}

function Sidebar({
  activeSection,
  onNavigate,
  onNewThread,
  onClose,
  theme,
  onToggleTheme,
}: {
  activeSection: ProductSection;
  onNavigate: (section: ProductSection) => void;
  onNewThread: () => void;
  onClose: () => void;
  theme: Theme;
  onToggleTheme: () => void;
}) {
  return (
    <aside className="sidebar" aria-label="Primary navigation">
      <div className="sidebar__brand-row">
        <a className="brand" href="#overview" onClick={() => onNavigate("overview")}>
          <img className="brand__mark" src="/loom-mark.png" alt="" width="32" height="32" />
          <span className="brand__name">loom</span>
          <span className="brand__mode">workbench</span>
        </a>
        <button className="icon-button sidebar__close" type="button" onClick={onClose} aria-label="Close navigation" title="Close navigation">
          <PanelLeftClose size={17} strokeWidth={1.8} />
        </button>
      </div>

      <button className="new-thread-button" type="button" onClick={onNewThread}>
        <SquarePen size={17} strokeWidth={1.9} />
        <span>New thread</span>
        <kbd>Cmd K</kbd>
      </button>

      <nav className="sidebar__nav">
        <p className="nav-label">Workspace</p>
        <div className="nav-list">
          {PRODUCT_NAV_ITEMS.map(({ id, label, icon: Icon }) => (
            <button
              className={`nav-item${activeSection === id ? " nav-item--active" : ""}`}
              type="button"
              key={id}
              onClick={() => onNavigate(id)}
              aria-current={activeSection === id ? "page" : undefined}
            >
              <Icon size={17} strokeWidth={1.8} />
              <span>{label}</span>
              {id === "threads" ? <span className="nav-item__count">0</span> : null}
            </button>
          ))}
        </div>
      </nav>

      <div className="sidebar__bottom">
        <div className="build-status" aria-label="Product shell preview status">
          <span className="status-dot status-dot--ready" />
          <span>Product shell</span>
          <span className="build-status__value">preview</span>
        </div>
        <div className="sidebar__account-row">
          <div className="account">
            <span className="account__avatar">P</span>
            <span className="account__copy">
              <strong>Personal workspace</strong>
              <span>Local account</span>
            </span>
          </div>
          <button
            className="icon-button"
            type="button"
            onClick={onToggleTheme}
            aria-label={`Use ${theme === "light" ? "dark" : "light"} theme`}
            title={`Use ${theme === "light" ? "dark" : "light"} theme`}
          >
            {theme === "light" ? <Moon size={17} strokeWidth={1.8} /> : <Sun size={17} strokeWidth={1.8} />}
          </button>
        </div>
      </div>
    </aside>
  );
}

function Topbar({
  activeSection,
  onOpenSidebar,
  onOpenCommand,
}: {
  activeSection: ProductSection;
  onOpenSidebar: () => void;
  onOpenCommand: () => void;
}) {
  return (
    <header className="topbar">
      <button className="icon-button topbar__menu" type="button" onClick={onOpenSidebar} aria-label="Open navigation" title="Open navigation">
        <Menu size={19} strokeWidth={1.8} />
      </button>
      <div className="breadcrumb" aria-label="Current location">
        <span className="breadcrumb__root">Workspace</span>
        <span className="breadcrumb__divider">/</span>
        <strong>{sectionLabel(activeSection)}</strong>
      </div>
      <div className="topbar__actions">
        <button className="command-trigger" type="button" onClick={onOpenCommand} aria-label="Open command menu" title="Open command menu">
          <Command size={15} strokeWidth={1.8} />
          <span>Search</span>
          <kbd>Cmd K</kbd>
        </button>
        <button className="account__avatar account__avatar--top" type="button" aria-label="Account menu" title="Account menu">P</button>
      </div>
    </header>
  );
}

function OverviewPage({ onNewThread }: { onNewThread: () => void }) {
  return (
    <div className="page page--overview" data-testid="overview-page">
      <div className="page-heading">
        <div className="heading-kicker"><Sparkles size={15} strokeWidth={1.8} /><span>Personal workspace</span></div>
        <h1>What are you working on?</h1>
        <p>Start a thread to turn an idea into a focused piece of work.</p>
      </div>

      <section className="start-surface" aria-labelledby="start-title">
        <div className="start-surface__copy">
          <div className="surface-icon surface-icon--accent"><SquarePen size={19} strokeWidth={1.8} /></div>
          <div>
            <h2 id="start-title">Start a new thread</h2>
            <p>Choose a project and environment when thread setup is available.</p>
          </div>
        </div>
        <button className="primary-button" type="button" onClick={onNewThread}>
          <Plus size={17} strokeWidth={2} />
          <span>New thread</span>
        </button>
      </section>

      <div className="overview-grid">
        <section className="surface" aria-labelledby="threads-title" data-thread-status={EMPTY_STATUS}>
          <div className="surface__header">
            <div>
              <p className="surface__eyebrow">Activity</p>
              <h2 id="threads-title">Threads</h2>
            </div>
            <span className="surface__metric">0 active</span>
          </div>
          <EmptyState icon={MessageSquare} title="No threads yet" detail="Your active threads will appear here." />
        </section>

        <section className="surface" aria-labelledby="projects-title">
          <div className="surface__header">
            <div>
              <p className="surface__eyebrow">Context</p>
              <h2 id="projects-title">Projects</h2>
            </div>
            <span className="surface__metric">0 connected</span>
          </div>
          <EmptyState icon={Folder} title="No projects connected" detail="Projects give threads a home and an environment." />
        </section>
      </div>

      <div className="overview-footer">
        <div className="overview-footer__item"><Server size={16} strokeWidth={1.8} /><span>Local-first workspace</span></div>
        <div className="overview-footer__item"><Check size={16} strokeWidth={2} /><span>Ready for app setup</span></div>
        <span className="overview-footer__note">Reference client remains a separate build target</span>
      </div>
    </div>
  );
}

function EmptyState({ icon: Icon, title, detail }: { icon: LucideIcon; title: string; detail: string }) {
  return (
    <div className="empty-state">
      <div className="empty-state__icon"><Icon size={20} strokeWidth={1.7} /></div>
      <div>
        <h3>{title}</h3>
        <p>{detail}</p>
      </div>
    </div>
  );
}

function SectionPage({ section, onNavigate }: { section: Exclude<ProductSection, "overview">; onNavigate: (section: ProductSection) => void }) {
  const content: Record<Exclude<ProductSection, "overview">, { icon: LucideIcon; title: string; detail: string }> = {
    threads: { icon: MessageSquare, title: "No threads yet", detail: "Create a thread to see it in this workspace." },
    projects: { icon: Folder, title: "No projects connected", detail: "Project setup will be available from this workspace." },
    environments: { icon: GitBranch, title: "No environments available", detail: "Environments will be listed here once a project is connected." },
    settings: { icon: Settings2, title: "Workspace settings", detail: "Theme preferences are available from the navigation footer." },
  };
  const current = content[section];
  const Icon = current.icon;
  return (
    <div className="page page--section" data-testid={`${section}-page`}>
      <div className="page-heading page-heading--compact">
        <div className="heading-kicker"><Icon size={15} strokeWidth={1.8} /><span>Workspace</span></div>
        <h1>{current.title}</h1>
        <p>{current.detail}</p>
      </div>
      <section className="surface surface--single">
        <EmptyState icon={current.icon} title={section === "settings" ? "Configuration is ready to be connected" : current.title} detail={section === "settings" ? "The product app keeps its shell independent from server capabilities." : "This view is intentionally empty until the next app surface is connected."} />
        {section !== "settings" ? <button className="secondary-button" type="button" onClick={() => onNavigate("overview")}><span>Back to overview</span></button> : null}
      </section>
    </div>
  );
}

function CommandMenu({ onClose, onNavigate }: { onClose: () => void; onNavigate: (section: ProductSection) => void }) {
  return (
    <div className="command-layer" role="presentation" onMouseDown={onClose}>
      <section className="command-menu" role="dialog" aria-modal="true" aria-labelledby="command-title" onMouseDown={(event) => event.stopPropagation()}>
        <div className="command-menu__header">
          <div><Command size={17} strokeWidth={1.8} /><h2 id="command-title">Jump to</h2></div>
          <button className="icon-button" type="button" onClick={onClose} aria-label="Close command menu" title="Close command menu"><X size={17} strokeWidth={1.8} /></button>
        </div>
        <div className="command-menu__list">
          {PRODUCT_NAV_ITEMS.map(({ id, label, icon: Icon }) => (
            <button className="command-menu__item" type="button" key={id} onClick={() => { onNavigate(id); onClose(); }}>
              <Icon size={17} strokeWidth={1.8} />
              <span>{label}</span>
              <ArrowUpRight className="command-menu__arrow" size={15} strokeWidth={1.8} />
            </button>
          ))}
        </div>
      </section>
    </div>
  );
}

export function App() {
  const [activeSection, setActiveSection] = useState<ProductSection>("overview");
  const [sidebarOpen, setSidebarOpen] = useState(false);
  const [commandOpen, setCommandOpen] = useState(false);
  const [theme, setTheme] = useState<Theme>(initialTheme);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    window.localStorage.setItem("loom.product.theme", theme);
  }, [theme]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setCommandOpen((open) => !open);
      }
      if (event.key === "Escape") {
        setCommandOpen(false);
        setSidebarOpen(false);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const navigate = (section: ProductSection) => {
    setActiveSection(section);
    setSidebarOpen(false);
    window.history.replaceState(null, "", section === "overview" ? "#overview" : `#${section}`);
  };

  const toggleTheme = () => setTheme((current) => current === "light" ? "dark" : "light");

  return (
    <div className="app-shell" data-testid="product-app">
      <div className={`sidebar-layer${sidebarOpen ? " sidebar-layer--open" : ""}`}>
        <button className="sidebar-scrim" type="button" onClick={() => setSidebarOpen(false)} aria-label="Close navigation" />
        <Sidebar
          activeSection={activeSection}
          onNavigate={navigate}
          onNewThread={() => navigate("threads")}
          onClose={() => setSidebarOpen(false)}
          theme={theme}
          onToggleTheme={toggleTheme}
        />
      </div>
      <main className="workspace">
        <Topbar activeSection={activeSection} onOpenSidebar={() => setSidebarOpen(true)} onOpenCommand={() => setCommandOpen(true)} />
        <div className="workspace__scroll">
          {activeSection === "overview" ? <OverviewPage onNewThread={() => navigate("threads")} /> : <SectionPage section={activeSection} onNavigate={navigate} />}
        </div>
      </main>
      {commandOpen ? <CommandMenu onClose={() => setCommandOpen(false)} onNavigate={navigate} /> : null}
    </div>
  );
}

export { EMPTY_STATUS };

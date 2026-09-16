import { useCallback, useMemo, useState } from "react";
import type { SidebarBootstrapResponse } from "@bb/server-contract";
import {
  buildSectionMentionSuggestions,
  type SectionMentionCandidate,
} from "./sectionMentionSuggestions";
import { buildPathMentionSuggestions } from "./pathMentionSuggestions";
import {
  buildProjectMentionSuggestions,
  type ProjectMentionCandidate,
} from "./projectMentionSuggestions";
import { useSidebarNavigation } from "./queries/sidebar-navigation-query";
import { useThreadMentionCandidates } from "./queries/thread-queries";
import { buildThreadMentionSuggestions } from "./threadMentionSuggestions";
import { usePathSuggestions } from "./usePathSuggestions";
import {
  DEFAULT_MENTION_TRIGGER,
  MENTION_TRIGGER_VALUES,
  type MentionTrigger,
  type OrderedMentionSuggestions,
} from "@bb/client-core";
import { buildPromptMentionResults } from "./promptMentionCandidates";

const PROMPT_MENTION_SOURCE_LIMIT = 8;

interface UsePromptMentionsOptions {
  currentThreadId?: string;
  threadStorageThreadId?: string;
  environmentId: string | null;
  hostId?: string | null;
}

interface UsePromptMentionsResult {
  query: string | null;
  triggers: readonly MentionTrigger[];
  setQuery: (query: string | null, trigger: MentionTrigger | null) => void;
  results: OrderedMentionSuggestions;
  isLoading: boolean;
  isError: boolean;
}

function buildProjectNamesById(
  sidebarNavigation: SidebarBootstrapResponse | undefined,
): ReadonlyMap<string, string> {
  const names = new Map<string, string>();
  for (const project of sidebarNavigation?.projects ?? []) {
    names.set(project.id, project.name);
  }
  return names;
}

function buildProjectMentionCandidates(
  sidebarNavigation: SidebarBootstrapResponse | undefined,
): ProjectMentionCandidate[] {
  if (!sidebarNavigation) return [];
  return [...sidebarNavigation.projects, sidebarNavigation.personalProject].map(
    (project) => ({ id: project.id, name: project.name }),
  );
}

function buildSectionMentionCandidates(
  sidebarNavigation: SidebarBootstrapResponse | undefined,
): SectionMentionCandidate[] {
  return (
    sidebarNavigation?.sections.map((section) => ({
      id: section.id,
      name: section.name,
    })) ?? []
  );
}

export function usePromptMentions(
  projectId: string | undefined,
  options: UsePromptMentionsOptions,
): UsePromptMentionsResult {
  const [activeMention, setActiveMention] = useState<{
    query: string;
    trigger: MentionTrigger;
  } | null>(null);
  const setQuery = useCallback(
    (query: string | null, trigger: MentionTrigger | null) => {
      setActiveMention(
        query === null
          ? null
          : { query, trigger: trigger ?? DEFAULT_MENTION_TRIGGER },
      );
    },
    [],
  );
  const query = activeMention?.query ?? null;
  const trimmedQuery = query?.trim() ?? "";
  const hasQuery = trimmedQuery.length > 0;

  const pathSearch = usePathSuggestions({
    projectId,
    query,
    limit: PROMPT_MENTION_SOURCE_LIMIT,
    environmentId: options.environmentId,
    hostId: options.hostId,
    currentThreadId: options.threadStorageThreadId,
    includeDirectories: true,
  });
  const projectNamesQuery = useSidebarNavigation({ enabled: hasQuery });
  const threadsQuery = useThreadMentionCandidates({ enabled: hasQuery });
  const projectNamesById = useMemo(
    () => buildProjectNamesById(projectNamesQuery.data),
    [projectNamesQuery.data],
  );
  const projectCandidates = useMemo(
    () => buildProjectMentionCandidates(projectNamesQuery.data),
    [projectNamesQuery.data],
  );
  const sectionCandidates = useMemo(
    () => buildSectionMentionCandidates(projectNamesQuery.data),
    [projectNamesQuery.data],
  );
  const pathSuggestions = useMemo(
    () => buildPathMentionSuggestions({ paths: pathSearch.suggestions }),
    [pathSearch.suggestions],
  );
  const threadSuggestions = useMemo(
    () =>
      buildThreadMentionSuggestions({
        threads: threadsQuery.data ?? [],
        query: trimmedQuery,
        currentProjectId: projectId,
        currentThreadId: options.currentThreadId,
        projectNamesById,
        limit: PROMPT_MENTION_SOURCE_LIMIT,
      }),
    [
      options.currentThreadId,
      projectId,
      projectNamesById,
      threadsQuery.data,
      trimmedQuery,
    ],
  );
  const projectSuggestions = useMemo(
    () =>
      buildProjectMentionSuggestions({
        projects: projectCandidates,
        query: trimmedQuery,
        limit: PROMPT_MENTION_SOURCE_LIMIT,
      }),
    [projectCandidates, trimmedQuery],
  );
  const sectionSuggestions = useMemo(
    () =>
      buildSectionMentionSuggestions({
        sections: sectionCandidates,
        query: trimmedQuery,
        limit: PROMPT_MENTION_SOURCE_LIMIT,
      }),
    [sectionCandidates, trimmedQuery],
  );
  const results = useMemo(
    () =>
      buildPromptMentionResults({
        query: hasQuery ? trimmedQuery : "",
        paths: hasQuery ? pathSuggestions : [],
        threads: hasQuery ? threadSuggestions : [],
        projects: hasQuery ? projectSuggestions : [],
        sections: hasQuery ? sectionSuggestions : [],
        plugins: [],
      }),
    [
      hasQuery,
      pathSuggestions,
      projectSuggestions,
      sectionSuggestions,
      threadSuggestions,
      trimmedQuery,
    ],
  );
  const isLoading =
    hasQuery &&
    results.suggestions.length === 0 &&
    (pathSearch.isDebouncing ||
      pathSearch.isLoading ||
      threadsQuery.isLoading ||
      threadsQuery.isFetching);
  const isThreadError =
    hasQuery &&
    threadsQuery.isError &&
    !threadsQuery.isLoading &&
    !threadsQuery.isFetching;

  return {
    query,
    triggers: MENTION_TRIGGER_VALUES,
    setQuery,
    results,
    isLoading,
    isError: pathSearch.isError || isThreadError,
  };
}

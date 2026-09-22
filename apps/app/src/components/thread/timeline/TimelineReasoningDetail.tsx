import { useMemo } from "react";
import remend from "remend";
import { closeUnterminatedMarkdownCodeSpan } from "@bb/client-core";
import { MarkdownPreview } from "../../ui/markdown-preview.js";

const REASONING_THREAD_MENTIONS = {
  mentions: [],
  preserveSoftBreaks: true,
} as const;
const STREAMING_MARKDOWN_BYPASS_PATTERN =
  /^(?:[ \t]*(?:>|[-+*]|\d{1,9}[.)]))*[ \t]*::[a-zA-Z]|`{3}|~{3}/mu;

interface TimelineReasoningDetailProps {
  text: string;
  streaming?: boolean;
}

export function TimelineReasoningDetail({
  text,
  streaming = false,
}: TimelineReasoningDetailProps) {
  const content = useMemo(() => {
    if (!streaming || STREAMING_MARKDOWN_BYPASS_PATTERN.test(text)) {
      return text;
    }
    return remend(closeUnterminatedMarkdownCodeSpan(text), {
      linkMode: "text-only",
      comparisonOperators: false,
      htmlTags: false,
      katex: false,
      setextHeadings: false,
      singleTilde: false,
    });
  }, [streaming, text]);

  return (
    <div className="max-h-80 overflow-auto break-words border-l border-border pl-3 text-sm leading-relaxed text-muted-foreground">
      <MarkdownPreview
        content={content}
        imagePolicy="alt-text"
        threadMentions={REASONING_THREAD_MENTIONS}
      />
    </div>
  );
}

//! Turning an agent's failure text into something a timeline row can carry.
//!
//! An ACP agent's die-off message is frequently one enormous line. pi-acp
//! reports its child's "stderr tail" verbatim, and a bundled runtime's stderr
//! tail is a whole minified chunk, so a single failure can arrive as tens of
//! kilobytes of JavaScript with the actual exception buried near the end.
//!
//! The **event** keeps that text: it is the evidence, and `threads.events` is
//! where a diagnosis is read. The **row** cannot: a timeline row is copied into
//! every timeline response and every replayed frame, and a user reading a chat
//! needs the exception, not the bundle. This module is the one place that
//! decides what a reader sees first and how much of the rest survives.

/// The most bytes of a headline kept before it is cut.
const HEADLINE_MAX_BYTES: usize = 300;

/// The most bytes of detail kept.
const DETAIL_MAX_BYTES: usize = 4096;

/// How many stack frames after an exception headline are kept.
const STACK_FRAMES: usize = 20;

/// How many trailing lines are kept when there is no exception to anchor on.
const TAIL_LINES: usize = 30;

/// The least amount of unshown text that earns a truncation marker. Below it
/// the difference is whitespace or a line the headline already carried, and a
/// marker would be noise rather than information.
const TRUNCATION_NOTE_MIN_DROPPED: usize = 512;

const TRUNCATION_MARKER: &str = "\n…(truncated)";

/// What a failure message looks like once it has been made readable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFailureSummary {
    /// The one line a reader should see first.
    pub headline: String,
    /// The bounded rest: the exception and its stack, or the tail. `None` when
    /// the headline already is the whole message.
    pub detail: Option<String>,
}

/// Splits an agent failure into a bounded headline and detail.
///
/// The original message is never modified by this function; callers that want
/// the whole thing (the terminal turn event does) keep passing it along
/// untouched.
pub fn summarize_agent_failure(message: &str) -> AgentFailureSummary {
    let normalized = message.replace("\r\n", "\n").replace('\r', "\n");
    let text = normalized.trim();
    if text.is_empty() {
        return AgentFailureSummary {
            headline: "the provider failed without a message".to_owned(),
            detail: None,
        };
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let first_line = lines
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .unwrap_or("the provider failed without a message");
    let (headline_text, headline_cut) = truncate_bytes(first_line, HEADLINE_MAX_BYTES);
    let mut headline = headline_text.trim_end().to_owned();
    if headline_cut {
        headline.push('…');
    }

    let detail = build_detail(&lines, &headline, text.len());
    AgentFailureSummary { headline, detail }
}

/// Builds the bounded detail, or `None` when it would repeat the headline.
fn build_detail(lines: &[&str], headline: &str, message_len: usize) -> Option<String> {
    let (start, end) = excerpt_range(lines)?;
    // A message that opens with its exception carries that line as the
    // headline already; repeating it above the stack would be noise.
    let start = if start == 0 { 1 } else { start };
    if start >= end {
        return None;
    }
    let excerpt = lines[start..end].join("\n");
    let (kept, cut) = truncate_bytes(&excerpt, DETAIL_MAX_BYTES);
    let mut detail = kept.trim_end().to_owned();
    if detail.is_empty() {
        return None;
    }
    if detail == headline.trim() {
        return None;
    }

    let shown = headline.trim().len() + detail.len();
    if cut || message_len.saturating_sub(shown) >= TRUNCATION_NOTE_MIN_DROPPED {
        detail.push_str(TRUNCATION_MARKER);
    }
    Some(detail)
}

/// The half-open line range worth showing: an exception headline with the
/// stack that follows it, or the tail when there is no exception to anchor on.
fn excerpt_range(lines: &[&str]) -> Option<(usize, usize)> {
    if let Some(anchor) = lines.iter().rposition(|line| is_error_headline(line)) {
        let mut end = anchor + 1;
        while end < lines.len() && end - anchor <= STACK_FRAMES && is_stack_frame(lines[end]) {
            end += 1;
        }
        return Some((anchor, end));
    }
    if lines.len() > 1 {
        return Some((lines.len().saturating_sub(TAIL_LINES), lines.len()));
    }
    None
}

/// Whether a line is the start of an exception or panic report.
///
/// Only column-zero matches count: a stack frame or a minified bundle line can
/// contain the word `Error` anywhere, and anchoring on one would show a
/// fragment of the bundle instead of the exception below it.
fn is_error_headline(line: &str) -> bool {
    const PREFIXES: [&str; 9] = [
        "TypeError",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "EvalError",
        "URIError",
        "Error",
        "Uncaught",
        "panic",
    ];
    if line.trim_start().len() != line.len() {
        return false;
    }
    PREFIXES.iter().any(|prefix| {
        line.strip_prefix(prefix).is_some_and(|rest| {
            rest.is_empty()
                || rest.starts_with(':')
                || rest.starts_with(' ')
                || rest.starts_with('(')
        })
    })
}

/// Whether a line is a stack frame belonging to the exception above it.
fn is_stack_frame(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("at ") || trimmed.starts_with("at(")
}

/// Keeps at most `max_bytes`, cutting on a char boundary.
fn truncate_bytes(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape pi-acp reports when the child dies: a headline, then the
    /// child's stderr tail, which for a bundled runtime is one minified chunk
    /// line with the exception at the very end.
    fn pi_style_message() -> String {
        let mut bundle = String::from("`)}}function printTimings(){if(ENABLED)for(let[a,b]of c)");
        bundle.push_str(&"x".repeat(20_000));
        format!(
            "the ACP connection ended: pi process exited (code=Some(1), signal=None) — \
             pi-acp does not restart pi automatically; start a new session (session/new) \
             or restart pi-acp to recover; stderr tail:\n\
             file:///home/ljm/.bun/install/global/node_modules/pi/dist/chunk-7YM6BE7Y.js:1092\n\
             {bundle}\n\
             TypeError: Cannot read properties of undefined (reading 'runtime')\n\
             at streamSimple (file:///home/ljm/.bun/install/global/node_modules/pi/dist/chunk-7YM6BE7Y.js:1092:16944)\n\
             at /home/ljm/.pi/agent/npm/node_modules/pi-observational-memory/src/agents/worker-stream.ts:43:45\n\
             at streamAssistantResponse (file:///home/ljm/.bun/install/global/node_modules/pi/dist/chunk-7YM6BE7Y.js:683:19165)\n\
             at process.processTicksAndRejections (node:internal/process/task_queues:105:5)\n\
             at async runLoop (file:///home/ljm/.bun/install/global/node_modules/pi/dist/chunk-7YM6BE7Y.js:683:16147)\n\
             at async runAgentLoop (file:///home/ljm/.bun/install/global/node_modules/pi/dist/chunk-7YM6BE7Y.js:683:14228)\n\
             Node.js v22.20.0"
        )
    }

    #[test]
    fn a_pi_style_stderr_tail_keeps_the_exception_and_its_stack() {
        let summary = summarize_agent_failure(&pi_style_message());

        assert!(
            summary.headline.starts_with("the ACP connection ended:"),
            "the headline is the agent's own reason: {}",
            summary.headline
        );
        let detail = summary.detail.expect("there is a stack to show");
        assert!(
            detail.starts_with("TypeError: Cannot read properties of undefined"),
            "the exception leads the detail: {detail}"
        );
        assert!(detail.contains("worker-stream.ts:43:45"), "{detail}");
        assert!(detail.contains("at async runAgentLoop"), "{detail}");
        assert!(
            !detail.contains("printTimings"),
            "the minified chunk is not part of the row: {detail}"
        );
        assert!(
            detail.ends_with(TRUNCATION_MARKER.trim_end_matches('\n')),
            "the reader is told something was dropped: {detail}"
        );
        assert!(
            detail.len() < 2000,
            "the detail stays bounded: {}",
            detail.len()
        );
    }

    #[test]
    fn a_short_failure_is_its_own_headline() {
        let summary = summarize_agent_failure("transport closed");
        assert_eq!(summary.headline, "transport closed");
        assert_eq!(summary.detail, None);
    }

    #[test]
    fn a_message_following_its_own_exception_does_not_repeat_it() {
        let message = "TypeError: boom\n    at foo (a.ts:1:1)\n    at bar (b.ts:2:2)";
        let summary = summarize_agent_failure(message);
        assert_eq!(summary.headline, "TypeError: boom");
        let detail = summary.detail.expect("the stack is the detail");
        assert!(!detail.contains("TypeError"), "no repetition: {detail}");
        assert!(detail.contains("at foo (a.ts:1:1)"), "{detail}");
        assert!(detail.contains("at bar (b.ts:2:2)"), "{detail}");
        assert!(!detail.contains(TRUNCATION_MARKER), "{detail}");
    }

    #[test]
    fn a_multiline_message_without_an_exception_keeps_its_tail() {
        let message = "first\nsecond\nthird";
        let summary = summarize_agent_failure(message);
        assert_eq!(summary.headline, "first");
        let detail = summary.detail.expect("the rest is the detail");
        assert!(detail.contains("second"), "{detail}");
        assert!(detail.contains("third"), "{detail}");
        assert!(!detail.contains(TRUNCATION_MARKER), "{detail}");
    }

    #[test]
    fn an_enormous_single_line_is_cut_at_the_headline() {
        let summary = summarize_agent_failure(&"y".repeat(100_000));
        assert!(summary.headline.len() <= HEADLINE_MAX_BYTES + '…'.len_utf8());
        assert!(summary.headline.ends_with('…'));
        assert_eq!(summary.detail, None);
    }

    #[test]
    fn an_empty_message_still_says_something() {
        let summary = summarize_agent_failure("   \n  ");
        assert_eq!(summary.headline, "the provider failed without a message");
        assert_eq!(summary.detail, None);
    }

    #[test]
    fn carriage_returns_are_normalized() {
        let summary = summarize_agent_failure("boom\r\ncause");
        assert_eq!(summary.headline, "boom");
        assert_eq!(summary.detail.as_deref(), Some("cause"));
    }
}

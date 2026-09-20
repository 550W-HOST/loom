//! Loading a thread's history from its ACP session, without running it.
//!
//! The property under test is not the translation — the translator has its own
//! tests — but the *lifecycle*: a history load opens a session, lets the agent
//! replay it, and closes again. It never prompts. The fake agents below record
//! the methods they were asked for, so "no prompt" is an assertion over the
//! agent's side rather than an assumption about ours.

use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_domain::{ProviderEvent, ThreadEventItem, ThreadId, UserContent};
use loom_worker::acp::history::{load_history, HistoryLimits};
use loom_worker::acp::session::Transport;

/// The marker a fake agent touches when it is asked to run a turn.
fn prompted_marker(script: &Path) -> PathBuf {
    PathBuf::from(format!("{}.prompted", script.display()))
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// A v1 agent whose whole conversation is two replayed messages.
///
/// `session/load` publishes them *before* answering, which is the completion
/// boundary this loader relies on.
fn write_v1_agent(dir: &Path) -> PathBuf {
    write_script(
        dir,
        "v1-history-agent.sh",
        r#"#!/bin/sh
session_id=hist-session
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}\n' "$id"
      ;;
    session/load)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"first question"}}}}\n' "$session_id"
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"first answer"}}}}\n' "$session_id"
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id"
      ;;
    session/prompt)
      touch "$0.prompted"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#,
    )
}

/// The same conversation over v2, where the replay must be asked for.
fn write_v2_agent(dir: &Path) -> PathBuf {
    write_script(
        dir,
        "v2-history-agent.sh",
        r#"#!/bin/sh
session_id=hist-session
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([^,]*\),"method":.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  case "$method" in
    initialize)
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{"protocolVersion":2,"info":{"name":"fake-v2-agent","version":"1"},"capabilities":{"session":{}}}}'
      ;;
    session/resume)
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"'"$session_id"'","update":{"sessionUpdate":"user_message_chunk","messageId":"m1","content":{"type":"text","text":"first question"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"'"$session_id"'","update":{"sessionUpdate":"agent_message_chunk","messageId":"m2","content":{"type":"text","text":"first answer"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      ;;
    session/prompt)
      touch "$0.prompted"
      printf '%s\n' '{"jsonrpc":"2.0","id":'"$id"',"result":{}}'
      ;;
  esac
done
"#,
    )
}

fn limits() -> HistoryLimits {
    HistoryLimits {
        max_total_bytes: 1024 * 1024,
        budget: Duration::from_secs(20),
    }
}

fn user_texts(entries: &[ProviderEvent]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ItemStarted {
                item: ThreadEventItem::UserMessage { content, .. },
                ..
            } => content.iter().find_map(|part| match part {
                UserContent::Text { text } => Some(text.clone()),
                _ => None,
            }),
            _ => None,
        })
        .collect()
}

fn assistant_text(entries: &[ProviderEvent]) -> String {
    entries
        .iter()
        .filter_map(|event| match event {
            ProviderEvent::ItemAgentMessageDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_v1_load_replays_the_conversation_without_prompting() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let agent = write_v1_agent(tmp.path());

    let entries = load_history(
        Transport::Stdio {
            command: agent.to_string_lossy().into_owned(),
            args: Vec::new(),
        },
        cwd.to_string_lossy().into_owned(),
        ThreadId::mint(),
        "hist-session".into(),
        limits(),
    )
    .await
    .expect("the fake agent replays its conversation");

    assert_eq!(user_texts(&entries), vec!["first question".to_owned()]);
    assert_eq!(assistant_text(&entries), "first answer");
    assert!(
        !prompted_marker(&agent).exists(),
        "loading history must not send a prompt"
    );
}

#[tokio::test]
async fn a_v2_load_asks_for_the_replay_and_does_not_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let agent = write_v2_agent(tmp.path());

    let entries = load_history(
        Transport::Stdio {
            command: agent.to_string_lossy().into_owned(),
            args: Vec::new(),
        },
        cwd.to_string_lossy().into_owned(),
        ThreadId::mint(),
        "hist-session".into(),
        limits(),
    )
    .await
    .expect("the fake agent replays its conversation");

    // An empty result would mean `replayFrom` was omitted: v2 restores the
    // session and hands back no history, which looks like an empty thread.
    assert_eq!(user_texts(&entries), vec!["first question".to_owned()]);
    assert_eq!(assistant_text(&entries), "first answer");
    assert!(
        !prompted_marker(&agent).exists(),
        "loading history must not send a prompt"
    );
}

#[tokio::test]
async fn an_over_budget_conversation_fails_rather_than_truncating() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let agent = write_v1_agent(tmp.path());

    let failure = load_history(
        Transport::Stdio {
            command: agent.to_string_lossy().into_owned(),
            args: Vec::new(),
        },
        cwd.to_string_lossy().into_owned(),
        ThreadId::mint(),
        "hist-session".into(),
        HistoryLimits {
            // Smaller than the two replayed messages.
            max_total_bytes: 16,
            budget: Duration::from_secs(20),
        },
    )
    .await
    .expect_err("a conversation past the budget must fail");

    assert_eq!(failure.code, "too_large");
}

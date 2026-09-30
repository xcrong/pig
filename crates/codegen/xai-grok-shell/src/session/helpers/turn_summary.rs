//! After each turn the shell generates an ultra-short one-line summary of the agent's reply for that turn (not a meta activity log).
//! The dashboard row shows it as its secondary line.
//! Like recap, it is display-only and never mutates the conversation.
//! The request carries only the last user message and the agent's visible text after it, so its cost does not grow with the session.

use crate::sampling::ConversationItem;
use crate::session::helpers::chat::floor_char_boundary;

/// The instruction targets 5-12 words; this only guards against runaway output.
/// Rows truncate to width on render.
pub(crate) const TURN_SUMMARY_MAX_CHARS: usize = 200;

/// Max characters of the user message quoted in the instruction as the last-turn anchor.
const ANCHOR_MAX_CHARS: usize = 120;

/// The message that opened the latest prompt turn and the agent's visible text after it.
/// The opener can be a real prompt or a server-initiated wake (scheduler, task or subagent completion, agent message).
/// Reasoning, tool calls, tool results, and mid-turn injected user-role items are dropped.
/// The user message keeps its first `user_max_chars`, the reply its last `reply_max_chars`.
/// `None` when no turn has started or the agent wrote no text in it.
pub(crate) fn last_turn(
    conversation: &[ConversationItem],
    user_max_chars: usize,
    reply_max_chars: usize,
) -> Option<(String, String)> {
    let (start, opener) = conversation
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| {
            matches!(item, ConversationItem::User(u) if u.synthetic_reason.starts_prompt_turn())
        })?;
    let user_text = opener.text_content();
    let user_text = match user_text.trim() {
        "" => "(no text; attachments only)",
        text => text,
    };
    let reply = conversation
        .iter()
        .skip(start + 1)
        .filter_map(|item| match item {
            ConversationItem::Assistant(a) if !a.content.trim().is_empty() => {
                Some(a.content.trim())
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    if reply.is_empty() {
        return None;
    }
    Some((
        keep_head(user_text, user_max_chars),
        keep_tail(&reply, reply_max_chars),
    ))
}

fn keep_head(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let head = text.get(..floor_char_boundary(text, max)).unwrap_or(text);
    format!("{head}\u{2026}")
}

fn keep_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = text.get(start..).unwrap_or(text);
    format!("\u{2026}{tail}")
}

/// Bridge: the daemon path still replays the conversation prefix with an anchor reminder turn.
/// New shell call sites use [`last_turn`] with [`TURN_SUMMARY_SYSTEM`] instead.

/// The conversation contains user-role turns the user never wrote (reminders, injected context).
/// Angle brackets are dropped so the quote cannot close the instruction's reminder tag.
/// `None` when no real user message with text exists (caller should skip generation).
pub(crate) fn last_user_anchor(conversation: &[ConversationItem]) -> Option<String> {
    let text = conversation.iter().rev().find_map(|item| match item {
        ConversationItem::User(u) if u.synthetic_reason.is_human() => {
            let text = item.text_content();
            (!text.trim().is_empty()).then_some(text)
        }
        _ => None,
    })?;
    let mut anchor: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| *c != '<' && *c != '>')
        .collect();
    if anchor.len() > ANCHOR_MAX_CHARS {
        let cut = floor_char_boundary(&anchor, ANCHOR_MAX_CHARS);
        anchor.truncate(cut);
        anchor = anchor.trim_end().to_string();
        anchor.push('\u{2026}');
    }
    Some(anchor)
}

/// Same single-user-message design as recap (`recap_instruction`): all directions live in one reminder-wrapped turn.
/// The conversation prefix is then reused verbatim, so the prompt cache stays warm.
/// Few-shots must stay synthetic: never embed real eval/session content.
/// Bridge: kept for the daemon path; new shell call sites use [`TURN_SUMMARY_SYSTEM`] plus [`turn_summary_user_message`].
pub(crate) fn turn_summary_instruction(tag: &str, anchor: &str) -> String {
    format!(
        "<{tag}>Write an ultra-short dashboard line that captures the AGENT'S REPLY for the \
         last turn only — everything after the user message beginning: \"{anchor}\". \
         Focus on what the assistant concluded, answered, recommended, or delivered — not a \
         meta description of the turn (avoid \"Explained…\", \"Answered…\", \"Greeted…\", \
         \"Reviewed…\"). User-role messages wrapped in reminder tags like this one are \
         injected context, not the user.\n\n\
         Output ONLY the fragment: 5-12 words, plain text, glanceable on a status row. \
         Prefer the payload: answer, finding, change, or decision needed. \
         Do NOT call any tools — respond with plain text only.\n\n\
         Synthetic examples (style only — adapt to THIS turn, do not copy):\n\
         `queue_worker` shutdown race fixed; suite green\n\
         Payment retries: exp backoff in `billing/retry.rs`, 5× on 429\n\
         Retry backoff wired into `billing/retry.rs`; tests pending\n\
         Need decision: keep or drop `sqlx` cache before refactor\n\
         Black — matches the terminal aesthetic\n\n\
         Bad (never):\n\
         - Lead with Explained / Answered / Greeted / Reviewed / Confirmed / Flagged / Summarized\n\
         - Labels, quotes, bullets, markdown, code fences, multi-sentence dumps\n\
         - Filler like \"no code changes\" or \"awaiting task\" unless that is the whole point\n\
         - Summarize earlier turns or the whole session\n\
         - Call tools or invent content not in the agent's reply</{tag}>"
    )
}

pub(crate) const TURN_SUMMARY_SYSTEM: &str = "Write an ultra-short dashboard line that captures the AGENT'S REPLY \
     to the user's message. Focus on what the agent concluded, answered, recommended, or delivered, \
     not a meta description of the turn (avoid \"Explained…\", \"Answered…\", \"Greeted…\", \
     \"Reviewed…\").\n\n\
     Output ONLY the fragment: 5-12 words, plain text, glanceable on a status row. \
     Prefer the payload: answer, finding, change, or decision needed.\n\n\
     Synthetic examples (style only, do not copy):\n\
     `queue_worker` shutdown race fixed; suite green\n\
     Payment retries: exp backoff in `billing/retry.rs`, 5× on 429\n\
     Retry backoff wired into `billing/retry.rs`; tests pending\n\
     Need decision: keep or drop `sqlx` cache before refactor\n\
     Black — matches the terminal aesthetic\n\n\
     Bad (never):\n\
     - Lead with Explained / Answered / Greeted / Reviewed / Confirmed / Flagged / Summarized\n\
     - Labels, quotes, bullets, markdown, code fences, multi-sentence dumps\n\
     - Filler like \"no code changes\" or \"awaiting task\" unless that is the whole point\n\
     - Invent content not in the agent's reply";

pub(crate) fn turn_summary_user_message(user_text: &str, reply: &str) -> String {
    format!(
        "<user_message>\n{user_text}\n</user_message>\n\n<agent_reply>\n{reply}\n</agent_reply>"
    )
}

/// Clean the model's raw output into a one-line fragment.
/// Recap normalization (whitespace collapse, stray label/quote stripping) runs first, then the tighter [`TURN_SUMMARY_MAX_CHARS`] cap.
pub(crate) fn clean_turn_summary_text(raw: &str) -> String {
    let mut out = super::session_recap::clean_recap_text(raw);
    if out.len() > TURN_SUMMARY_MAX_CHARS {
        let cut = floor_char_boundary(&out, TURN_SUMMARY_MAX_CHARS);
        out.truncate(cut);
        out = out.trim_end().to_string();
        out.push('\u{2026}');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ConversationItem {
        ConversationItem::user(text.to_string())
    }

    fn synthetic_user(text: &str) -> ConversationItem {
        use xai_grok_sampling_types::{ContentPart, SyntheticReason, UserItem};
        ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: std::sync::Arc::from(text),
            }],
            synthetic_reason: SyntheticReason::SystemReminder,
            ..Default::default()
        })
    }

    #[test]
    fn anchor_skips_synthetic_user_turns() {
        let conv = vec![
            ConversationItem::system("sys".to_string()),
            user("fix the parser"),
            ConversationItem::assistant("done".to_string()),
            synthetic_user("<system-reminder>injected</system-reminder>"),
        ];
        assert_eq!(last_user_anchor(&conv).as_deref(), Some("fix the parser"));
    }

    #[test]
    fn anchor_none_without_real_user_message() {
        let conv = vec![
            ConversationItem::system("sys".to_string()),
            synthetic_user("injected"),
        ];
        assert_eq!(last_user_anchor(&conv), None);
        assert_eq!(last_user_anchor(&[user("   \n ")]), None);
    }

    #[test]
    fn anchor_collapses_drops_angle_brackets_and_truncates() {
        let long = format!("review <the>   plan\n{}", "x".repeat(200));
        let anchor = last_user_anchor(&[user(&long)]).unwrap();
        assert!(anchor.starts_with("review the plan"));
        assert!(!anchor.contains('<') && !anchor.contains('>'));
        assert!(anchor.ends_with('\u{2026}'));
        assert!(anchor.chars().count() <= ANCHOR_MAX_CHARS + 1);
    }

    #[test]
    fn instruction_embeds_tag_and_anchor() {
        let text = turn_summary_instruction("system-reminder", "fix the parser");
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.ends_with("</system-reminder>"));
        assert!(text.contains("beginning: \"fix the parser\""));
    }

    const USER_MAX: usize = 4_000;
    const REPLY_MAX: usize = 32_000;

    #[test]
    fn last_turn_keeps_user_message_and_visible_agent_text() {
        use xai_grok_sampling_types::{ContentPart, SyntheticReason, UserItem};
        let conv = vec![
            ConversationItem::system("sys"),
            user("old question"),
            ConversationItem::assistant("old answer"),
            user("fix the parser"),
            ConversationItem::assistant("Looking at the parser."),
            ConversationItem::tool_result("call-1", "tool output"),
            ConversationItem::system_reminder("injected"),
            ConversationItem::assistant(format!("{}Fixed the parser.", "x".repeat(REPLY_MAX))),
        ];
        let (user_text, reply) = last_turn(&conv, USER_MAX, REPLY_MAX).unwrap();
        assert_eq!(user_text, "fix the parser");
        assert!(reply.starts_with('\u{2026}'));
        assert!(reply.ends_with("Fixed the parser."));
        assert!(!reply.contains("tool output") && !reply.contains("injected"));

        assert_eq!(
            last_turn(
                &[user("fix it"), ConversationItem::tool_result("c", "out")],
                USER_MAX,
                REPLY_MAX
            ),
            None
        );
        assert_eq!(
            last_turn(
                &[ConversationItem::system_reminder("injected")],
                USER_MAX,
                REPLY_MAX
            ),
            None
        );

        let image_only = ConversationItem::user_with_parts(vec![ContentPart::Image {
            url: "data:image/png;base64,AA".into(),
        }]);
        let wake = ConversationItem::User(UserItem {
            content: vec![ContentPart::Text {
                text: "background task finished".into(),
            }],
            synthetic_reason: SyntheticReason::TaskCompleted,
            ..Default::default()
        });
        for (opener, expected) in [
            (image_only, "(no text; attachments only)"),
            (wake, "background task finished"),
        ] {
            let conv = vec![
                user("old question"),
                ConversationItem::assistant("old answer"),
                opener,
                ConversationItem::assistant("new answer"),
            ];
            let (user_text, reply) = last_turn(&conv, USER_MAX, REPLY_MAX).unwrap();
            assert_eq!(user_text, expected);
            assert_eq!(reply, "new answer");
        }
    }

    #[test]
    fn clean_normalizes_and_caps() {
        assert_eq!(
            clean_turn_summary_text("Summary: \"Fixed the\n\n  parser\""),
            "Fixed the parser"
        );
        let capped = clean_turn_summary_text(&"word ".repeat(100));
        assert!(capped.len() <= TURN_SUMMARY_MAX_CHARS + '\u{2026}'.len_utf8());
        assert!(capped.ends_with('\u{2026}'));
    }
}

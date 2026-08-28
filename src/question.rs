//! Interactive Telegram questions for MCP clients.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{self, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{config, conversation, telegram};

const POLL_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_TIMEOUT_SECS: u64 = 2 * 60 * 60;
pub const MAX_TIMEOUT_SECS: u64 = 2 * 60 * 60;
const MAX_CHOICES: usize = 8;
const ACTION_HEADER: &str = "🚨 Action needed";
const ANSWERED_HEADER: &str = "🟢 Action answered";
const EXPIRED_HEADER: &str = "🟠 Action expired";
const CANCELLED_HEADER: &str = "🟠 Action cancelled";
pub(crate) const MESSAGE_DIVIDER: &str = "──────────";
const QUESTION_EDIT_ATTEMPTS: usize = 3;
const QUESTION_EDIT_TIMEOUT: Duration = Duration::from_secs(5);
const QUESTION_EDIT_RETRY_DELAY: Duration = Duration::from_millis(300);

pub(crate) fn ask_with_cancellation(
    question: &str,
    choices: &[String],
    timeout_secs: u64,
    conversation_title: Option<&str>,
    is_cancelled: impl Fn() -> bool,
) -> Result<String> {
    let question = question.trim();
    let conversation_title = conversation_title.map(conversation::title).transpose()?;
    ensure!(!question.is_empty(), "question must not be empty");
    ensure!(
        question.chars().count() <= 3000,
        "question must be at most 3000 characters"
    );
    ensure!(
        (2..=MAX_CHOICES).contains(&choices.len()),
        "choices must contain between 2 and {MAX_CHOICES} items"
    );
    ensure!(
        (30..=MAX_TIMEOUT_SECS).contains(&timeout_secs),
        "timeout_seconds must be between 30 and {MAX_TIMEOUT_SECS}"
    );

    let choices = choices
        .iter()
        .map(|choice| choice.trim().to_string())
        .collect::<Vec<_>>();
    ensure!(
        choices
            .iter()
            .all(|choice| !choice.is_empty() && choice.chars().count() <= 64),
        "each choice must contain between 1 and 64 characters"
    );
    ensure!(
        !is_cancelled(),
        "Telegram question was cancelled before it was sent"
    );

    let token = config::read_token()?;
    let chat = telegram::chat_id(&token)?;
    let user_id = ensure_private_chat(&token, &chat)?;
    let lock = AskLock::acquire(&token)?;

    // Ignore updates that arrived before this question was sent.
    let mut offset = pending_offset(&token, user_id)?;
    ensure!(
        !is_cancelled(),
        "Telegram question was cancelled before it was sent"
    );
    let request_id = request_id();
    let sent = telegram::call(
        &token,
        "sendMessage",
        Some(json!({
            "chat_id": chat,
            "text": pending_text(question, conversation_title),
            "reply_markup": {
                "inline_keyboard": choices.iter().enumerate().map(|(index, choice)| {
                    vec![json!({
                        "text": choice,
                        "callback_data": format!("talbot:{request_id}:{index}")
                    })]
                }).collect::<Vec<_>>()
            }
        })),
    )?;
    let message_id = sent["message_id"]
        .as_i64()
        .context("sendMessage response is missing message_id")?;
    lock.record_question(&chat, message_id)?;
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    loop {
        if is_cancelled() {
            return cancel_question(&token, &chat, message_id, question, conversation_title);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let wait = wait_label(timeout_secs);
            return match expire_question(
                &token,
                &chat,
                message_id,
                question,
                conversation_title,
                &wait,
            ) {
                Ok(()) => Err(anyhow::anyhow!(
                    "Telegram question expired after {wait}; the buttons were removed. \
                     Do not assume an answer or continue work that depends on one"
                )),
                Err(error) => {
                    eprintln!("talbot: could not update the expired Telegram question: {error:#}");
                    Err(anyhow::anyhow!(
                        "Telegram question expired after {wait}, but its Telegram message \
                         could not be updated. The request is inactive; do not assume an \
                         answer or continue work that depends on one"
                    ))
                }
            };
        }
        let poll_secs = remaining.as_secs().clamp(1, POLL_TIMEOUT_SECS);
        let updates = match get_updates(&token, offset, poll_secs) {
            Ok(updates) => updates,
            Err(_) if is_cancelled() => {
                return cancel_question(&token, &chat, message_id, question, conversation_title);
            }
            Err(error) => return Err(error),
        };
        if is_cancelled() {
            return cancel_question(&token, &chat, message_id, question, conversation_title);
        }
        if let Some(next) = next_offset(&updates) {
            config::write_update_offset(next)?;
            offset = Some(next);
        }

        for update in &updates {
            let Some(answer) = parse_answer(update, user_id, &request_id, &choices) else {
                continue;
            };
            if is_cancelled() {
                return cancel_question(&token, &chat, message_id, question, conversation_title);
            }
            acknowledge_answer(
                &token,
                &chat,
                message_id,
                question,
                conversation_title,
                &answer,
            );
            return Ok(answer.text().to_string());
        }
    }
}

fn cancel_question(
    token: &str,
    chat: &str,
    message_id: i64,
    question: &str,
    conversation_title: Option<&str>,
) -> Result<String> {
    match edit_question_with_retry(
        token,
        chat,
        message_id,
        &cancelled_text(question, conversation_title),
    ) {
        Ok(()) => bail!(
            "Telegram question was cancelled because Codex stopped waiting; the buttons were removed"
        ),
        Err(error) => {
            eprintln!("talbot: could not update the cancelled Telegram question: {error:#}");
            bail!(
                "Telegram question was cancelled because Codex stopped waiting, but its Telegram message could not be updated. The request is inactive"
            )
        }
    }
}

fn ensure_private_chat(token: &str, chat: &str) -> Result<i64> {
    let details = telegram::call(token, "getChat", Some(json!({ "chat_id": chat })))?;
    ensure!(
        details["type"].as_str() == Some("private"),
        "interactive questions require a private Telegram chat"
    );
    details["id"]
        .as_i64()
        .context("getChat response is missing a numeric id")
}

fn pending_offset(token: &str, user_id: i64) -> Result<Option<i64>> {
    let mut offset = config::read_update_offset();
    for _ in 0..100 {
        let updates = get_updates(token, offset, 0)?;
        if updates.is_empty() {
            return Ok(offset);
        }
        for update in &updates {
            dismiss_stale_callback(token, update, user_id);
        }
        if let Some(next) = next_offset(&updates) {
            config::write_update_offset(next)?;
            offset = Some(next);
        }
        if updates.len() < 100 {
            return Ok(offset);
        }
    }
    bail!("too many pending Telegram updates; send the question again")
}

fn get_updates(token: &str, offset: Option<i64>, timeout_secs: u64) -> Result<Vec<Value>> {
    let mut body = json!({
        "timeout": timeout_secs,
        "limit": 100,
        "allowed_updates": ["message", "callback_query"]
    });
    if let Some(offset) = offset {
        body["offset"] = json!(offset);
    }
    let result = telegram::call(token, "getUpdates", Some(body))?;
    result
        .as_array()
        .cloned()
        .context("getUpdates response is not an array")
}

fn next_offset(updates: &[Value]) -> Option<i64> {
    updates
        .iter()
        .filter_map(|update| update["update_id"].as_i64())
        .max()
        .map(|id| id + 1)
}

fn request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}{nanos:x}", std::process::id())
}

fn expire_question(
    token: &str,
    chat: &str,
    message_id: i64,
    question: &str,
    conversation_title: Option<&str>,
    wait: &str,
) -> Result<()> {
    edit_question_with_retry(
        token,
        chat,
        message_id,
        &expired_text(question, conversation_title, wait),
    )
}

fn edit_question_with_retry(token: &str, chat: &str, message_id: i64, text: &str) -> Result<()> {
    let body = json!({
        "chat_id": chat,
        "message_id": message_id,
        "text": text,
        "reply_markup": { "inline_keyboard": [] }
    });
    call_question_edit_with_retry(token, "editMessageText", body)
}

fn remove_question_buttons_with_retry(token: &str, chat: i64, message_id: i64) -> Result<()> {
    let body = json!({
        "chat_id": chat,
        "message_id": message_id,
        "reply_markup": { "inline_keyboard": [] }
    });
    call_question_edit_with_retry(token, "editMessageReplyMarkup", body)
}

fn call_question_edit_with_retry(token: &str, method: &str, body: Value) -> Result<()> {
    let mut last_error = None;

    for attempt in 0..QUESTION_EDIT_ATTEMPTS {
        match telegram::call_with_timeout(token, method, Some(body.clone()), QUESTION_EDIT_TIMEOUT)
        {
            Ok(_) => return Ok(()),
            Err(error) if error.to_string().contains("message is not modified") => return Ok(()),
            Err(error) => last_error = Some(error),
        }

        if attempt + 1 < QUESTION_EDIT_ATTEMPTS {
            std::thread::sleep(QUESTION_EDIT_RETRY_DELAY * (attempt as u32 + 1));
        }
    }

    Err(last_error.expect("question edit attempted at least once"))
}

fn acknowledge_answer(
    token: &str,
    chat: &str,
    message_id: i64,
    question: &str,
    conversation_title: Option<&str>,
    answer: &IncomingAnswer,
) {
    if let IncomingAnswer::Choice {
        callback_query_id, ..
    } = answer
        && let Err(error) = telegram::call(
            token,
            "answerCallbackQuery",
            Some(json!({
                "callback_query_id": callback_query_id,
                "text": "🟢 Answer received."
            })),
        )
    {
        eprintln!("talbot: could not dismiss the Telegram button spinner: {error:#}");
    }

    if let Err(error) = mark_answered(
        token,
        chat,
        message_id,
        question,
        conversation_title,
        answer.text(),
    ) {
        eprintln!("talbot: could not update the answered Telegram question: {error:#}");
    }
}

fn mark_answered(
    token: &str,
    chat: &str,
    message_id: i64,
    question: &str,
    conversation_title: Option<&str>,
    answer: &str,
) -> Result<()> {
    edit_question_with_retry(
        token,
        chat,
        message_id,
        &answered_text(question, conversation_title, answer),
    )
}

pub(crate) fn action_required_text_with_title(title: &str, message: &str) -> String {
    let message = message
        .trim()
        .strip_prefix(ACTION_HEADER)
        .map(str::trim_start)
        .unwrap_or_else(|| message.trim());
    conversation::status_text(ACTION_HEADER, Some(title), message)
}

fn pending_text(question: &str, conversation_title: Option<&str>) -> String {
    conversation::status_text(ACTION_HEADER, conversation_title, question)
}

fn answered_text(question: &str, conversation_title: Option<&str>, answer: &str) -> String {
    conversation::status_text(
        ANSWERED_HEADER,
        conversation_title,
        &format!(
            "You chose: {}\n\n{MESSAGE_DIVIDER}\n{question}",
            answer_preview(answer),
        ),
    )
}

fn answer_preview(answer: &str) -> String {
    const MAX_CHARS: usize = 512;
    if answer.chars().count() <= MAX_CHARS {
        answer.to_string()
    } else {
        let suffix = "…";
        let keep = MAX_CHARS - suffix.chars().count();
        format!(
            "{}{}",
            answer.chars().take(keep).collect::<String>(),
            suffix
        )
    }
}

fn expired_text(question: &str, conversation_title: Option<&str>, wait: &str) -> String {
    conversation::status_text(
        EXPIRED_HEADER,
        conversation_title,
        &format!(
            "Codex stopped waiting after {wait}. Go back to Codex to try again.\n\n{MESSAGE_DIVIDER}\n{question}"
        ),
    )
}

fn cancelled_text(question: &str, conversation_title: Option<&str>) -> String {
    conversation::status_text(
        CANCELLED_HEADER,
        conversation_title,
        &format!(
            "Codex moved on, so this question is no longer active. Go back to Codex to try again.\n\n{MESSAGE_DIVIDER}\n{question}"
        ),
    )
}

fn dismiss_stale_callback(token: &str, update: &Value, user_id: i64) {
    if id_at(update, "/callback_query/message/chat/id") != Some(user_id)
        || id_at(update, "/callback_query/from/id") != Some(user_id)
    {
        return;
    }

    let Some(callback_query_id) = update.pointer("/callback_query/id").and_then(Value::as_str)
    else {
        return;
    };

    if let Err(error) = telegram::call_with_timeout(
        token,
        "answerCallbackQuery",
        Some(json!({
            "callback_query_id": callback_query_id,
            "text": "🟠 This action expired. Go back to Codex and try again."
        })),
        QUESTION_EDIT_TIMEOUT,
    ) {
        eprintln!("talbot: could not dismiss an expired Telegram button: {error:#}");
    }

    let Some(chat) = update
        .pointer("/callback_query/message/chat/id")
        .and_then(Value::as_i64)
    else {
        return;
    };
    let Some(message_id) = update
        .pointer("/callback_query/message/message_id")
        .and_then(Value::as_i64)
    else {
        return;
    };
    if let Err(error) = remove_question_buttons_with_retry(token, chat, message_id) {
        eprintln!("talbot: could not remove buttons from an inactive question: {error:#}");
    }
}

fn wait_label(seconds: u64) -> String {
    if seconds.is_multiple_of(3600) {
        plural(seconds / 3600, "hour")
    } else if seconds.is_multiple_of(60) {
        plural(seconds / 60, "minute")
    } else {
        plural(seconds, "second")
    }
}

fn plural(value: u64, unit: &str) -> String {
    let suffix = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{suffix}")
}

#[derive(Debug, PartialEq, Eq)]
enum IncomingAnswer {
    Choice {
        text: String,
        callback_query_id: String,
    },
    Text(String),
}

impl IncomingAnswer {
    fn text(&self) -> &str {
        match self {
            Self::Choice { text, .. } | Self::Text(text) => text,
        }
    }
}

fn parse_answer(
    update: &Value,
    user_id: i64,
    request_id: &str,
    choices: &[String],
) -> Option<IncomingAnswer> {
    if id_at(update, "/callback_query/message/chat/id") == Some(user_id)
        && id_at(update, "/callback_query/from/id") == Some(user_id)
    {
        let data = update.pointer("/callback_query/data")?.as_str()?;
        let index = data
            .strip_prefix(&format!("talbot:{request_id}:"))?
            .parse::<usize>()
            .ok()?;
        let text = choices.get(index)?.clone();
        let callback_query_id = update.pointer("/callback_query/id")?.as_str()?.to_string();
        return Some(IncomingAnswer::Choice {
            text,
            callback_query_id,
        });
    }

    if id_at(update, "/message/chat/id") == Some(user_id)
        && id_at(update, "/message/from/id") == Some(user_id)
    {
        let text = update.pointer("/message/text")?.as_str()?.trim();
        if !text.is_empty() {
            return Some(IncomingAnswer::Text(text.to_string()));
        }
    }
    None
}

fn id_at(value: &Value, pointer: &str) -> Option<i64> {
    value.pointer(pointer)?.as_i64()
}

struct AskLock {
    path: PathBuf,
}

impl AskLock {
    fn acquire(token: &str) -> Result<Self> {
        let path = config::dir()?.join("ask.lock");
        for attempt in 0..2 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && is_stale(&path) {
                        mark_abandoned(token, &path);
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    bail!("another Telegram question is already waiting for an answer");
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot create {}", path.display()));
                }
            }
        }
        unreachable!()
    }

    fn record_question(&self, chat: &str, message_id: i64) -> Result<()> {
        fs::write(
            &self.path,
            json!({
                "pid": process::id(),
                "chat_id": chat,
                "message_id": message_id
            })
            .to_string(),
        )
        .with_context(|| format!("cannot update {}", self.path.display()))
    }
}

impl Drop for AskLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn is_stale(path: &Path) -> bool {
    if let Ok(owner) = fs::read_to_string(path)
        && let Some(pid) = lock_owner_pid(&owner)
        && !process_is_alive(pid)
    {
        return true;
    }

    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .is_ok_and(|age| age > Duration::from_secs(MAX_TIMEOUT_SECS + 300))
}

fn lock_owner_pid(contents: &str) -> Option<u32> {
    contents.trim().parse::<u32>().ok().or_else(|| {
        serde_json::from_str::<Value>(contents)
            .ok()?
            .get("pid")?
            .as_u64()?
            .try_into()
            .ok()
    })
}

fn mark_abandoned(token: &str, path: &Path) {
    let Some((chat, message_id)) = fs::read_to_string(path)
        .ok()
        .and_then(|contents| abandoned_message_target(&contents))
    else {
        return;
    };
    if let Err(error) = edit_question_with_retry(token, &chat, message_id, abandoned_text()) {
        eprintln!("talbot: could not close an abandoned Telegram question: {error:#}");
    }
}

fn abandoned_message_target(contents: &str) -> Option<(String, i64)> {
    let value = serde_json::from_str::<Value>(contents).ok()?;
    Some((
        value.get("chat_id")?.as_str()?.to_string(),
        value.get("message_id")?.as_i64()?,
    ))
}

fn abandoned_text() -> &'static str {
    "🟠 Action expired\n\nCodex is no longer waiting. Nothing was approved.\n\nGo back to Codex to try again."
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // Preserve the age-based fallback on platforms where this tiny CLI does
    // not yet have a portable process-liveness probe.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices() -> Vec<String> {
        vec!["First".to_string(), "Second".to_string()]
    }

    #[test]
    fn advances_past_the_latest_update() {
        let updates = vec![json!({ "update_id": 7 }), json!({ "update_id": 11 })];
        assert_eq!(next_offset(&updates), Some(12));
        assert_eq!(next_offset(&[]), None);
    }

    #[test]
    fn parses_matching_button_answer() {
        let update = json!({
            "callback_query": {
                "id": "callback-1",
                "from": { "id": 42 },
                "message": { "chat": { "id": 42 } },
                "data": "talbot:request-1:1"
            }
        });
        assert_eq!(
            parse_answer(&update, 42, "request-1", &choices()),
            Some(IncomingAnswer::Choice {
                text: "Second".to_string(),
                callback_query_id: "callback-1".to_string()
            })
        );
    }

    #[test]
    fn rejects_button_from_another_user_or_request() {
        let wrong_user = json!({
            "callback_query": {
                "id": "callback-1",
                "from": { "id": 99 },
                "message": { "chat": { "id": 42 } },
                "data": "talbot:request-1:0"
            }
        });
        let wrong_request = json!({
            "callback_query": {
                "id": "callback-1",
                "from": { "id": 42 },
                "message": { "chat": { "id": 42 } },
                "data": "talbot:old-request:0"
            }
        });
        assert_eq!(parse_answer(&wrong_user, 42, "request-1", &choices()), None);
        assert_eq!(
            parse_answer(&wrong_request, 42, "request-1", &choices()),
            None
        );
    }

    #[test]
    fn parses_text_from_the_configured_private_chat() {
        let update = json!({
            "message": {
                "from": { "id": 42 },
                "chat": { "id": 42 },
                "text": "A custom answer"
            }
        });
        assert_eq!(
            parse_answer(&update, 42, "request-1", &choices()),
            Some(IncomingAnswer::Text("A custom answer".to_string()))
        );
    }

    #[test]
    fn formats_waits_for_expiry_messages() {
        assert_eq!(wait_label(30), "30 seconds");
        assert_eq!(wait_label(60), "1 minute");
        assert_eq!(wait_label(15 * 60), "15 minutes");
        assert_eq!(wait_label(2 * 60 * 60), "2 hours");
    }

    #[test]
    fn marks_messages_that_need_an_answer() {
        assert_eq!(
            pending_text("Deploy the update now?", None),
            "🚨 Action needed\n\nDeploy the update now?"
        );
        assert_eq!(
            action_required_text_with_title("Finance Page", "🚨 Action needed\n\nAlready marked"),
            "🚨 Action needed\n\nFinance Page\n\nAlready marked"
        );
    }

    #[test]
    fn puts_the_conversation_title_after_the_action_marker() {
        assert_eq!(
            pending_text("Deploy the update now?", Some("Finance Page")),
            "🚨 Action needed\n\nFinance Page\n\nDeploy the update now?"
        );
    }

    #[test]
    fn turns_answered_messages_green() {
        assert_eq!(
            answered_text("Deploy the update now?", None, "Yes"),
            "🟢 Action answered\n\nYou chose: Yes\n\n──────────\nDeploy the update now?"
        );
        assert_eq!(
            answered_text("Deploy the update now?", Some("Finance Page"), "Yes"),
            "🟢 Action answered\n\nFinance Page\n\nYou chose: Yes\n\n──────────\nDeploy the update now?"
        );
    }

    #[test]
    fn uses_ten_second_telegram_long_polls() {
        assert_eq!(POLL_TIMEOUT_SECS, 10);
    }

    #[test]
    fn recognizes_the_current_process_as_a_live_lock_owner() {
        assert!(process_is_alive(process::id()));
    }

    #[test]
    fn reads_legacy_and_structured_lock_owners() {
        assert_eq!(lock_owner_pid("42\n"), Some(42));
        assert_eq!(
            lock_owner_pid(r#"{"pid":42,"chat_id":"7","message_id":9}"#),
            Some(42)
        );
        assert_eq!(lock_owner_pid("not a lock"), None);
    }

    #[test]
    fn recovers_an_abandoned_question_target() {
        assert_eq!(
            abandoned_message_target(r#"{"pid":42,"chat_id":"7","message_id":9}"#),
            Some(("7".to_string(), 9))
        );
        assert!(abandoned_message_target("42").is_none());
        assert!(abandoned_text().starts_with("🟠 Action expired"));
        assert!(abandoned_text().contains("Nothing was approved"));
    }

    #[test]
    fn expired_message_is_unambiguous() {
        assert_eq!(
            expired_text("Deploy now?", None, "2 hours"),
            "🟠 Action expired\n\nCodex stopped waiting after 2 hours. Go back to Codex to try again.\n\n──────────\nDeploy now?"
        );
        assert_eq!(
            expired_text("Deploy now?", Some("Finance Page"), "2 hours"),
            "🟠 Action expired\n\nFinance Page\n\nCodex stopped waiting after 2 hours. Go back to Codex to try again.\n\n──────────\nDeploy now?"
        );
    }

    #[test]
    fn cancelled_message_closes_the_old_action() {
        assert_eq!(
            cancelled_text("Deploy now?", Some("Finance Page")),
            "🟠 Action cancelled\n\nFinance Page\n\nCodex moved on, so this question is no longer active. Go back to Codex to try again.\n\n──────────\nDeploy now?"
        );
    }
}

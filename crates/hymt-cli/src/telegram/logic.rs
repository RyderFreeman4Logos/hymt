//! Pure claim, authorization, and CN↔EN routing for the Telegram bot.

use hymt_core::config::TelegramMode;
use hymt_core::language::detect_target_language;
use hymt_core::language_spec::normalize_language_code;

/// Kind of Telegram chat we care about for v1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatKind {
    Private,
    Group,
    Other,
}

/// Normalized inbound text message (no Telegram SDK types).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingTextMessage {
    pub chat_id: i64,
    pub chat_kind: ChatKind,
    pub text: String,
}

/// Authorization outcome before translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    Allowed,
    Denied,
}

/// Side effects the poll loop should perform (no I/O here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotAction {
    /// Reply with a short help /start message.
    Help,
    /// Claim succeeded; caller should persist owner id.
    Claimed { chat_id: i64 },
    /// Owner already claimed; acknowledge without re-adding.
    AlreadyOwner,
    /// Unauthorized chat; short denial.
    Denied,
    /// Translate `text` toward `target_lang` and reply with the result.
    Translate { text: String, target_lang: String },
    /// Empty or unsupported; no reply.
    Ignore,
}

/// Constant-time-ish comparison for claim passwords.
///
/// Avoids early-exit on the first differing byte for equal-length secrets.
/// Length mismatches still short-circuit (length is not secret-sensitive once
/// the password is known to be fixed-width generated tokens).
pub fn claim_password_matches(provided: &str, expected: &str) -> bool {
    let provided = provided.trim();
    let expected = expected.trim();
    if expected.is_empty() || provided.len() != expected.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in provided.bytes().zip(expected.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Choose CN↔EN target for bot replies using CJK ratio against `primary`.
///
/// When the source looks predominantly like `primary` (default zh), target
/// `secondary` (default en); otherwise target `primary`.
pub fn cn_en_target_lang(text: &str, primary: &str, secondary: &str) -> String {
    let primary = canonical_or_default(primary, "zh");
    let secondary = canonical_or_default(secondary, "en");
    if let Some(det) = detect_target_language(text, &primary) {
        if det.target_ratio > hymt_core::language::TARGET_PARAGRAPH_RATIO {
            return secondary;
        }
    }
    primary
}

fn canonical_or_default(language: &str, default: &str) -> String {
    let requested = if language.trim().is_empty() {
        default
    } else {
        language
    };
    normalize_language_code(requested)
        .map(str::to_owned)
        .unwrap_or_else(|_| requested.trim().to_owned())
}

/// Authorize a chat under the configured mode.
pub fn authorize_chat(
    chat_kind: ChatKind,
    chat_id: i64,
    mode: TelegramMode,
    owners: &[i64],
    groups: &[i64],
) -> AuthDecision {
    match mode {
        TelegramMode::Owners => {
            if chat_kind == ChatKind::Private && owners.contains(&chat_id) {
                AuthDecision::Allowed
            } else {
                AuthDecision::Denied
            }
        }
        TelegramMode::Groups => {
            if matches!(chat_kind, ChatKind::Group) && groups.contains(&chat_id) {
                AuthDecision::Allowed
            } else if chat_kind == ChatKind::Private && owners.contains(&chat_id) {
                // Owners may still use private chat while group mode is active.
                AuthDecision::Allowed
            } else {
                AuthDecision::Denied
            }
        }
    }
}

/// Evaluate one inbound text message into a pure bot action.
pub fn evaluate_text_message(
    msg: &IncomingTextMessage,
    claim_password: &str,
    mode: TelegramMode,
    owners: &[i64],
    groups: &[i64],
    primary_lang: &str,
    secondary_lang: &str,
) -> BotAction {
    let text = msg.text.trim();
    if text.is_empty() {
        return BotAction::Ignore;
    }

    // Bot commands (Telegram may send "/start@BotName").
    let command = text.split_whitespace().next().unwrap_or("");
    let command_base = command.split('@').next().unwrap_or(command);
    if command_base.eq_ignore_ascii_case("/start") || command_base.eq_ignore_ascii_case("/help") {
        return BotAction::Help;
    }

    // Private-chat ownership claim: exact password (or /claim[@BotName] <password>).
    // Telegram may append @BotName to the command token, same as /start.
    if msg.chat_kind == ChatKind::Private {
        let claim_candidate = if command_base.eq_ignore_ascii_case("/claim") {
            text[command.len()..].trim()
        } else {
            text
        };
        if claim_password_matches(claim_candidate, claim_password) {
            if owners.contains(&msg.chat_id) {
                return BotAction::AlreadyOwner;
            }
            return BotAction::Claimed {
                chat_id: msg.chat_id,
            };
        }
    }

    match authorize_chat(msg.chat_kind, msg.chat_id, mode, owners, groups) {
        AuthDecision::Denied => BotAction::Denied,
        AuthDecision::Allowed => {
            if msg.chat_kind == ChatKind::Other {
                return BotAction::Ignore;
            }
            let target = cn_en_target_lang(text, primary_lang, secondary_lang);
            BotAction::Translate {
                text: text.to_owned(),
                target_lang: target,
            }
        }
    }
}

/// Short static help text (English; no secrets).
pub fn help_text() -> &'static str {
    "hymt Telegram bot\n\
     • Private: send the claim password from config to become an owner.\n\
     • Owners (and authorized groups) get automatic Chinese↔English translation.\n\
     • /start or /help show this message.\n\
     Unauthorized chats are denied."
}

/// Short denial message (no secrets, no chat ids).
pub fn denial_text() -> &'static str {
    "Unauthorized. Claim ownership in a private chat with the claim password from hymt config."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_password_matches_trims_and_is_exact() {
        assert!(claim_password_matches("  ABC123  ", "ABC123"));
        assert!(!claim_password_matches("abc123", "ABC123"));
        assert!(!claim_password_matches("ABC12", "ABC123"));
        assert!(!claim_password_matches("ABC123", ""));
    }

    #[test]
    fn cn_en_routes_cjk_to_english() {
        let target = cn_en_target_lang("这是一段中文测试文本用于检测", "zh", "en");
        assert_eq!(target, "en");
    }

    #[test]
    fn cn_en_routes_english_to_chinese() {
        let target = cn_en_target_lang(
            "This is a long enough English sentence for routing.",
            "zh",
            "en",
        );
        assert_eq!(target, "zh");
    }

    #[test]
    fn cn_en_normalizes_aliases_to_canonical_targets() {
        assert_eq!(
            cn_en_target_lang("English source", "zh_TW", "EN_US"),
            "zh-Hant"
        );
        assert_eq!(
            cn_en_target_lang("这是一段中文测试文本用于检测", "zh_TW", "EN_US"),
            "en"
        );
    }

    #[test]
    fn private_claim_adds_owner() {
        let msg = IncomingTextMessage {
            chat_id: 7,
            chat_kind: ChatKind::Private,
            text: "SECRET99".into(),
        };
        let action =
            evaluate_text_message(&msg, "SECRET99", TelegramMode::Owners, &[], &[], "zh", "en");
        assert_eq!(action, BotAction::Claimed { chat_id: 7 });
    }

    #[test]
    fn multi_owner_claim_and_already_owner() {
        let msg = IncomingTextMessage {
            chat_id: 8,
            chat_kind: ChatKind::Private,
            text: "/claim SECRET99".into(),
        };
        let action = evaluate_text_message(
            &msg,
            "SECRET99",
            TelegramMode::Owners,
            &[1, 2],
            &[],
            "zh",
            "en",
        );
        assert_eq!(action, BotAction::Claimed { chat_id: 8 });

        let again = evaluate_text_message(
            &msg,
            "SECRET99",
            TelegramMode::Owners,
            &[1, 2, 8],
            &[],
            "zh",
            "en",
        );
        assert_eq!(again, BotAction::AlreadyOwner);
    }

    #[test]
    fn unauthorized_private_denied() {
        let msg = IncomingTextMessage {
            chat_id: 9,
            chat_kind: ChatKind::Private,
            text: "hello there friend".into(),
        };
        let action = evaluate_text_message(
            &msg,
            "SECRET99",
            TelegramMode::Owners,
            &[1],
            &[],
            "zh",
            "en",
        );
        assert_eq!(action, BotAction::Denied);
    }

    #[test]
    fn authorized_owner_translates() {
        let msg = IncomingTextMessage {
            chat_id: 1,
            chat_kind: ChatKind::Private,
            text: "Hello from an owner chat".into(),
        };
        let action = evaluate_text_message(
            &msg,
            "SECRET99",
            TelegramMode::Owners,
            &[1],
            &[],
            "zh",
            "en",
        );
        match action {
            BotAction::Translate { text, target_lang } => {
                assert_eq!(text, "Hello from an owner chat");
                assert_eq!(target_lang, "zh");
            }
            other => panic!("expected Translate, got {other:?}"),
        }
    }

    #[test]
    fn group_mode_translates_authorized_group() {
        let msg = IncomingTextMessage {
            chat_id: -100,
            chat_kind: ChatKind::Group,
            text: "Group English message for members".into(),
        };
        let action = evaluate_text_message(
            &msg,
            "SECRET99",
            TelegramMode::Groups,
            &[],
            &[-100],
            "zh",
            "en",
        );
        match action {
            BotAction::Translate { .. } => {}
            other => panic!("expected Translate, got {other:?}"),
        }
    }

    #[test]
    fn group_mode_denies_unknown_group() {
        let msg = IncomingTextMessage {
            chat_id: -200,
            chat_kind: ChatKind::Group,
            text: "nope".into(),
        };
        let action = evaluate_text_message(
            &msg,
            "SECRET99",
            TelegramMode::Groups,
            &[],
            &[-100],
            "zh",
            "en",
        );
        assert_eq!(action, BotAction::Denied);
    }

    #[test]
    fn start_returns_help() {
        let msg = IncomingTextMessage {
            chat_id: 1,
            chat_kind: ChatKind::Private,
            text: "/start@MyBot".into(),
        };
        let action = evaluate_text_message(&msg, "x", TelegramMode::Owners, &[], &[], "zh", "en");
        assert_eq!(action, BotAction::Help);
    }

    #[test]
    fn claim_with_bot_suffix_accepts_password() {
        let msg = IncomingTextMessage {
            chat_id: 11,
            chat_kind: ChatKind::Private,
            text: "/claim@MyBot SECRET99".into(),
        };
        let action =
            evaluate_text_message(&msg, "SECRET99", TelegramMode::Owners, &[], &[], "zh", "en");
        assert_eq!(action, BotAction::Claimed { chat_id: 11 });
    }

    #[test]
    fn claim_command_case_insensitive_with_bot_suffix() {
        let msg = IncomingTextMessage {
            chat_id: 12,
            chat_kind: ChatKind::Private,
            text: "/Claim@OtherBot  SECRET99  ".into(),
        };
        let action =
            evaluate_text_message(&msg, "SECRET99", TelegramMode::Owners, &[], &[], "zh", "en");
        assert_eq!(action, BotAction::Claimed { chat_id: 12 });
    }
}

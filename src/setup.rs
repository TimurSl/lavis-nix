//! Telegram-independent domain rules for an interactive BotFather setup flow.
//!
//! This module deliberately has no transport, persistence, or logging dependency.

use std::fmt;

const BOT_SUFFIX: &str = "_bot";
const MAX_DIAGNOSTIC_BYTES: usize = 160;
const MAX_RESPONSE_BYTES: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UsernameError {
    MissingBotSuffix,
    InvalidCharactersOrLength,
}

/// A validated BotFather username in both its canonical and user-facing spelling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UsernameCandidate {
    normalized: String,
    display: String,
}

impl UsernameCandidate {
    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    /// A bounded, control-character-free value suitable for structured logs.
    pub fn safe_log_value(&self) -> String {
        safe_log_candidate(&self.display)
    }
}

/// Validates an entire username. The `_bot` suffix is case-insensitive, while
/// normalization is always lowercase so comparisons are deterministic.
pub fn validate_username(input: &str) -> Result<UsernameCandidate, UsernameError> {
    if !input.to_ascii_lowercase().ends_with(BOT_SUFFIX) {
        return Err(UsernameError::MissingBotSuffix);
    }
    if !(5..=32).contains(&input.len())
        || !input.is_ascii()
        || !input
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(UsernameError::InvalidCharactersOrLength);
    }

    Ok(UsernameCandidate {
        normalized: input.to_ascii_lowercase(),
        display: input.to_owned(),
    })
}

/// Produces a bounded, one-line representation of untrusted candidate input.
pub fn safe_log_candidate(input: &str) -> String {
    let mut value = String::new();
    for byte in input.bytes().take(32) {
        if byte.is_ascii_alphanumeric() || byte == b'_' {
            value.push(char::from(byte));
        } else {
            value.push('?');
        }
    }
    if input.len() > 32 {
        value.push('…');
    }
    value
}

pub trait EntropySource {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), EntropyError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EntropyError {
    Unavailable,
}

pub struct SystemEntropy;

impl EntropySource for SystemEntropy {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<(), EntropyError> {
        getrandom::fill(bytes).map_err(|_| EntropyError::Unavailable)
    }
}

/// Generates `lavis_<16 hex characters>_bot`, using 64 bits of entropy.
pub fn generate_candidate() -> Result<UsernameCandidate, EntropyError> {
    generate_candidate_with(&mut SystemEntropy)
}

/// Injectable form of [`generate_candidate`] for deterministic tests.
pub fn generate_candidate_with(
    source: &mut impl EntropySource,
) -> Result<UsernameCandidate, EntropyError> {
    let mut random = [0_u8; 8];
    source.fill(&mut random)?;
    let random = random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    // This construction is statically within the username constraints.
    validate_username(&format!("lavis_{random}_bot")).map_err(|_| EntropyError::Unavailable)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Confirmation {
    Confirmed,
    Cancelled,
}

/// Parses the documented confirmation controls after Unicode whitespace and
/// ASCII case normalization.
pub fn parse_confirmation(input: &str) -> Option<Confirmation> {
    match input.trim().to_ascii_lowercase().as_str() {
        "confirm" => Some(Confirmation::Confirmed),
        "cancel" | "/cancel" => Some(Confirmation::Cancelled),
        _ => None,
    }
}

/// A bot token whose debug representation cannot disclose its value.
#[derive(Clone, Eq, PartialEq)]
pub struct BotToken(String);

impl BotToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BotToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BotToken([REDACTED])")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct RedactedDiagnostic(String);

impl RedactedDiagnostic {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RedactedDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RedactedDiagnostic")
            .field(&self.0)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BotFatherResponse {
    Success { token: BotToken },
    UsernameOccupied,
    UsernameInvalid,
    LimitReached,
    FloodWait,
    Unexpected { diagnostic: RedactedDiagnostic },
}

pub fn classify_botfather_response(response: &str) -> BotFatherResponse {
    if let Some(token) = extract_token(response) {
        return BotFatherResponse::Success { token };
    }
    let lower = bounded_prefix(response, MAX_RESPONSE_BYTES).to_ascii_lowercase();
    if lower.contains("already taken") || lower.contains("not available") {
        BotFatherResponse::UsernameOccupied
    } else if lower.contains("invalid username") || lower.contains("username is invalid") {
        BotFatherResponse::UsernameInvalid
    } else if lower.contains("too many bots") || lower.contains("bot limit") {
        BotFatherResponse::LimitReached
    } else if lower.contains("too many attempts")
        || lower.contains("try again later")
        || lower.contains("flood")
    {
        BotFatherResponse::FloodWait
    } else {
        BotFatherResponse::Unexpected {
            diagnostic: RedactedDiagnostic::from_response(response),
        }
    }
}

impl RedactedDiagnostic {
    fn from_response(response: &str) -> Self {
        let mut output = String::new();
        for character in response.chars() {
            if output.len() >= MAX_DIAGNOSTIC_BYTES {
                output.push('…');
                break;
            }
            if character.is_control() {
                output.push(' ');
            } else {
                output.push(character);
            }
        }
        // Valid token-shaped substrings would have been classified as success.
        Self(output)
    }
}

fn extract_token(response: &str) -> Option<BotToken> {
    let bounded = bounded_prefix(response, MAX_RESPONSE_BYTES);
    for piece in bounded.split(|character: char| {
        !(character.is_ascii_alphanumeric()
            || character == ':'
            || character == '_'
            || character == '-')
    }) {
        if is_token_shape(piece) {
            return Some(BotToken(piece.to_owned()));
        }
    }
    None
}

fn bounded_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn is_token_shape(value: &str) -> bool {
    let Some((id, secret)) = value.split_once(':') else {
        return false;
    };
    !id.is_empty()
        && (6..=12).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && (20..=64).contains(&secret.len())
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupState {
    Idle,
    AwaitingDisplayName,
    AwaitingUsername {
        display_name: String,
    },
    AwaitingConfirmation {
        display_name: String,
        username: UsernameCandidate,
    },
    AwaitingBotFather {
        display_name: String,
        username: UsernameCandidate,
    },
    Completed {
        username: UsernameCandidate,
        token: BotToken,
    },
    Cancelled,
    Failed,
    TimedOut,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupEvent {
    CommandNewBot,
    CommandCancel,
    DisplayName(String),
    Username(String),
    Confirmation(String),
    BotFather(BotFatherResponse),
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupEffect {
    SendBotFather {
        text: String,
    },
    PromptDisplayName,
    PromptUsername,
    PromptConfirmation {
        username: UsernameCandidate,
    },
    Notify {
        message: &'static str,
    },
    Complete {
        username: UsernameCandidate,
        token: BotToken,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transition {
    pub state: SetupState,
    pub effects: Vec<SetupEffect>,
}

pub fn transition(state: SetupState, event: SetupEvent) -> Transition {
    match event {
        SetupEvent::CommandCancel => Transition {
            state: SetupState::Cancelled,
            effects: vec![SetupEffect::Notify {
                message: "Setup cancelled.",
            }],
        },
        SetupEvent::CommandNewBot => Transition {
            state: SetupState::AwaitingDisplayName,
            effects: vec![
                SetupEffect::SendBotFather {
                    text: "/newbot".to_owned(),
                },
                SetupEffect::PromptDisplayName,
            ],
        },
        SetupEvent::Timeout
            if !matches!(
                state,
                SetupState::Idle
                    | SetupState::Completed { .. }
                    | SetupState::Cancelled
                    | SetupState::Failed
                    | SetupState::TimedOut
            ) =>
        {
            Transition {
                state: SetupState::TimedOut,
                effects: vec![SetupEffect::Notify {
                    message: "Setup timed out.",
                }],
            }
        }
        SetupEvent::DisplayName(name) if matches!(state, SetupState::AwaitingDisplayName) => {
            let name = name.trim();
            if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
                Transition {
                    state: SetupState::AwaitingDisplayName,
                    effects: vec![SetupEffect::Notify {
                        message: "Display name must be 1 to 64 printable characters.",
                    }],
                }
            } else {
                Transition {
                    state: SetupState::AwaitingUsername {
                        display_name: name.to_owned(),
                    },
                    effects: vec![
                        SetupEffect::SendBotFather {
                            text: name.to_owned(),
                        },
                        SetupEffect::PromptUsername,
                    ],
                }
            }
        }
        SetupEvent::Username(input) if matches!(state, SetupState::AwaitingUsername { .. }) => {
            let SetupState::AwaitingUsername { display_name } = state else {
                unreachable!("guarded above")
            };
            match validate_username(&input) {
                Ok(username) => Transition {
                    state: SetupState::AwaitingConfirmation {
                        display_name,
                        username: username.clone(),
                    },
                    effects: vec![SetupEffect::PromptConfirmation { username }],
                },
                Err(UsernameError::MissingBotSuffix) => Transition {
                    state: SetupState::AwaitingUsername { display_name },
                    effects: vec![SetupEffect::Notify {
                        message: "Username must end with _bot.",
                    }],
                },
                Err(UsernameError::InvalidCharactersOrLength) => Transition {
                    state: SetupState::AwaitingUsername { display_name },
                    effects: vec![SetupEffect::Notify {
                        message: "Username must be 5-32 ASCII letters, digits, or underscores.",
                    }],
                },
            }
        }
        SetupEvent::Confirmation(input)
            if matches!(state, SetupState::AwaitingConfirmation { .. }) =>
        {
            let SetupState::AwaitingConfirmation {
                display_name,
                username,
            } = state
            else {
                unreachable!("guarded above")
            };
            match parse_confirmation(&input) {
                Some(Confirmation::Confirmed) => Transition {
                    state: SetupState::AwaitingBotFather {
                        display_name,
                        username: username.clone(),
                    },
                    effects: vec![SetupEffect::SendBotFather {
                        text: username.display().to_owned(),
                    }],
                },
                Some(Confirmation::Cancelled) => Transition {
                    state: SetupState::Cancelled,
                    effects: vec![SetupEffect::Notify {
                        message: "Setup cancelled.",
                    }],
                },
                None => Transition {
                    state: SetupState::AwaitingConfirmation {
                        display_name,
                        username,
                    },
                    effects: vec![SetupEffect::Notify {
                        message: "Reply exactly YES or /cancel.",
                    }],
                },
            }
        }
        SetupEvent::BotFather(response)
            if matches!(state, SetupState::AwaitingBotFather { .. }) =>
        {
            let SetupState::AwaitingBotFather {
                display_name,
                username,
            } = state
            else {
                unreachable!("guarded above")
            };
            match response {
                BotFatherResponse::Success { token } => Transition {
                    state: SetupState::Completed {
                        username: username.clone(),
                        token: token.clone(),
                    },
                    effects: vec![SetupEffect::Complete { username, token }],
                },
                BotFatherResponse::UsernameOccupied | BotFatherResponse::UsernameInvalid => {
                    Transition {
                        state: SetupState::AwaitingUsername { display_name },
                        effects: vec![
                            SetupEffect::Notify {
                                message: "BotFather rejected that username; choose another.",
                            },
                            SetupEffect::PromptUsername,
                        ],
                    }
                }
                BotFatherResponse::LimitReached => Transition {
                    state: SetupState::Failed,
                    effects: vec![SetupEffect::Notify {
                        message: "BotFather reports the bot limit was reached.",
                    }],
                },
                BotFatherResponse::FloodWait => Transition {
                    state: SetupState::Failed,
                    effects: vec![SetupEffect::Notify {
                        message: "BotFather requested a retry later.",
                    }],
                },
                BotFatherResponse::Unexpected { .. } => Transition {
                    state: SetupState::Failed,
                    effects: vec![SetupEffect::Notify {
                        message: "Unexpected BotFather response.",
                    }],
                },
            }
        }
        _ => Transition {
            state,
            effects: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_validation_normalizes_and_preserves_display_case() {
        let username = validate_username("Lavis_Test_BOT").unwrap();
        assert_eq!(username.normalized(), "lavis_test_bot");
        assert_eq!(username.display(), "Lavis_Test_BOT");
        assert_eq!(
            validate_username("lavisname"),
            Err(UsernameError::MissingBotSuffix)
        );
        assert_eq!(
            validate_username("bad-_bot"),
            Err(UsernameError::InvalidCharactersOrLength)
        );
    }

    #[test]
    fn candidate_generation_is_injectable_and_has_64_bits_of_input() {
        struct Fixed;
        impl EntropySource for Fixed {
            fn fill(&mut self, bytes: &mut [u8]) -> Result<(), EntropyError> {
                bytes.copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7]);
                Ok(())
            }
        }
        assert_eq!(
            generate_candidate_with(&mut Fixed).unwrap().normalized(),
            "lavis_0001020304050607_bot"
        );
    }

    #[test]
    fn confirmation_is_exact_and_logging_is_bounded() {
        assert_eq!(
            parse_confirmation(" Confirm "),
            Some(Confirmation::Confirmed)
        );
        assert_eq!(parse_confirmation("yes"), None);
        assert_eq!(parse_confirmation("/cancel"), Some(Confirmation::Cancelled));
        assert_eq!(safe_log_candidate("a\n$"), "a??");
        assert!(safe_log_candidate(&"x".repeat(33)).ends_with('…'));
    }

    #[test]
    fn response_classification_extracts_tokens_without_debug_leaks() {
        let token = "123456:abcdefghijklmnopqrstUVWX";
        let response = classify_botfather_response(&format!("Use {token}."));
        assert!(matches!(response, BotFatherResponse::Success { .. }));
        assert!(!format!("{response:?}").contains(token));
        assert_eq!(
            classify_botfather_response("Sorry, this username is already taken."),
            BotFatherResponse::UsernameOccupied
        );
        assert_eq!(
            classify_botfather_response("Invalid username"),
            BotFatherResponse::UsernameInvalid
        );
        assert_eq!(
            classify_botfather_response("Too many bots"),
            BotFatherResponse::LimitReached
        );
        assert_eq!(
            classify_botfather_response("Try again later"),
            BotFatherResponse::FloodWait
        );
    }

    #[test]
    fn conversation_handles_happy_path_and_rejected_username() {
        let started = transition(SetupState::Idle, SetupEvent::CommandNewBot);
        assert!(matches!(started.state, SetupState::AwaitingDisplayName));
        let named = transition(
            started.state,
            SetupEvent::DisplayName("Lavis helper".into()),
        );
        let chosen = transition(named.state, SetupEvent::Username("Lavis_Helper_BOT".into()));
        let sent = transition(chosen.state, SetupEvent::Confirmation("confirm".into()));
        let done = transition(
            sent.state,
            SetupEvent::BotFather(classify_botfather_response(
                "123456:abcdefghijklmnopqrstUVWX",
            )),
        );
        assert!(matches!(done.state, SetupState::Completed { .. }));

        let retry = transition(
            SetupState::AwaitingBotFather {
                display_name: "Lavis helper".into(),
                username: validate_username("lavis_helper_bot").unwrap(),
            },
            SetupEvent::BotFather(BotFatherResponse::UsernameOccupied),
        );
        assert!(matches!(retry.state, SetupState::AwaitingUsername { .. }));
        assert!(
            retry
                .effects
                .iter()
                .any(|effect| matches!(effect, SetupEffect::PromptUsername))
        );
    }

    #[test]
    fn cancel_and_timeout_are_terminal_effects() {
        let cancelled = transition(SetupState::AwaitingDisplayName, SetupEvent::CommandCancel);
        assert_eq!(cancelled.state, SetupState::Cancelled);
        let timed_out = transition(SetupState::AwaitingDisplayName, SetupEvent::Timeout);
        assert_eq!(timed_out.state, SetupState::TimedOut);
    }
}

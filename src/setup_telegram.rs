//! Event-driven companion setup orchestration.
//!
//! Telegram-specific delivery and the Bot API validation boundary are injected,
//! keeping the sequence deterministic and preventing token-bearing URLs from
//! entering the update loop or diagnostics.

use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

use crate::{
    bot_api::{BotApi, BotApiError},
    setup::{BotToken, UsernameCandidate, classify_botfather_response},
    setup_provision::{CompletedWithoutFolder, ProvisionResult},
    setup_store::{CompanionToken, PersistedSetupState, SetupStore},
};
use grammers_client::{Client, message::InputMessage};
use grammers_session::types::{PeerId, PeerRef};

pub const DISPLAY_NAME: &str = "Lavis — really your userbot";
pub const PROVISION_TIMEOUT: Duration = Duration::from_secs(90);
pub const BOTFATHER_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);

/// The only data a detached provisioning task may own. It deliberately does
/// not retain runtime or external-module state.
#[derive(Clone)]
pub struct ProvisionRequest {
    client: Client,
    state_path: PathBuf,
    token_path: PathBuf,
    bot_username: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionOutcome {
    Completed,
    CompletedWithoutFolder(CompletedWithoutFolder),
    Failed(crate::setup_grammers::ProvisionError),
}

impl ProvisionRequest {
    pub fn new(
        client: Client,
        state_path: PathBuf,
        token_path: PathBuf,
        bot_username: String,
    ) -> Self {
        Self {
            client,
            state_path,
            token_path,
            bot_username,
        }
    }

    pub async fn run(self) -> ProvisionOutcome {
        match tokio::time::timeout(
            PROVISION_TIMEOUT,
            crate::setup_grammers::provision(
                &self.client,
                self.state_path,
                self.token_path,
                &self.bot_username,
            ),
        )
        .await
        {
            Ok(Ok(result)) => provision_outcome(result),
            Ok(Err(error)) => {
                tracing::warn!(event = "companion_provision_failed", error_category = ?error, "Companion provisioning failed");
                ProvisionOutcome::Failed(error)
            }
            Err(_) => {
                tracing::warn!(event = "companion_provision_failed", error_category = ?crate::setup_grammers::ProvisionError::Timeout, "Companion provisioning timed out");
                ProvisionOutcome::Failed(crate::setup_grammers::ProvisionError::Timeout)
            }
        }
    }
}

fn provision_outcome(result: ProvisionResult) -> ProvisionOutcome {
    match result {
        ProvisionResult::Completed => ProvisionOutcome::Completed,
        ProvisionResult::CompletedWithoutFolder(reason) => {
            ProvisionOutcome::CompletedWithoutFolder(reason)
        }
    }
}

pub type SetupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), SetupTelegramError>> + Send + 'a>>;

/// MTProto boundary. The production adapter is intentionally responsible for
/// raw grammers calls (channel/forum/topic/admin/folder); orchestration never
/// deals in raw TL fields.
pub trait TelegramSetup: Send + Sync {
    fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a>;
}

/// Production delivery adapter. `PeerRef` is obtained from
/// `Client::resolve_username("BotFather")` and retains the access hash needed
/// by `Client::send_message`.
pub struct GrammersTelegramSetup {
    client: Client,
    botfather: PeerRef,
}

impl GrammersTelegramSetup {
    pub async fn resolve(client: &Client) -> Result<(Self, PeerId), SetupTelegramError> {
        tokio::time::timeout(BOTFATHER_OPERATION_TIMEOUT, async {
            let peer = client
                .resolve_username("BotFather")
                .await
                .map_err(|_| SetupTelegramError::Telegram)?
                .ok_or(SetupTelegramError::Telegram)?;
            let peer_id = peer.id();
            let reference = peer
                .to_ref()
                .await
                .map_err(|_| SetupTelegramError::Telegram)?
                .ok_or(SetupTelegramError::Telegram)?;
            Ok((
                Self {
                    client: client.clone(),
                    botfather: reference,
                },
                peer_id,
            ))
        })
        .await
        .map_err(|_| SetupTelegramError::Timeout)?
    }
}

impl TelegramSetup for GrammersTelegramSetup {
    fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a> {
        Box::pin(async move {
            tokio::time::timeout(BOTFATHER_OPERATION_TIMEOUT, async {
                self.client
                    .send_message(self.botfather, InputMessage::new().text(text))
                    .await
                    .map(|_| ())
                    .map_err(|_| SetupTelegramError::Telegram)
            })
            .await
            .map_err(|_| SetupTelegramError::Timeout)?
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupTelegramError {
    Telegram,
    BotApi(BotApiError),
    Storage,
    UsernameMismatch,
    Timeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BotFatherProgress {
    Pending,
    UsernameOccupied,
    UsernameInvalid,
    LimitReached,
    FloodWait,
    Unexpected,
    ProvisionReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Step {
    Cancel,
    NewBot,
    DisplayName,
    Username,
    Complete,
}

/// A single active BotFather conversation. Each BotFather reply advances exactly
/// one step, so `/newbot`, name, and username are never sent speculatively.
pub struct CompanionSetup {
    username: UsernameCandidate,
    step: Step,
    state_path: PathBuf,
    token_path: PathBuf,
}

impl CompanionSetup {
    pub fn new(username: UsernameCandidate, state_path: PathBuf, token_path: PathBuf) -> Self {
        Self {
            username,
            step: Step::Cancel,
            state_path,
            token_path,
        }
    }

    pub async fn start(&mut self, telegram: &impl TelegramSetup) -> Result<(), SetupTelegramError> {
        // Clearing a stale BotFather flow is best effort. Its reply is not a
        // prerequisite for this new conversation.
        let _ = send_with_timeout(telegram, "/cancel").await;
        self.step = Step::NewBot;
        send_with_timeout(telegram, "/newbot").await
    }

    /// Consume only a BotFather reply from the resolved BotFather peer.
    pub async fn on_botfather_reply(
        &mut self,
        text: &str,
        telegram: &impl TelegramSetup,
        bot_api: &impl BotApi,
    ) -> Result<BotFatherProgress, SetupTelegramError> {
        // These are terminal regardless of which prompt we were waiting for.
        // BotFather can send them late, including after a delayed `/cancel`.
        match classify_botfather_response(text) {
            crate::setup::BotFatherResponse::LimitReached => {
                return Ok(BotFatherProgress::LimitReached);
            }
            crate::setup::BotFatherResponse::FloodWait => {
                return Ok(BotFatherProgress::FloodWait);
            }
            _ => {}
        }
        match self.step {
            Step::Cancel => {
                self.step = Step::NewBot;
                send_with_timeout(telegram, "/newbot").await?;
            }
            Step::NewBot => {
                if !is_display_name_prompt(text) {
                    return Ok(BotFatherProgress::Pending);
                }
                self.step = Step::DisplayName;
                send_with_timeout(telegram, DISPLAY_NAME).await?;
            }
            Step::DisplayName => {
                if !is_username_prompt(text) {
                    return Ok(BotFatherProgress::Pending);
                }
                self.step = Step::Username;
                send_with_timeout(telegram, self.username.display()).await?;
            }
            Step::Username => {
                let response = classify_botfather_response(text);
                let token = match response {
                    crate::setup::BotFatherResponse::Success { token } => token,
                    crate::setup::BotFatherResponse::UsernameOccupied => {
                        return Ok(BotFatherProgress::UsernameOccupied);
                    }
                    crate::setup::BotFatherResponse::UsernameInvalid => {
                        return Ok(BotFatherProgress::UsernameInvalid);
                    }
                    crate::setup::BotFatherResponse::LimitReached => {
                        return Ok(BotFatherProgress::LimitReached);
                    }
                    crate::setup::BotFatherResponse::FloodWait => {
                        return Ok(BotFatherProgress::FloodWait);
                    }
                    crate::setup::BotFatherResponse::Unexpected { .. } => {
                        return Ok(BotFatherProgress::Unexpected);
                    }
                };
                self.validate_and_persist(token, bot_api).await?;
                self.step = Step::Complete;
                return Ok(BotFatherProgress::ProvisionReady);
            }
            Step::Complete => return Ok(BotFatherProgress::ProvisionReady),
        }
        Ok(BotFatherProgress::Pending)
    }

    pub fn provision_request(&self, client: Client) -> ProvisionRequest {
        ProvisionRequest::new(
            client,
            self.state_path.clone(),
            self.token_path.clone(),
            self.username.normalized().to_owned(),
        )
    }

    async fn validate_and_persist(
        &self,
        token: BotToken,
        bot_api: &impl BotApi,
    ) -> Result<(), SetupTelegramError> {
        let token = CompanionToken::new(token.as_str().to_owned())
            .map_err(|_| SetupTelegramError::Storage)?;
        let identity = tokio::time::timeout(Duration::from_secs(25), bot_api.get_me(&token))
            .await
            .map_err(|_| SetupTelegramError::Timeout)?
            .map_err(SetupTelegramError::BotApi)?;
        if !identity
            .username
            .eq_ignore_ascii_case(self.username.normalized())
        {
            return Err(SetupTelegramError::UsernameMismatch);
        }
        let state_path = self.state_path.clone();
        let token_path = self.token_path.clone();
        let username = identity.username;
        let bot_id = identity.id;
        tokio::task::spawn_blocking(move || {
            let mut store = SetupStore::new(state_path, token_path);
            let mut state = match store.load_state() {
                Ok(state) => state,
                Err(crate::error::SetupStoreError::NotFound) => PersistedSetupState::default(),
                Err(error) => return Err(error),
            };
            // Persist a verified identity before the credential. A crash can
            // therefore leave only a repairable, unvalidated identity; it can
            // never mark a bot validated without its token being durable.
            state.identities.bot_username = Some(username);
            state.identities.bot_user_id = Some(bot_id);
            store.save_state(&state)?;
            store.save_token(&token)?;
            state.status = "bot_validated".into();
            state.stages.bot_created = true;
            store.save_state(&state)
        })
        .await
        .map_err(|_| SetupTelegramError::Storage)?
        .map_err(|_| SetupTelegramError::Storage)?;
        Ok(())
    }
}

fn is_display_name_prompt(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    !is_cancel_acknowledgement(&text)
        && (text.contains("how are we going to call")
            || text.contains("name for your bot")
            || text.contains("new bot"))
}

fn is_username_prompt(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    !is_cancel_acknowledgement(&text) && text.contains("username")
}

fn is_cancel_acknowledgement(text: &str) -> bool {
    text.contains("cancelled")
        || text.contains("canceled")
        || text.contains("no active conversation")
}

async fn send_with_timeout(
    telegram: &impl TelegramSetup,
    text: &str,
) -> Result<(), SetupTelegramError> {
    send_with_timeout_for(telegram, text, BOTFATHER_OPERATION_TIMEOUT).await
}

async fn send_with_timeout_for(
    telegram: &impl TelegramSetup,
    text: &str,
    timeout: Duration,
) -> Result<(), SetupTelegramError> {
    tokio::time::timeout(timeout, telegram.send_botfather(text))
        .await
        .map_err(|_| SetupTelegramError::Timeout)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot_api::{BotApiFuture, BotIdentity};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    struct TelegramMock(Arc<Mutex<Vec<String>>>);
    impl TelegramSetup for TelegramMock {
        fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a> {
            self.0.lock().unwrap().push(text.into());
            Box::pin(async { Ok(()) })
        }
    }
    struct BotApiMock;
    impl BotApi for BotApiMock {
        fn get_me<'a>(&'a self, _: &'a CompanionToken) -> BotApiFuture<'a> {
            Box::pin(async {
                Ok(BotIdentity {
                    id: 1,
                    username: "lavis_test_bot".into(),
                })
            })
        }
    }

    struct CancelFailsTelegram(Arc<Mutex<Vec<String>>>);
    impl TelegramSetup for CancelFailsTelegram {
        fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a> {
            self.0.lock().unwrap().push(text.into());
            Box::pin(async move {
                if text == "/cancel" {
                    Err(SetupTelegramError::Telegram)
                } else {
                    Ok(())
                }
            })
        }
    }

    #[tokio::test]
    async fn advances_botfather_conversation_only_after_replies() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock(sent.clone());
        let path = std::env::temp_dir().join(format!("lavis-setup-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut setup = CompanionSetup::new(
            crate::setup::validate_username("lavis_test_bot").unwrap(),
            path.join("state"),
            path.join("token"),
        );
        setup.start(&telegram).await.unwrap();
        for (prompt, expected) in [
            ("How are we going to call it?", BotFatherProgress::Pending),
            (
                "Good. Now let's choose a username for your bot.",
                BotFatherProgress::Pending,
            ),
            ("ok", BotFatherProgress::Unexpected),
        ] {
            assert_eq!(
                setup
                    .on_botfather_reply(prompt, &telegram, &BotApiMock)
                    .await
                    .unwrap(),
                expected
            );
        }
        assert_eq!(
            *sent.lock().unwrap(),
            vec!["/cancel", "/newbot", DISPLAY_NAME, "lavis_test_bot"]
        );
        let _ = std::fs::remove_dir_all(path);
    }

    #[tokio::test]
    async fn starts_newbot_when_optional_cancel_fails() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = CancelFailsTelegram(sent.clone());
        let mut setup = CompanionSetup::new(
            crate::setup::validate_username("lavis_test_bot").unwrap(),
            PathBuf::new(),
            PathBuf::new(),
        );

        setup.start(&telegram).await.unwrap();

        assert_eq!(*sent.lock().unwrap(), ["/cancel", "/newbot"]);
    }

    #[tokio::test]
    async fn stuck_botfather_operation_times_out() {
        struct Stuck;
        impl TelegramSetup for Stuck {
            fn send_botfather<'a>(&'a self, _: &'a str) -> SetupFuture<'a> {
                Box::pin(std::future::pending())
            }
        }

        assert_eq!(
            send_with_timeout_for(&Stuck, "/newbot", Duration::ZERO).await,
            Err(SetupTelegramError::Timeout)
        );
    }

    #[tokio::test]
    async fn ignores_delayed_cancel_reply_until_the_expected_prompt_arrives() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock(sent.clone());
        let mut setup = CompanionSetup::new(
            crate::setup::validate_username("lavis_test_bot").unwrap(),
            PathBuf::new(),
            PathBuf::new(),
        );
        setup.start(&telegram).await.unwrap();

        assert_eq!(
            setup
                .on_botfather_reply("No active conversation to cancel.", &telegram, &BotApiMock)
                .await
                .unwrap(),
            BotFatherProgress::Pending
        );
        assert_eq!(*sent.lock().unwrap(), ["/cancel", "/newbot"]);
        setup
            .on_botfather_reply("How are we going to call it?", &telegram, &BotApiMock)
            .await
            .unwrap();
        assert_eq!(*sent.lock().unwrap(), ["/cancel", "/newbot", DISPLAY_NAME]);
    }

    #[tokio::test]
    async fn limit_and_flood_are_terminal_before_any_expected_prompt() {
        let telegram = TelegramMock(Arc::new(Mutex::new(Vec::new())));
        for step in [Step::NewBot, Step::DisplayName, Step::Username] {
            for (reply, expected) in [
                ("Too many bots", BotFatherProgress::LimitReached),
                ("Try again later", BotFatherProgress::FloodWait),
            ] {
                let mut setup = CompanionSetup::new(
                    crate::setup::validate_username("lavis_test_bot").unwrap(),
                    PathBuf::new(),
                    PathBuf::new(),
                );
                setup.step = step;
                assert_eq!(
                    setup
                        .on_botfather_reply(reply, &telegram, &BotApiMock)
                        .await
                        .unwrap(),
                    expected
                );
            }
        }
    }

    #[tokio::test]
    async fn validated_identity_is_durable_before_the_validated_stage() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let telegram = TelegramMock(sent);
        let path =
            std::env::temp_dir().join(format!("lavis-setup-identity-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let state_path = path.join("state");
        let token_path = path.join("token");
        let mut setup = CompanionSetup::new(
            crate::setup::validate_username("lavis_test_bot").unwrap(),
            state_path.clone(),
            token_path.clone(),
        );
        setup.start(&telegram).await.unwrap();
        for prompt in [
            "How are we going to call it?",
            "Now choose a username",
            "123456:abcdefghijklmnopqrstUVWX",
        ] {
            setup
                .on_botfather_reply(prompt, &telegram, &BotApiMock)
                .await
                .unwrap();
        }
        let store = SetupStore::new(state_path, token_path);
        let state = store.load_state().unwrap();
        assert_eq!(
            state.identities.bot_username.as_deref(),
            Some("lavis_test_bot")
        );
        assert_eq!(state.identities.bot_user_id, Some(1));
        assert!(state.stages.bot_created);
        assert_eq!(state.status, "bot_validated");
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn maps_every_provision_result_without_losing_the_partial_reason() {
        assert_eq!(
            provision_outcome(ProvisionResult::Completed),
            ProvisionOutcome::Completed
        );
        assert_eq!(
            provision_outcome(ProvisionResult::CompletedWithoutFolder(
                CompletedWithoutFolder::Capacity,
            )),
            ProvisionOutcome::CompletedWithoutFolder(CompletedWithoutFolder::Capacity)
        );
        assert_eq!(
            provision_outcome(ProvisionResult::CompletedWithoutFolder(
                CompletedWithoutFolder::NameOrOwnershipConflict,
            )),
            ProvisionOutcome::CompletedWithoutFolder(
                CompletedWithoutFolder::NameOrOwnershipConflict,
            )
        );
    }
}

//! Event-driven companion setup orchestration.
//!
//! Telegram-specific delivery and the Bot API validation boundary are injected,
//! keeping the sequence deterministic and preventing token-bearing URLs from
//! entering the update loop or diagnostics.

use std::{future::Future, path::PathBuf, pin::Pin, time::Duration};

use crate::{
    bot_api::{BotApi, BotApiError},
    setup::{BotToken, UsernameCandidate, classify_botfather_response},
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
    Failed,
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
            Ok(Ok(())) => ProvisionOutcome::Completed,
            Ok(Err(error)) => {
                tracing::warn!(event = "companion_provision_failed", error_category = ?error, "Companion provisioning failed");
                ProvisionOutcome::Failed
            }
            Err(_) => ProvisionOutcome::Failed,
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
    Occupied,
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
        match self.step {
            Step::Cancel => {
                self.step = Step::NewBot;
                send_with_timeout(telegram, "/newbot").await?;
            }
            Step::NewBot => {
                self.step = Step::DisplayName;
                send_with_timeout(telegram, DISPLAY_NAME).await?;
            }
            Step::DisplayName => {
                self.step = Step::Username;
                send_with_timeout(telegram, self.username.display()).await?;
            }
            Step::Username => {
                let response = classify_botfather_response(text);
                if matches!(
                    response,
                    crate::setup::BotFatherResponse::UsernameOccupied
                        | crate::setup::BotFatherResponse::UsernameInvalid
                ) {
                    return Ok(BotFatherProgress::Occupied);
                }
                let crate::setup::BotFatherResponse::Success { token } = response else {
                    return Ok(BotFatherProgress::Pending);
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
        tokio::task::spawn_blocking(move || {
            let mut store = SetupStore::new(state_path, token_path);
            let mut state = match store.load_state() {
                Ok(state) => state,
                Err(crate::error::SetupStoreError::NotFound) => PersistedSetupState::default(),
                Err(error) => return Err(error),
            };
            state.status = "bot_validated".into();
            state.stages.bot_created = true;
            state.identities.bot_username = Some(username);
            store
                .save_token(&token)
                .and_then(|_| store.save_state(&state))
        })
        .await
        .map_err(|_| SetupTelegramError::Storage)?
        .map_err(|_| SetupTelegramError::Storage)?;
        Ok(())
    }
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
        for _ in 0..3 {
            assert_eq!(
                setup
                    .on_botfather_reply("ok", &telegram, &BotApiMock)
                    .await
                    .unwrap(),
                BotFatherProgress::Pending
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
}

//! Event-driven companion setup orchestration.
//!
//! Telegram-specific delivery and the Bot API validation boundary are injected,
//! keeping the sequence deterministic and preventing token-bearing URLs from
//! entering the update loop or diagnostics.

use std::{future::Future, path::PathBuf, pin::Pin};

use crate::{
    bot_api::{BotApi, BotApiError},
    setup::{BotToken, UsernameCandidate, classify_botfather_response},
    setup_store::{CompanionToken, PersistedSetupState, SetupStore},
};

pub const DISPLAY_NAME: &str = "Lavis — really your userbot";

pub type SetupFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), SetupTelegramError>> + Send + 'a>>;

/// MTProto boundary. The production adapter is intentionally responsible for
/// raw grammers calls (channel/forum/topic/admin/folder); orchestration never
/// deals in raw TL fields.
pub trait TelegramSetup: Send + Sync {
    fn send_botfather<'a>(&'a self, text: &'a str) -> SetupFuture<'a>;
    fn provision_companion<'a>(&'a self, bot_username: &'a str) -> SetupFuture<'a>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupTelegramError {
    Telegram,
    BotApi(BotApiError),
    Storage,
    UsernameMismatch,
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
        telegram.send_botfather("/cancel").await
    }

    /// Consume only a BotFather reply from the resolved BotFather peer.
    pub async fn on_botfather_reply(
        &mut self,
        text: &str,
        telegram: &impl TelegramSetup,
        bot_api: &impl BotApi,
    ) -> Result<bool, SetupTelegramError> {
        match self.step {
            Step::Cancel => {
                self.step = Step::NewBot;
                telegram.send_botfather("/newbot").await?;
            }
            Step::NewBot => {
                self.step = Step::DisplayName;
                telegram.send_botfather(DISPLAY_NAME).await?;
            }
            Step::DisplayName => {
                self.step = Step::Username;
                telegram.send_botfather(self.username.display()).await?;
            }
            Step::Username => {
                let crate::setup::BotFatherResponse::Success { token } =
                    classify_botfather_response(text)
                else {
                    return Ok(false);
                };
                self.validate_persist_and_provision(token, telegram, bot_api)
                    .await?;
                self.step = Step::Complete;
                return Ok(true);
            }
            Step::Complete => return Ok(true),
        }
        Ok(false)
    }

    async fn validate_persist_and_provision(
        &self,
        token: BotToken,
        telegram: &impl TelegramSetup,
        bot_api: &impl BotApi,
    ) -> Result<(), SetupTelegramError> {
        let token = CompanionToken::new(token.as_str().to_owned())
            .map_err(|_| SetupTelegramError::Storage)?;
        let identity = bot_api
            .get_me(&token)
            .await
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
            let mut state = PersistedSetupState::default();
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
        telegram
            .provision_companion(self.username.normalized())
            .await
    }
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
        fn provision_companion<'a>(&'a self, _: &'a str) -> SetupFuture<'a> {
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
            assert!(
                !setup
                    .on_botfather_reply("ok", &telegram, &BotApiMock)
                    .await
                    .unwrap()
            );
        }
        assert_eq!(
            *sent.lock().unwrap(),
            vec!["/cancel", "/newbot", DISPLAY_NAME, "lavis_test_bot"]
        );
        let _ = std::fs::remove_dir_all(path);
    }
}

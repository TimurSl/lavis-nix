//! Minimal, token-redacting Telegram Bot API validation client.

use std::{future::Future, pin::Pin, time::Duration};

use serde::Deserialize;

use crate::setup_store::CompanionToken;

pub type BotApiFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BotIdentity, BotApiError>> + Send + 'a>>;

/// The deliberately narrow HTTP boundary used by setup.  Tests can inject this
/// without opening a socket or ever constructing a token URL.
pub trait BotApi: Send + Sync {
    fn get_me<'a>(&'a self, token: &'a CompanionToken) -> BotApiFuture<'a>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BotIdentity {
    pub id: i64,
    pub username: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BotApiError {
    Transport,
    Rejected,
    Malformed,
}

/// Uses Rustls only (via reqwest's `rustls-tls` feature).  Do not include the
/// request URL in errors: its path contains the token.
pub struct HttpBotApi {
    client: reqwest::Client,
}

impl HttpBotApi {
    pub fn new() -> Result<Self, BotApiError> {
        reqwest::Client::builder()
            .https_only(true)
            .timeout(Duration::from_secs(20))
            .build()
            .map(|client| Self { client })
            .map_err(|_| BotApiError::Transport)
    }
}

#[derive(Deserialize)]
struct GetMeResponse {
    ok: bool,
    result: Option<GetMeResult>,
}

#[derive(Deserialize)]
struct GetMeResult {
    id: i64,
    username: Option<String>,
}

impl BotApi for HttpBotApi {
    fn get_me<'a>(&'a self, token: &'a CompanionToken) -> BotApiFuture<'a> {
        Box::pin(async move {
            let url = format!("https://api.telegram.org/bot{}/getMe", token.as_str());
            let response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|_| BotApiError::Transport)?;
            if !response.status().is_success() {
                return Err(BotApiError::Rejected);
            }
            let body: GetMeResponse = response.json().await.map_err(|_| BotApiError::Malformed)?;
            let Some(result) = body.ok.then_some(body.result).flatten() else {
                return Err(BotApiError::Rejected);
            };
            let Some(username) = result.username.filter(|name| !name.is_empty()) else {
                return Err(BotApiError::Malformed);
            };
            Ok(BotIdentity {
                id: result.id,
                username,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mock;
    impl BotApi for Mock {
        fn get_me<'a>(&'a self, _: &'a CompanionToken) -> BotApiFuture<'a> {
            Box::pin(async {
                Ok(BotIdentity {
                    id: 7,
                    username: "lavis_test_bot".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn typed_mock_validates_without_network() {
        let token = CompanionToken::new("123456:abcdefghijklmnopqrstUVWX".into()).unwrap();
        assert_eq!(
            Mock.get_me(&token).await.unwrap().username,
            "lavis_test_bot"
        );
        assert!(!format!("{token:?}").contains(token.as_str()));
    }
}

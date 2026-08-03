use super::{manifest::ExternalCapability, protocol::TelegramCallError};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

/// A nested Telegram call is always shorter than its parent module request.
pub const TELEGRAM_CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// Scope is deliberately supplied before peer/media methods exist. Future
/// handlers must validate opaque references against this module/request scope;
/// `account.updateStatus` has no opaque references to validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayContext {
    pub module_id: String,
    pub request_id: String,
}

pub trait TelegramGateway: Send + Sync {
    fn invoke<'a>(
        &'a self,
        context: GatewayContext,
        method: &'a str,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, TelegramCallError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct GrammersGateway {
    client: grammers_client::Client,
}

impl GrammersGateway {
    pub fn new(client: grammers_client::Client) -> Arc<Self> {
        Arc::new(Self { client })
    }
}

impl TelegramGateway for GrammersGateway {
    fn invoke<'a>(
        &'a self,
        _context: GatewayContext,
        method: &'a str,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, TelegramCallError>> + Send + 'a>>
    {
        Box::pin(async move {
            let offline =
                validate_request(ExternalCapability::TelegramAccountStatus, method, &params)?;
            let request = grammers_client::tl::functions::account::UpdateStatus { offline };
            match tokio::time::timeout(TELEGRAM_CALL_TIMEOUT, self.client.invoke(&request)).await {
                Ok(Ok(value)) => Ok(serde_json::Value::Bool(value)),
                Ok(Err(grammers_client::InvocationError::Rpc(error))) => {
                    Err(rpc_error(error.code, &error.name, error.value))
                }
                Ok(Err(_)) => Err(TelegramCallError {
                    kind: "transport",
                    code: None,
                    name: None,
                    message: "Telegram request failed".to_owned(),
                    retry_after_seconds: None,
                }),
                Err(_) => Err(TelegramCallError {
                    kind: "timeout",
                    code: None,
                    name: None,
                    message: "Telegram request timed out".to_owned(),
                    retry_after_seconds: None,
                }),
            }
        })
    }
}

fn rpc_error(code: i32, name: &str, value: Option<u32>) -> TelegramCallError {
    TelegramCallError {
        kind: "rpc",
        code: Some(code),
        name: Some(name.to_owned()),
        message: name.to_owned(),
        retry_after_seconds: (name == "FLOOD_WAIT").then_some(value).flatten(),
    }
}

pub fn validate_request(
    capability: ExternalCapability,
    method: &str,
    params: &serde_json::Value,
) -> Result<bool, TelegramCallError> {
    if capability != ExternalCapability::TelegramAccountStatus {
        return Err(validation("capability is required"));
    }
    if method != "account.updateStatus" {
        return Err(validation("method is not allowed"));
    }
    let object = params
        .as_object()
        .ok_or_else(|| validation("params must be an object"))?;
    if object.len() != 1 {
        return Err(validation("unknown params"));
    }
    object
        .get("offline")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| validation("offline must be boolean"))
}

fn validation(message: &str) -> TelegramCallError {
    TelegramCallError {
        kind: "validation",
        code: None,
        name: None,
        message: message.to_owned(),
        retry_after_seconds: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_status_policy_is_capability_gated_and_strict() {
        assert!(
            validate_request(
                ExternalCapability::TelegramAccountStatus,
                "account.updateStatus",
                &serde_json::json!({"offline": true})
            )
            .unwrap()
        );
        assert_eq!(
            validate_request(
                ExternalCapability::MessageRead,
                "account.updateStatus",
                &serde_json::json!({"offline": true})
            )
            .unwrap_err()
            .kind,
            "validation"
        );
        assert!(
            validate_request(
                ExternalCapability::TelegramAccountStatus,
                "messages.sendMessage",
                &serde_json::json!({})
            )
            .is_err()
        );
        assert!(
            validate_request(
                ExternalCapability::TelegramAccountStatus,
                "account.updateStatus",
                &serde_json::json!({"offline": true, "extra": false})
            )
            .is_err()
        );
    }

    #[test]
    fn rpc_mapping_preserves_code_name_and_flood_wait_seconds() {
        let error = rpc_error(420, "FLOOD_WAIT", Some(17));
        assert_eq!(error.kind, "rpc");
        assert_eq!(error.code, Some(420));
        assert_eq!(error.name.as_deref(), Some("FLOOD_WAIT"));
        assert_eq!(error.retry_after_seconds, Some(17));
    }
}

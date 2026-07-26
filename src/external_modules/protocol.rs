use crate::error::ExternalError;

pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MAX_RESULT_BYTES: usize = 32 * 1024;
pub const MAX_ERROR_MESSAGE_CHARS: usize = 256;
pub const MAX_LOG_MESSAGE_CHARS: usize = 1024;
pub const MAX_EVENT_ACTIONS: usize = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomEmojiEntity {
    pub offset_utf16: usize,
    pub length_utf16: usize,
    pub document_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageCreatedEvent {
    pub event_id: String,
    pub message_ref: String,
    pub text: String,
    pub outgoing: bool,
    pub entities: Vec<CustomEmojiEntity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionSpec {
    Emoji(String),
    CustomEmoji { document_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventAction {
    pub message_ref: String,
    pub reaction: ReactionSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CoreMessage {
    Initialize {
        request_id: String,
        module_id: String,
    },
    Execute {
        request_id: String,
        command: String,
        arguments: String,
        argument_entities: Vec<CustomEmojiEntity>,
    },
    Health {
        request_id: String,
    },
    Shutdown {
        request_id: String,
    },
    Event {
        request_id: String,
        payload: MessageCreatedEvent,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModuleMessage {
    Initialized {
        request_id: String,
        module_id: String,
    },
    Result {
        request_id: String,
        text: String,
    },
    Error {
        request_id: String,
        code: String,
        message: String,
    },
    Health {
        request_id: String,
    },
    Log {
        request_id: String,
        level: String,
        message: String,
    },
    EventResult {
        request_id: String,
        actions: Vec<EventAction>,
    },
}

impl CoreMessage {
    pub fn serialize(&self) -> Result<String, ExternalError> {
        self.serialize_for(PROTOCOL_VERSION)
    }

    pub fn serialize_for(&self, protocol_version: u32) -> Result<String, ExternalError> {
        match self {
            Self::Initialize {
                request_id,
                module_id,
            } => serde_json::to_string(&serde_json::json!({
                "protocol_version": protocol_version,
                "type": "initialize",
                "request_id": request_id,
                "module_id": module_id,
            })),
            Self::Execute {
                request_id,
                command,
                arguments,
                argument_entities,
            } => {
                let mut message = serde_json::json!({
                    "protocol_version": protocol_version,
                    "type": "execute",
                    "request_id": request_id,
                    "command": command,
                    "arguments": arguments,
                });
                if protocol_version == 3 {
                    message["context"] = serde_json::json!({
                        "argument_entities": argument_entities.iter().map(|entity| serde_json::json!({
                            "type": "custom_emoji",
                            "offset_utf16": entity.offset_utf16,
                            "length_utf16": entity.length_utf16,
                            "document_id": entity.document_id,
                        })).collect::<Vec<_>>(),
                    });
                }
                serde_json::to_string(&message)
            }
            Self::Health { request_id } => serde_json::to_string(&serde_json::json!({
                "protocol_version": protocol_version,
                "type": "health",
                "request_id": request_id,
            })),
            Self::Shutdown { request_id } => serde_json::to_string(&serde_json::json!({
                "protocol_version": protocol_version,
                "type": "shutdown",
                "request_id": request_id,
            })),
            Self::Event {
                request_id,
                payload,
            } => serde_json::to_string(&serde_json::json!({
                "protocol_version": protocol_version,
                "type": "event",
                "request_id": request_id,
                "event": "message.created",
                "payload": {
                    "event_id": payload.event_id,
                    "message_ref": payload.message_ref,
                    "text": payload.text,
                    "outgoing": payload.outgoing,
                    "entities": payload.entities.iter().map(|entity| serde_json::json!({
                        "type": "custom_emoji",
                        "offset_utf16": entity.offset_utf16,
                        "length_utf16": entity.length_utf16,
                        "document_id": entity.document_id,
                    })).collect::<Vec<_>>(),
                },
            })),
        }
        .map_err(|_| ExternalError::ProtocolEncode)
    }

    pub fn request_id(&self) -> &str {
        match self {
            Self::Initialize { request_id, .. }
            | Self::Execute { request_id, .. }
            | Self::Health { request_id }
            | Self::Shutdown { request_id }
            | Self::Event { request_id, .. } => request_id,
        }
    }
}

fn validate_request_id(value: &serde_json::Value) -> Result<String, ExternalError> {
    let id = get_string(value, "request_id")?;
    if id.is_empty() || id.len() > 64 {
        return Err(ExternalError::ProtocolDecode);
    }
    if !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ExternalError::ProtocolDecode);
    }
    Ok(id)
}

pub fn parse_module_line(line: &str) -> Result<Option<ModuleMessage>, ExternalError> {
    parse_module_line_for(line, PROTOCOL_VERSION)
}

pub fn parse_module_line_for(
    line: &str,
    expected_protocol_version: u32,
) -> Result<Option<ModuleMessage>, ExternalError> {
    if line.len() > MAX_LINE_BYTES {
        return Err(ExternalError::LineTooLarge);
    }
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|_| ExternalError::ProtocolDecode)?;

    let proto = value
        .get("protocol_version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if proto != expected_protocol_version as u64 {
        return Err(ExternalError::ProtocolVersionMismatch);
    }

    let msg_type = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or(ExternalError::ProtocolDecode)?;

    match msg_type {
        "initialized" => {
            let request_id = validate_request_id(&value)?;
            let module_id = get_string(&value, "module_id")?;
            Ok(Some(ModuleMessage::Initialized {
                request_id,
                module_id,
            }))
        }
        "result" => {
            let request_id = validate_request_id(&value)?;
            let text = get_string(&value, "text")?;
            if text.len() > MAX_RESULT_BYTES {
                return Err(ExternalError::ResultTooLarge);
            }
            Ok(Some(ModuleMessage::Result { request_id, text }))
        }
        "error" => {
            let request_id = validate_request_id(&value)?;
            let code = get_string(&value, "code")?;
            let message = get_string(&value, "message")?;
            if code.chars().count() > MAX_ERROR_MESSAGE_CHARS
                || message.chars().count() > MAX_ERROR_MESSAGE_CHARS
            {
                return Err(ExternalError::ResultTooLarge);
            }
            Ok(Some(ModuleMessage::Error {
                request_id,
                code,
                message,
            }))
        }
        "health" => {
            let request_id = validate_request_id(&value)?;
            Ok(Some(ModuleMessage::Health { request_id }))
        }
        "log" => {
            let request_id = validate_request_id(&value)?;
            let level = get_string(&value, "level")?;
            let message = get_string(&value, "message")?;
            if level.chars().count() > MAX_LOG_MESSAGE_CHARS
                || message.chars().count() > MAX_LOG_MESSAGE_CHARS
            {
                return Err(ExternalError::ResultTooLarge);
            }
            Ok(Some(ModuleMessage::Log {
                request_id,
                level,
                message,
            }))
        }
        "event_result" => {
            let request_id = validate_request_id(&value)?;
            let actions = value
                .get("actions")
                .and_then(|value| value.as_array())
                .ok_or(ExternalError::ProtocolDecode)?;
            if actions.len() > MAX_EVENT_ACTIONS {
                return Err(ExternalError::ProtocolDecode);
            }
            let actions = actions
                .iter()
                .map(parse_event_action)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Some(ModuleMessage::EventResult {
                request_id,
                actions,
            }))
        }
        _ => Err(ExternalError::ProtocolDecode),
    }
}

fn parse_event_action(value: &serde_json::Value) -> Result<EventAction, ExternalError> {
    if value.get("type").and_then(|value| value.as_str()) != Some("message.react") {
        return Err(ExternalError::ProtocolDecode);
    }
    let message_ref = get_string(value, "message_ref")?;
    let reaction = value.get("reaction").ok_or(ExternalError::ProtocolDecode)?;
    let reaction = match reaction.get("type").and_then(|value| value.as_str()) {
        Some("emoji") => ReactionSpec::Emoji(get_string(reaction, "emoji")?),
        Some("custom_emoji") => ReactionSpec::CustomEmoji {
            document_id: get_string(reaction, "document_id")?,
        },
        _ => return Err(ExternalError::ProtocolDecode),
    };
    Ok(EventAction {
        message_ref,
        reaction,
    })
}

fn get_string(value: &serde_json::Value, key: &str) -> Result<String, ExternalError> {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
        .ok_or(ExternalError::ProtocolDecode)
}

pub fn request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_ID.fetch_add(1, Ordering::Relaxed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_initialize() {
        let msg = CoreMessage::Initialize {
            request_id: "1".to_owned(),
            module_id: "echo".to_owned(),
        };
        let json = msg.serialize().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["protocol_version"], 2);
        assert_eq!(parsed["type"], "initialize");
        assert_eq!(parsed["request_id"], "1");
        assert_eq!(parsed["module_id"], "echo");
    }

    #[test]
    fn serialize_execute() {
        let msg = CoreMessage::Execute {
            request_id: "2".to_owned(),
            command: "repeat".to_owned(),
            arguments: "Привет".to_owned(),
            argument_entities: Vec::new(),
        };
        let json = msg.serialize().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["protocol_version"], 2);
        assert_eq!(parsed["type"], "execute");
        assert_eq!(parsed["request_id"], "2");
        assert_eq!(parsed["command"], "repeat");
        assert_eq!(parsed["arguments"], "Привет");
        assert!(parsed.get("context").is_none());
    }

    #[test]
    fn v3_execute_projects_custom_emoji_context() {
        let msg = CoreMessage::Execute {
            request_id: "2".to_owned(),
            command: "manage".to_owned(),
            arguments: "добавить 🦀".to_owned(),
            argument_entities: vec![CustomEmojiEntity {
                offset_utf16: 9,
                length_utf16: 2,
                document_id: "5456140674028019486".to_owned(),
            }],
        };
        let parsed: serde_json::Value =
            serde_json::from_str(&msg.serialize_for(3).unwrap()).unwrap();
        assert_eq!(parsed["context"]["argument_entities"][0]["offset_utf16"], 9);
        assert_eq!(parsed["context"]["argument_entities"][0]["length_utf16"], 2);
        assert_eq!(
            parsed["context"]["argument_entities"][0]["document_id"],
            "5456140674028019486"
        );
    }

    #[test]
    fn serialize_health() {
        let msg = CoreMessage::Health {
            request_id: "3".to_owned(),
        };
        let json = msg.serialize().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "health");
    }

    #[test]
    fn serialize_shutdown() {
        let msg = CoreMessage::Shutdown {
            request_id: "4".to_owned(),
        };
        let json = msg.serialize().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "shutdown");
    }

    #[test]
    fn v3_event_result_preserves_custom_emoji_document_id_as_string() {
        let event = CoreMessage::Event {
            request_id: "9".to_owned(),
            payload: MessageCreatedEvent {
                event_id: "evt".to_owned(),
                message_ref: "opaque".to_owned(),
                text: "Привет 🦀".to_owned(),
                outgoing: false,
                entities: vec![CustomEmojiEntity {
                    offset_utf16: 7,
                    length_utf16: 2,
                    document_id: "5456140674028019486".to_owned(),
                }],
            },
        };
        let serialized = event.serialize_for(3).unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["protocol_version"], 3);
        assert_eq!(
            value["payload"]["entities"][0]["document_id"],
            "5456140674028019486"
        );
        let reply = r#"{"protocol_version":3,"type":"event_result","request_id":"9","actions":[{"type":"message.react","message_ref":"opaque","reaction":{"type":"custom_emoji","document_id":"5456140674028019486"}}]}"#;
        assert!(matches!(
            parse_module_line_for(reply, 3),
            Ok(Some(ModuleMessage::EventResult { .. }))
        ));
    }

    #[test]
    fn parse_initialized() {
        let line =
            r#"{"protocol_version":2,"type":"initialized","request_id":"1","module_id":"echo"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Initialized {
                request_id: "1".to_owned(),
                module_id: "echo".to_owned()
            }
        );
    }

    #[test]
    fn parse_result() {
        let line = r#"{"protocol_version":2,"type":"result","request_id":"2","text":"Привет"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Result {
                request_id: "2".to_owned(),
                text: "Привет".to_owned()
            }
        );
    }

    #[test]
    fn parse_error() {
        let line = r#"{"protocol_version":2,"type":"error","request_id":"2","code":"BAD_INPUT","message":"Invalid input"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Error {
                request_id: "2".to_owned(),
                code: "BAD_INPUT".to_owned(),
                message: "Invalid input".to_owned()
            }
        );
    }

    #[test]
    fn parse_health() {
        let line = r#"{"protocol_version":2,"type":"health","request_id":"3"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Health {
                request_id: "3".to_owned()
            }
        );
    }

    #[test]
    fn parse_log() {
        let line = r#"{"protocol_version":2,"type":"log","request_id":"1","level":"info","message":"hello"}"#;
        let msg = parse_module_line(line).unwrap().unwrap();
        assert_eq!(
            msg,
            ModuleMessage::Log {
                request_id: "1".to_owned(),
                level: "info".to_owned(),
                message: "hello".to_owned()
            }
        );
    }

    #[test]
    fn reject_wrong_protocol_version() {
        let line =
            r#"{"protocol_version":1,"type":"initialized","request_id":"1","module_id":"echo"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolVersionMismatch)
        ));
    }

    #[test]
    fn reject_unknown_type() {
        let line = r#"{"protocol_version":2,"type":"unknown","request_id":"1"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_malformed_json() {
        assert!(matches!(
            parse_module_line("not json"),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_oversized_line() {
        let long = "x".repeat(MAX_LINE_BYTES + 1);
        assert!(matches!(
            parse_module_line(&long),
            Err(ExternalError::LineTooLarge)
        ));
    }

    #[test]
    fn reject_oversized_result() {
        let long = "x".repeat(MAX_RESULT_BYTES + 1);
        let line = serde_json::to_string(&serde_json::json!({
            "protocol_version": 2,
            "type": "result",
            "request_id": "1",
            "text": long,
        }))
        .unwrap();
        assert!(matches!(
            parse_module_line(&line),
            Err(ExternalError::ResultTooLarge)
        ));
    }

    #[test]
    fn request_id_is_unique() {
        let id1 = request_id();
        let id2 = request_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn reject_missing_field() {
        let line = r#"{"protocol_version":2,"type":"initialized","request_id":"1"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_missing_request_id() {
        let line = r#"{"protocol_version":2,"type":"result","text":"hello"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_non_numeric_request_id() {
        let line = r#"{"protocol_version":2,"type":"result","request_id":"abc","text":"hello"}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }

    #[test]
    fn reject_empty_request_id() {
        let line = r#"{"protocol_version":2,"type":"health","request_id":""}"#;
        assert!(matches!(
            parse_module_line(line),
            Err(ExternalError::ProtocolDecode)
        ));
    }
}

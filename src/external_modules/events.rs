use super::{
    manifest::{
        ExternalAction, ExternalCapability, ExternalModuleDescriptor, ExternalSubscription,
    },
    protocol::{EventAction, ReactionSpec},
};

pub const MAX_EMOJI_CHARS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventScope {
    pub module_id: String,
    pub request_id: String,
    pub message_ref: String,
}

impl EventScope {
    pub fn accepts(&self, module_id: &str, request_id: &str, action: &EventAction) -> bool {
        self.module_id == module_id
            && self.request_id == request_id
            && self.message_ref == action.message_ref
    }
}

pub fn module_can_receive_created_event(descriptor: &ExternalModuleDescriptor) -> bool {
    descriptor.protocol_version == 3
        && descriptor
            .subscriptions
            .contains(&ExternalSubscription::MessageCreated)
        && descriptor
            .capabilities
            .contains(&ExternalCapability::MessageRead)
}

pub fn validate_reaction_action(
    descriptor: &ExternalModuleDescriptor,
    scope: &EventScope,
    request_id: &str,
    action: &EventAction,
) -> bool {
    descriptor.actions.contains(&ExternalAction::MessageReact)
        && descriptor
            .capabilities
            .contains(&ExternalCapability::MessageReact)
        && scope.accepts(&descriptor.id, request_id, action)
        && valid_reaction(&action.reaction)
}

fn valid_reaction(reaction: &ReactionSpec) -> bool {
    match reaction {
        ReactionSpec::Emoji(emoji) => !emoji.is_empty() && emoji.chars().count() <= MAX_EMOJI_CHARS && !emoji.chars().any(|character| character.is_control() || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')),
        ReactionSpec::CustomEmoji { document_id } => !document_id.is_empty() && document_id.bytes().all(|byte| byte.is_ascii_digit()) && document_id.parse::<i64>().is_ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_modules::manifest::ExternalCommandDescriptor;
    use std::path::PathBuf;

    fn descriptor() -> ExternalModuleDescriptor {
        ExternalModuleDescriptor {
            protocol_version: 3,
            id: "autoreact".to_owned(),
            display_name: "AutoReact".to_owned(),
            version: "1".to_owned(),
            author: "test".to_owned(),
            entrypoint: PathBuf::new(),
            module_dir: PathBuf::new(),
            capabilities: vec![
                ExternalCapability::MessageRead,
                ExternalCapability::MessageReact,
            ],
            default_command: Some("manage".to_owned()),
            subscriptions: vec![ExternalSubscription::MessageCreated],
            actions: vec![ExternalAction::MessageReact],
            commands: vec![ExternalCommandDescriptor {
                name: "manage".to_owned(),
                summary_ru: "x".to_owned(),
                description_ru: "x".to_owned(),
                usage: "x".to_owned(),
                examples: vec![],
            }],
        }
    }

    #[test]
    fn validates_scoped_reactions() {
        let descriptor = descriptor();
        let scope = EventScope {
            module_id: "autoreact".to_owned(),
            request_id: "7".to_owned(),
            message_ref: "opaque".to_owned(),
        };
        let action = EventAction {
            message_ref: "opaque".to_owned(),
            reaction: ReactionSpec::CustomEmoji {
                document_id: "5456140674028019486".to_owned(),
            },
        };
        assert!(module_can_receive_created_event(&descriptor));
        assert!(validate_reaction_action(&descriptor, &scope, "7", &action));
        assert!(!validate_reaction_action(&descriptor, &scope, "8", &action));
    }
}

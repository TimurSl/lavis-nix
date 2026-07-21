pub use crate::response::Response;
use crate::{
    aliases::AliasStore,
    commands::{COMMANDS, CommandKind, HelpRequest, definition},
};

pub struct RenderedHelp {
    pub response: Response,
    pub entity_fallback: bool,
}

pub fn render(request: &HelpRequest, prefix: &str, aliases: &AliasStore) -> RenderedHelp {
    match request {
        HelpRequest::Overview => render_quote(
            "🛠 Lavis commands".to_owned(),
            COMMANDS
                .iter()
                .map(|definition| {
                    format!(
                        "{} {prefix}{} — {}",
                        definition.icon, definition.usage, definition.summary
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        HelpRequest::Topic(kind) => render_topic(*kind, prefix),
        HelpRequest::Unknown(topic) => render_alias_or_unknown(topic, prefix, aliases),
        HelpRequest::Invalid => RenderedHelp {
            response: Response::plain(format!("⚠️ Usage: {prefix}help [command]")),
            entity_fallback: false,
        },
    }
}

fn render_alias_or_unknown(topic: &str, prefix: &str, aliases: &AliasStore) -> RenderedHelp {
    let Some(alias) = aliases.lookup(topic) else {
        return RenderedHelp {
            response: Response::plain(format!(
                "❓ Unknown command: {topic}\nUse {prefix}help to list available commands."
            )),
            entity_fallback: false,
        };
    };
    let preset = if alias.args.is_empty() {
        String::new()
    } else {
        format!(" {}", shell_words::join(&alias.args))
    };
    render_quote(
        format!("🔗 {prefix}{topic}"),
        format!(
            "Alias for {prefix}{}{preset}\n\nUsage: {prefix}{topic} [arguments]",
            alias.target
        ),
    )
}

fn render_topic(kind: CommandKind, prefix: &str) -> RenderedHelp {
    let definition = definition(kind);
    render_quote(
        format!("{} {prefix}{}", definition.icon, definition.usage),
        format!(
            "{}\n\nUsage: {prefix}{}",
            definition.description, definition.usage
        ),
    )
}

fn render_quote(heading: String, body: String) -> RenderedHelp {
    let rendered = Response::collapsed(heading, body);
    RenderedHelp {
        response: rendered.response,
        entity_fallback: rendered.entity_fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::{Response, render};
    use crate::{
        aliases::{Alias, AliasStore},
        commands::{CommandKind, HelpRequest, definition},
    };
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    async fn aliases() -> AliasStore {
        AliasStore::load(PathBuf::from("/nonexistent/lavis-help-aliases.json"))
            .await
            .unwrap()
    }

    async fn aliases_with_mini() -> (AliasStore, PathBuf) {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("lavis-help-alias-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let mut aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
        aliases
            .add(
                "mini",
                Alias {
                    target: "fastfetch".to_owned(),
                    args: Vec::new(),
                },
            )
            .await
            .unwrap();
        (aliases, directory)
    }

    #[tokio::test]
    async fn overview_uses_registry_order_and_configured_prefix_once_per_command() {
        let response = render(&HelpRequest::Overview, "!", &aliases().await).response;

        assert_eq!(response.text.matches('!').count(), 5);
        assert!(response.text.contains("Measure Telegram latency"));
        assert!(response.text.contains("Show runtime statistics"));
        assert!(response.text.contains("Show command help"));
        assert!(response.text.find("🏓 !ping").unwrap() < response.text.find("📊 !stats").unwrap());
        assert!(
            response.text.find("📊 !stats").unwrap()
                < response.text.find("🛠 !help [command]").unwrap()
        );
        assert_eq!(response.entities.len(), 1);
    }

    #[tokio::test]
    async fn command_details_keep_titles_outside_a_single_entity() {
        for (kind, title) in [
            (CommandKind::Ping, "🏓 ,ping\n\n"),
            (CommandKind::Stats, "📊 ,stats\n\n"),
            (CommandKind::Help, "🛠 ,help [command]\n\n"),
        ] {
            let response = render(&HelpRequest::Topic(kind), ",", &aliases().await).response;
            let definition = definition(kind);

            assert!(response.text.starts_with(title));
            assert!(response.text.ends_with(&format!(
                "{}\n\nUsage: ,{}",
                definition.description, definition.usage
            )));
            assert_eq!(response.entities.len(), 1);
        }
    }

    #[tokio::test]
    async fn renders_unknown_and_invalid_help_plainly() {
        let unknown = render(
            &HelpRequest::Unknown("foo".to_owned()),
            ",",
            &aliases().await,
        )
        .response;
        let invalid = render(&HelpRequest::Invalid, "!", &aliases().await).response;

        assert_eq!(
            unknown,
            Response::plain("❓ Unknown command: foo\nUse ,help to list available commands.")
        );
        assert_eq!(invalid, Response::plain("⚠️ Usage: !help [command]"));
    }

    #[tokio::test]
    async fn alias_help_uses_the_configured_alias_target() {
        let (aliases, directory) = aliases_with_mini().await;
        let response = render(&HelpRequest::Unknown("mini".to_owned()), "!", &aliases).response;

        assert!(response.text.starts_with("🔗 !mini\n\n"));
        assert!(response.text.contains("Alias for !fastfetch"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn blockquote_uses_utf16_body_bounds() {
        let response = render(
            &HelpRequest::Topic(CommandKind::Help),
            "🦀",
            &aliases().await,
        )
        .response;
        let text_units: Vec<u16> = response.text.encode_utf16().collect();
        let grammers_client::tl::enums::MessageEntity::Blockquote(entity) = &response.entities[0]
        else {
            panic!("expected a blockquote entity");
        };
        assert!(entity.collapsed);
        let offset = usize::try_from(entity.offset).unwrap();
        let length = usize::try_from(entity.length).unwrap();

        assert_eq!(
            String::from_utf16(&text_units[..offset]).unwrap(),
            "🛠 🦀help [command]\n\n"
        );
        assert_eq!(
            String::from_utf16(&text_units[offset..offset + length]).unwrap(),
            "Shows the command overview or detailed help for a single command.\n\nUsage: 🦀help [command]"
        );
    }

    #[test]
    fn plain_response_has_no_entities() {
        assert!(Response::plain("plain").entities.is_empty());
    }
}

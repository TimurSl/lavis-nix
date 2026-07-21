use crate::commands::{COMMANDS, CommandKind, HelpRequest, definition};

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub text: String,
    pub entities: Vec<grammers_client::tl::enums::MessageEntity>,
}

impl Response {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            entities: Vec::new(),
        }
    }
}

pub struct RenderedHelp {
    pub response: Response,
    pub entity_fallback: bool,
}

pub fn render(request: &HelpRequest, prefix: &str) -> RenderedHelp {
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
        HelpRequest::Unknown(topic) => RenderedHelp {
            response: Response::plain(format!(
                "❓ Unknown command: {topic}\nUse {prefix}help to list available commands."
            )),
            entity_fallback: false,
        },
        HelpRequest::Invalid => RenderedHelp {
            response: Response::plain(format!("⚠️ Usage: {prefix}help [command]")),
            entity_fallback: false,
        },
    }
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
    let text = format!("{heading}\n\n{body}");
    let offset = heading.encode_utf16().count().checked_add(2);
    let length = body.encode_utf16().count();
    let entity = offset
        .and_then(|offset| i32::try_from(offset).ok().zip(i32::try_from(length).ok()))
        .map(|(offset, length)| {
            grammers_client::tl::types::MessageEntityBlockquote {
                offset,
                length,
                collapsed: true,
            }
            .into()
        });

    match entity {
        Some(entity) => RenderedHelp {
            response: Response {
                text,
                entities: vec![entity],
            },
            entity_fallback: false,
        },
        None => RenderedHelp {
            response: Response::plain(text),
            entity_fallback: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{Response, render};
    use crate::commands::{CommandKind, HelpRequest, definition};

    #[test]
    fn overview_uses_registry_order_and_configured_prefix_once_per_command() {
        let response = render(&HelpRequest::Overview, "!").response;

        assert_eq!(response.text.matches('!').count(), 3);
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

    #[test]
    fn command_details_keep_titles_outside_a_single_entity() {
        for (kind, title) in [
            (CommandKind::Ping, "🏓 ,ping\n\n"),
            (CommandKind::Stats, "📊 ,stats\n\n"),
            (CommandKind::Help, "🛠 ,help [command]\n\n"),
        ] {
            let response = render(&HelpRequest::Topic(kind), ",").response;
            let definition = definition(kind);

            assert!(response.text.starts_with(title));
            assert!(response.text.ends_with(&format!(
                "{}\n\nUsage: ,{}",
                definition.description, definition.usage
            )));
            assert_eq!(response.entities.len(), 1);
        }
    }

    #[test]
    fn renders_unknown_and_invalid_help_plainly() {
        let unknown = render(&HelpRequest::Unknown("foo".to_owned()), ",").response;
        let invalid = render(&HelpRequest::Invalid, "!").response;

        assert_eq!(
            unknown,
            Response::plain("❓ Unknown command: foo\nUse ,help to list available commands.")
        );
        assert_eq!(invalid, Response::plain("⚠️ Usage: !help [command]"));
    }

    #[test]
    fn blockquote_uses_utf16_body_bounds() {
        let response = render(&HelpRequest::Topic(CommandKind::Help), "🦀").response;
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

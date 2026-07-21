use crate::command::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Ping,
    Stats,
    Help,
    Fastfetch,
    Alias,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDefinition {
    pub kind: CommandKind,
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub icon: &'static str,
    pub aliasable: bool,
}

pub struct CommandRegistry([CommandDefinition; 5]);

impl CommandRegistry {
    pub fn iter(&self) -> impl Iterator<Item = &CommandDefinition> {
        self.0.iter()
    }

    pub fn canonical_iter(&self) -> impl Iterator<Item = &CommandDefinition> {
        self.0.iter()
    }
}

pub const COMMANDS: CommandRegistry = CommandRegistry([
    CommandDefinition {
        kind: CommandKind::Ping,
        name: "ping",
        usage: "ping",
        summary: "Measure Telegram latency",
        description: "Measures a real Telegram MTProto RPC round-trip over the existing authenticated connection.",
        icon: "🏓",
        aliasable: true,
    },
    CommandDefinition {
        kind: CommandKind::Stats,
        name: "stats",
        usage: "stats",
        summary: "Show runtime statistics",
        description: "Shows fresh Telegram RPC latency, Lavis process uptime, host uptime, resident memory, command count, and package version.",
        icon: "📊",
        aliasable: true,
    },
    CommandDefinition {
        kind: CommandKind::Help,
        name: "help",
        usage: "help [command]",
        summary: "Show command help",
        description: "Shows the command overview or detailed help for a single command.",
        icon: "🛠",
        aliasable: true,
    },
    CommandDefinition {
        kind: CommandKind::Fastfetch,
        name: "fastfetch",
        usage: "fastfetch [options]",
        summary: "Show system information",
        description: "Runs fastfetch with a restricted set of display options.",
        icon: "🖥",
        aliasable: true,
    },
    CommandDefinition {
        kind: CommandKind::Alias,
        name: "alias",
        usage: "alias [list|add <name> <command> [arguments...]|show <name>|del <name>]",
        summary: "Manage command aliases",
        description: "Manages persistent aliases for canonical commands.",
        icon: "🔗",
        aliasable: false,
    },
]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelpRequest {
    Overview,
    Topic(CommandKind),
    Unknown(String),
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Ping,
    Stats,
    Help(HelpRequest),
    Fastfetch(String),
    Alias(AliasRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasRequest {
    List,
    Add {
        name: String,
        target: String,
        args: Vec<String>,
    },
    Delete {
        name: String,
    },
    Show {
        name: String,
    },
    Invalid,
}

impl Action {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Stats => "stats",
            Self::Help(_) => "help",
            Self::Fastfetch(_) => "fastfetch",
            Self::Alias(_) => "alias",
        }
    }
}

pub fn dispatch(command: &Command) -> Option<Action> {
    let definition = canonical_command(&command.name)?;
    match definition.kind {
        CommandKind::Ping => Some(Action::Ping),
        CommandKind::Stats => Some(Action::Stats),
        CommandKind::Help => Some(Action::Help(parse_help_request(&command.args))),
        CommandKind::Fastfetch => Some(Action::Fastfetch(command.args.clone())),
        CommandKind::Alias => Some(Action::Alias(parse_alias_request(&command.args))),
    }
}

pub fn definition(kind: CommandKind) -> &'static CommandDefinition {
    match kind {
        CommandKind::Ping => &COMMANDS.0[0],
        CommandKind::Stats => &COMMANDS.0[1],
        CommandKind::Help => &COMMANDS.0[2],
        CommandKind::Fastfetch => &COMMANDS.0[3],
        CommandKind::Alias => &COMMANDS.0[4],
    }
}

fn parse_alias_request(args: &str) -> AliasRequest {
    let tokens = match shell_words::split(args) {
        Ok(tokens) => tokens,
        Err(_) => return AliasRequest::Invalid,
    };
    match tokens.as_slice() {
        [] => AliasRequest::List,
        [command] if command == "list" => AliasRequest::List,
        [command, name, target, args @ ..] if command == "add" => AliasRequest::Add {
            name: name.clone(),
            target: target.clone(),
            args: args.to_vec(),
        },
        [command, name] if matches!(command.as_str(), "del" | "delete" | "remove") => {
            AliasRequest::Delete { name: name.clone() }
        }
        [command, name] if command == "show" => AliasRequest::Show { name: name.clone() },
        _ => AliasRequest::Invalid,
    }
}

pub fn canonical_command(name: &str) -> Option<&'static CommandDefinition> {
    COMMANDS
        .canonical_iter()
        .find(|command| command.name == name)
}

fn parse_help_request(args: &str) -> HelpRequest {
    let mut topics = args.split_whitespace();
    let Some(topic) = topics.next() else {
        return HelpRequest::Overview;
    };
    if topics.next().is_some() {
        return HelpRequest::Invalid;
    }
    let normalized = topic.to_ascii_lowercase();
    COMMANDS
        .iter()
        .find(|definition| definition.name == normalized)
        .map(|definition| HelpRequest::Topic(definition.kind))
        .unwrap_or_else(|| HelpRequest::Unknown(topic.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::{Action, AliasRequest, COMMANDS, CommandKind, HelpRequest, dispatch};
    use crate::command::Command;

    #[test]
    fn dispatches_ping() {
        let command = Command {
            name: "ping".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), Some(Action::Ping));
    }

    #[test]
    fn dispatches_stats() {
        let command = Command {
            name: "stats".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), Some(Action::Stats));
    }

    #[test]
    fn dispatches_help_overview_and_ascii_case_insensitive_topics() {
        let overview = Command {
            name: "help".to_owned(),
            args: String::new(),
        };
        let topic = Command {
            name: "help".to_owned(),
            args: "PING".to_owned(),
        };
        let alias_topic = Command {
            name: "help".to_owned(),
            args: "alias".to_owned(),
        };

        assert_eq!(
            dispatch(&overview),
            Some(Action::Help(HelpRequest::Overview))
        );
        assert_eq!(
            dispatch(&topic),
            Some(Action::Help(HelpRequest::Topic(CommandKind::Ping)))
        );
        assert_eq!(
            dispatch(&alias_topic),
            Some(Action::Help(HelpRequest::Topic(CommandKind::Alias)))
        );
    }

    #[test]
    fn dispatches_invalid_help_requests() {
        let unknown = Command {
            name: "help".to_owned(),
            args: "missing".to_owned(),
        };
        let invalid = Command {
            name: "help".to_owned(),
            args: "ping extra".to_owned(),
        };

        assert_eq!(
            dispatch(&unknown),
            Some(Action::Help(HelpRequest::Unknown("missing".to_owned())))
        );
        assert_eq!(dispatch(&invalid), Some(Action::Help(HelpRequest::Invalid)));
    }

    #[test]
    fn registry_is_unique_ordered_and_complete() {
        let names = COMMANDS
            .canonical_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["ping", "stats", "help", "fastfetch", "alias"]);
        for definition in COMMANDS.canonical_iter() {
            assert!(!definition.usage.is_empty());
            assert!(!definition.summary.is_empty());
            assert!(!definition.description.is_empty());
            assert!(!definition.icon.is_empty());
            if definition.name == "alias" {
                assert!(!definition.aliasable);
                continue;
            }
            let command = Command {
                name: definition.name.to_owned(),
                args: String::new(),
            };
            assert!(dispatch(&command).is_some());
        }
    }

    #[test]
    fn dispatches_fastfetch_and_alias_requests() {
        assert_eq!(
            dispatch(&Command {
                name: "fastfetch".to_owned(),
                args: "--logo none".to_owned()
            }),
            Some(Action::Fastfetch("--logo none".to_owned()))
        );
        assert_eq!(
            dispatch(&Command {
                name: "alias".to_owned(),
                args: "add sys fastfetch --logo none".to_owned()
            }),
            Some(Action::Alias(AliasRequest::Add {
                name: "sys".to_owned(),
                target: "fastfetch".to_owned(),
                args: vec!["--logo".to_owned(), "none".to_owned()],
            }))
        );
    }

    #[test]
    fn parses_alias_show_and_all_delete_spellings() {
        let alias = |args: &str| {
            dispatch(&Command {
                name: "alias".to_owned(),
                args: args.to_owned(),
            })
        };

        assert_eq!(
            alias("show Mini"),
            Some(Action::Alias(AliasRequest::Show {
                name: "Mini".to_owned(),
            }))
        );
        assert_eq!(alias("show"), Some(Action::Alias(AliasRequest::Invalid)));
        assert_eq!(
            alias("show mini extra"),
            Some(Action::Alias(AliasRequest::Invalid))
        );
        for spelling in ["del", "delete", "remove"] {
            assert_eq!(
                alias(&format!("{spelling} mini")),
                Some(Action::Alias(AliasRequest::Delete {
                    name: "mini".to_owned(),
                }))
            );
        }
    }

    #[test]
    fn ignores_unknown_commands() {
        let command = Command {
            name: "unknown".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), None);
    }
}

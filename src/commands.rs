use crate::command::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    Ping,
    Stats,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDefinition {
    pub kind: CommandKind,
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    pub icon: &'static str,
}

pub const COMMANDS: [CommandDefinition; 3] = [
    CommandDefinition {
        kind: CommandKind::Ping,
        name: "ping",
        usage: "ping",
        summary: "Measure Telegram latency",
        description: "Measures a real Telegram MTProto RPC round-trip over the existing authenticated connection.",
        icon: "🏓",
    },
    CommandDefinition {
        kind: CommandKind::Stats,
        name: "stats",
        usage: "stats",
        summary: "Show runtime statistics",
        description: "Shows fresh Telegram RPC latency, Lavis process uptime, host uptime, resident memory, command count, and package version.",
        icon: "📊",
    },
    CommandDefinition {
        kind: CommandKind::Help,
        name: "help",
        usage: "help [command]",
        summary: "Show command help",
        description: "Shows the command overview or detailed help for a single command.",
        icon: "🛠",
    },
];

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
}

impl Action {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Stats => "stats",
            Self::Help(_) => "help",
        }
    }
}

pub fn dispatch(command: &Command) -> Option<Action> {
    let definition = COMMANDS
        .iter()
        .find(|definition| definition.name == command.name)?;
    match definition.kind {
        CommandKind::Ping => Some(Action::Ping),
        CommandKind::Stats => Some(Action::Stats),
        CommandKind::Help => Some(Action::Help(parse_help_request(&command.args))),
    }
}

pub fn definition(kind: CommandKind) -> &'static CommandDefinition {
    match kind {
        CommandKind::Ping => &COMMANDS[0],
        CommandKind::Stats => &COMMANDS[1],
        CommandKind::Help => &COMMANDS[2],
    }
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
    use super::{Action, COMMANDS, CommandKind, HelpRequest, dispatch};
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

        assert_eq!(
            dispatch(&overview),
            Some(Action::Help(HelpRequest::Overview))
        );
        assert_eq!(
            dispatch(&topic),
            Some(Action::Help(HelpRequest::Topic(CommandKind::Ping)))
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
        let names = COMMANDS.map(|definition| definition.name);
        assert_eq!(names, ["ping", "stats", "help"]);
        for definition in COMMANDS {
            assert!(!definition.usage.is_empty());
            assert!(!definition.summary.is_empty());
            assert!(!definition.description.is_empty());
            assert!(!definition.icon.is_empty());
            let command = Command {
                name: definition.name.to_owned(),
                args: String::new(),
            };
            assert!(dispatch(&command).is_some());
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

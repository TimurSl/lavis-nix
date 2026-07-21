use crate::command::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Ping,
    Stats,
}

impl Action {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Stats => "stats",
        }
    }
}

pub fn dispatch(command: &Command) -> Option<Action> {
    match command.name.as_str() {
        "ping" => Some(Action::Ping),
        "stats" => Some(Action::Stats),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, dispatch};
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
    fn ignores_unknown_commands() {
        let command = Command {
            name: "unknown".to_owned(),
            args: String::new(),
        };

        assert_eq!(dispatch(&command), None);
    }
}

use crate::commands::{COMMANDS, CommandDefinition};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleId {
    Core,
    System,
    Aliases,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModuleSpec {
    pub id: ModuleId,
    pub name: &'static str,
    pub description: &'static str,
    pub icon: &'static str,
}

pub const MODULES: [ModuleSpec; 3] = [
    ModuleSpec {
        id: ModuleId::Core,
        name: "core",
        description: "Essential Lavis commands.",
        icon: "🧩",
    },
    ModuleSpec {
        id: ModuleId::System,
        name: "system",
        description: "System information commands.",
        icon: "🖥",
    },
    ModuleSpec {
        id: ModuleId::Aliases,
        name: "aliases",
        description: "Persistent command alias management.",
        icon: "🔗",
    },
];

pub fn module_by_name(name: &str) -> Option<&'static ModuleSpec> {
    MODULES
        .iter()
        .find(|module| module.name.eq_ignore_ascii_case(name))
}

pub fn module_definition(id: ModuleId) -> &'static ModuleSpec {
    MODULES
        .iter()
        .find(|module| module.id == id)
        .unwrap_or_else(|| unreachable!("all ModuleId values are registered"))
}

pub fn commands_for_module(id: ModuleId) -> impl Iterator<Item = &'static CommandDefinition> {
    COMMANDS
        .canonical_iter()
        .filter(move |command| command.module == id)
}

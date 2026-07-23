use std::sync::Arc;
use std::{collections::BTreeMap, path::PathBuf};
use tokio::sync::Mutex;
use tracing;

use super::{
    MAX_COMMANDS_PER_MODULE,
    manifest::{ExternalCommandDescriptor, ExternalModuleDescriptor},
    process::{ModuleProcess, ProcessStatus},
};
use crate::error::ExternalError;

#[derive(Debug, Clone)]
pub struct ExternalCommandRef {
    pub module_id: String,
    pub command_name: String,
    pub summary_ru: String,
    pub description_ru: String,
    pub usage: String,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExternalModuleStatus {
    pub id: String,
    pub display_name: String,
    pub version: String,
    pub author: String,
    pub capabilities: Vec<String>,
    pub command_count: usize,
    pub status: &'static str,
}

pub struct ExternalManager {
    descriptors: Vec<ExternalModuleDescriptor>,
    processes: BTreeMap<String, ModuleProcess>,
    module_root: PathBuf,
}

impl ExternalManager {
    pub fn new(module_root: PathBuf) -> Self {
        Self {
            descriptors: Vec::new(),
            processes: BTreeMap::new(),
            module_root,
        }
    }

    pub fn set_descriptors(&mut self, descriptors: Vec<ExternalModuleDescriptor>) {
        self.descriptors = descriptors;
    }

    pub fn descriptors(&self) -> &[ExternalModuleDescriptor] {
        &self.descriptors
    }

    pub fn descriptor_by_id(&self, id: &str) -> Option<&ExternalModuleDescriptor> {
        self.descriptors.iter().find(|d| d.id == id)
    }

    pub fn has_running_process(&self, id: &str) -> bool {
        self.processes
            .get(id)
            .is_some_and(|p| p.status() == ProcessStatus::Running)
    }

    pub fn running_command_count(&self) -> usize {
        self.command_refs().len()
    }

    pub fn statuses(&self) -> Vec<ExternalModuleStatus> {
        let mut statuses = Vec::new();
        for desc in &self.descriptors {
            let (status_label, command_count) = if let Some(proc) = self.processes.get(&desc.id) {
                match proc.status() {
                    ProcessStatus::Running => ("активен", proc.descriptor().commands.len()),
                    ProcessStatus::Failed | ProcessStatus::Crashed => ("ошибка", 0),
                    ProcessStatus::Terminated => ("остановлен", 0),
                }
            } else {
                ("не запущен", 0)
            };
            statuses.push(ExternalModuleStatus {
                id: desc.id.clone(),
                display_name: desc.display_name.clone(),
                version: desc.version.clone(),
                author: desc.author.clone(),
                capabilities: desc
                    .capabilities
                    .iter()
                    .map(|c| c.as_str().to_owned())
                    .collect(),
                command_count,
                status: status_label,
            });
        }
        statuses
    }

    pub async fn startup_enabled(&mut self, enabled_ids: &std::collections::BTreeSet<String>) {
        for desc in &self.descriptors {
            if !enabled_ids.contains(&desc.id) {
                continue;
            }
            match ModuleProcess::start(desc.clone(), &self.module_root).await {
                Ok(process) => {
                    tracing::info!(
                        event = "external_module_started",
                        module_id = %desc.id,
                        "External module started"
                    );
                    let id = desc.id.clone();
                    self.processes.insert(id, process);
                }
                Err(error) => {
                    tracing::warn!(
                        event = "external_module_startup_failed",
                        module_id = %desc.id,
                        error = %error,
                        "Не удалось запустить внешний модуль"
                    );
                }
            }
        }
    }

    /// Resolve a dotted command name `module-id.command-name` into
    /// `(module_id, command_name)` if the command exists on a running process.
    pub fn resolve_namespaced_command(&self, dotted: &str) -> Option<(String, String)> {
        let dot = dotted.find('.')?;
        let module_id = &dotted[..dot];
        let command_name = &dotted[dot + 1..];
        if module_id.is_empty() || command_name.is_empty() {
            return None;
        }
        let desc = self.descriptor_by_id(module_id)?;
        if !self.has_running_process(module_id) {
            return None;
        }
        desc.commands.iter().find(|c| c.name == command_name)?;
        Some((module_id.to_owned(), command_name.to_owned()))
    }

    pub fn command_refs(&self) -> Vec<ExternalCommandRef> {
        let mut refs = Vec::new();
        for process in self.processes.values() {
            if process.status() != ProcessStatus::Running {
                continue;
            }
            let desc = process.descriptor();
            for cmd in desc.commands.iter().take(MAX_COMMANDS_PER_MODULE) {
                refs.push(ExternalCommandRef {
                    module_id: desc.id.clone(),
                    command_name: cmd.name.clone(),
                    summary_ru: cmd.summary_ru.clone(),
                    description_ru: cmd.description_ru.clone(),
                    usage: cmd.usage.clone(),
                    examples: cmd.examples.clone(),
                });
            }
        }
        refs
    }

    pub fn find_command(&self, module_id: &str, command_name: &str) -> Option<ExternalCommandRef> {
        let process = self.processes.get(module_id)?;
        if process.status() != ProcessStatus::Running {
            return None;
        }
        let cmd = process
            .descriptor()
            .commands
            .iter()
            .find(|c| c.name == command_name)?;
        Some(ExternalCommandRef {
            module_id: process.descriptor().id.clone(),
            command_name: cmd.name.clone(),
            summary_ru: cmd.summary_ru.clone(),
            description_ru: cmd.description_ru.clone(),
            usage: cmd.usage.clone(),
            examples: cmd.examples.clone(),
        })
    }

    pub fn find_descriptor_command(
        &self,
        module_id: &str,
        command_name: &str,
    ) -> Option<&ExternalCommandDescriptor> {
        self.descriptor_by_id(module_id)?
            .commands
            .iter()
            .find(|c| c.name == command_name)
    }

    pub async fn execute(
        &mut self,
        module_id: &str,
        command_name: &str,
        arguments: &str,
    ) -> Result<String, ExternalError> {
        let process = self
            .processes
            .get_mut(module_id)
            .ok_or(ExternalError::Unavailable)?;

        if process.status() != ProcessStatus::Running {
            return Err(ExternalError::Unavailable);
        }

        let result = process.execute(command_name, arguments).await?;
        Ok(result)
    }

    pub async fn shutdown_all(&mut self) {
        tracing::info!(
            event = "external_modules_shutdown",
            "Shutting down external modules"
        );
        let ids: Vec<String> = self.processes.keys().cloned().collect();
        for id in &ids {
            if let Some(process) = self.processes.get_mut(id)
                && process.status() == ProcessStatus::Running
                && process.graceful_shutdown().await.is_err()
            {
                tracing::warn!(
                    event = "external_module_shutdown_forced",
                    module_id = %id,
                    "Forcefully terminating external module"
                );
                process.terminate().await;
            }
        }
        self.processes.clear();
    }

    pub fn remove_crashed(&mut self, module_id: &str) {
        if let Some(proc) = self.processes.get(module_id)
            && proc.status() == ProcessStatus::Crashed
        {
            self.processes.remove(module_id);
        }
    }

    pub fn has_command(&self, module_id: &str, command_name: &str) -> bool {
        self.find_command(module_id, command_name).is_some()
    }
}

#[derive(Clone)]
pub struct ExternalManagerHandle {
    inner: Arc<Mutex<ExternalManager>>,
}

impl ExternalManagerHandle {
    pub fn new(module_root: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ExternalManager::new(module_root))),
        }
    }

    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ExternalManager> {
        self.inner.lock().await
    }

    pub fn new_for_tests(manager: ExternalManager) -> Self {
        Self {
            inner: Arc::new(Mutex::new(manager)),
        }
    }
}

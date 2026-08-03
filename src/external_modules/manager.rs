use std::collections::BTreeMap;
use std::sync::Arc;
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
    processes: BTreeMap<String, Arc<Mutex<ModuleProcess>>>,
    gateway: Option<Arc<dyn super::gateway::TelegramGateway>>,
    timer_tasks: BTreeMap<String, Vec<tokio::task::JoinHandle<()>>>,
}

impl Default for ExternalManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ExternalManager {
    pub fn new() -> Self {
        Self {
            descriptors: Vec::new(),
            processes: BTreeMap::new(),
            gateway: None,
            timer_tasks: BTreeMap::new(),
        }
    }

    pub fn set_descriptors(&mut self, descriptors: Vec<ExternalModuleDescriptor>) {
        self.descriptors = descriptors;
    }

    pub fn set_gateway(&mut self, gateway: Arc<dyn super::gateway::TelegramGateway>) {
        self.gateway = Some(gateway);
    }

    pub fn descriptors(&self) -> &[ExternalModuleDescriptor] {
        &self.descriptors
    }

    /// Registers a newly installed descriptor without changing process or
    /// enabled-state ownership. A duplicate ID is rejected before mutation so
    /// a stale runtime snapshot cannot create ambiguous command routing.
    pub fn register_installed_descriptor(&mut self, descriptor: ExternalModuleDescriptor) -> bool {
        if self.descriptor_by_id(&descriptor.id).is_some() {
            return false;
        }
        self.descriptors.push(descriptor);
        true
    }

    pub fn descriptor_by_id(&self, id: &str) -> Option<&ExternalModuleDescriptor> {
        self.descriptors.iter().find(|d| d.id == id)
    }

    pub fn has_running_process(&self, id: &str) -> bool {
        self.processes.get(id).is_some_and(|p| {
            p.try_lock()
                .is_ok_and(|p| p.status() == ProcessStatus::Running)
        })
    }

    pub fn running_command_count(&self) -> usize {
        self.command_refs().len()
    }

    pub fn statuses(&self) -> Vec<ExternalModuleStatus> {
        let mut statuses = Vec::new();
        for desc in &self.descriptors {
            let status_label = if let Some(proc) = self
                .processes
                .get(&desc.id)
                .and_then(|proc| proc.try_lock().ok())
            {
                match proc.status() {
                    ProcessStatus::Running => "активен",
                    ProcessStatus::Failed | ProcessStatus::Crashed => "ошибка",
                    ProcessStatus::Terminated => "остановлен",
                }
            } else {
                "установлен, выключен"
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
                command_count: desc.commands.len(),
                status: status_label,
            });
        }
        statuses
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

    pub fn resolve_default_command(&self, module_id: &str) -> Option<(String, String)> {
        let process = self.processes.get(module_id)?.try_lock().ok()?;
        (process.status() == ProcessStatus::Running)
            .then(|| process.descriptor().default_command.as_ref())
            .flatten()
            .map(|command| (module_id.to_owned(), command.clone()))
    }

    pub fn command_refs(&self) -> Vec<ExternalCommandRef> {
        let mut refs = Vec::new();
        for process in self.processes.values() {
            let Ok(process) = process.try_lock() else {
                continue;
            };
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
        let process = self.processes.get(module_id)?.try_lock().ok()?;
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
            .get(module_id)
            .cloned()
            .ok_or(ExternalError::Unavailable)?;
        let mut process = process.lock().await;

        if process.status() != ProcessStatus::Running {
            return Err(ExternalError::Unavailable);
        }

        let result = process.execute(command_name, arguments).await?;
        Ok(result)
    }

    pub async fn dispatch_event(
        &mut self,
        module_id: &str,
        event: super::protocol::MessageEventKind,
        payload: super::protocol::MessageEvent,
    ) -> Result<(String, Vec<super::protocol::EventAction>), ExternalError> {
        let process = self
            .processes
            .get(module_id)
            .cloned()
            .ok_or(ExternalError::Unavailable)?;
        let mut process = process.lock().await;
        if process.status() != ProcessStatus::Running || process.descriptor().protocol_version < 3 {
            return Err(ExternalError::Unavailable);
        }
        process.dispatch_event(event, payload).await
    }

    pub async fn shutdown_all(&mut self) {
        tracing::info!(
            event = "external_modules_shutdown",
            "Shutting down external modules"
        );
        let timer_tasks = std::mem::take(&mut self.timer_tasks);
        stop_timer_tasks(timer_tasks).await;
        let processes: Vec<(String, Arc<Mutex<ModuleProcess>>)> = self
            .processes
            .iter()
            .map(|(id, process)| (id.clone(), process.clone()))
            .collect();
        for (id, process) in processes {
            let mut process = process.lock().await;
            match process.status() {
                ProcessStatus::Running => {
                    if process.graceful_shutdown().await.is_err() {
                        tracing::warn!(event = "external_module_shutdown_forced", module_id = %id, "Forcefully terminating external module");
                        process.terminate().await;
                    }
                }
                // A crashed process already ran fatal cleanup. Re-signalling
                // its old PID could hit a reused process group.
                ProcessStatus::Crashed | ProcessStatus::Failed | ProcessStatus::Terminated => {}
            }
        }
        self.processes.clear();
    }

    pub fn remove_crashed(&mut self, module_id: &str) {
        if let Some(proc) = self.processes.get(module_id)
            && proc
                .try_lock()
                .is_ok_and(|proc| proc.status() == ProcessStatus::Crashed)
        {
            self.processes.remove(module_id);
        }
    }

    pub fn has_command(&self, module_id: &str, command_name: &str) -> bool {
        self.find_command(module_id, command_name).is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExternalRuntimeSnapshot {
    pub command_refs: Vec<ExternalCommandRef>,
    pub descriptors: Vec<ExternalModuleDescriptor>,
    pub module_statuses: Vec<ExternalModuleStatus>,
    pub active_commands: std::collections::HashSet<String>,
    pub active_defaults: std::collections::HashMap<String, String>,
}

impl ExternalRuntimeSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_manager(manager: &ExternalManager) -> Self {
        let command_refs = manager.command_refs();
        let descriptors = manager.descriptors().to_vec();
        let module_statuses = manager.statuses();
        let active_commands = command_refs
            .iter()
            .map(|r| format!("{}.{}", r.module_id, r.command_name))
            .collect();
        let active_defaults = manager
            .processes
            .values()
            .filter_map(|process| process.try_lock().ok())
            .filter(|process| process.status() == ProcessStatus::Running)
            .filter_map(|process| {
                process
                    .descriptor()
                    .default_command
                    .as_ref()
                    .map(|command| (process.descriptor().id.clone(), command.clone()))
            })
            .collect();
        Self {
            command_refs,
            descriptors,
            module_statuses,
            active_commands,
            active_defaults,
        }
    }

    pub fn refresh_from(&mut self, manager: &ExternalManager) {
        *self = Self::from_manager(manager);
    }
}

#[derive(Clone)]
pub struct ExternalManagerHandle {
    inner: Arc<Mutex<ExternalManager>>,
}

impl ExternalManagerHandle {
    pub fn new(manager: ExternalManager) -> Self {
        Self {
            inner: Arc::new(Mutex::new(manager)),
        }
    }

    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ExternalManager> {
        self.inner.lock().await
    }

    pub async fn snapshot(&self) -> ExternalRuntimeSnapshot {
        let mgr = self.inner.lock().await;
        ExternalRuntimeSnapshot::from_manager(&mgr)
    }

    /// Starts children without retaining the manager mutex. Process I/O belongs
    /// to the individual process mutex; the manager only owns the index.
    pub async fn startup_enabled(&self, enabled_ids: &std::collections::BTreeSet<String>) {
        let (descriptors, gateway) = {
            let manager = self.inner.lock().await;
            (
                manager
                    .descriptors
                    .iter()
                    .filter(|descriptor| enabled_ids.contains(&descriptor.id))
                    .cloned()
                    .collect::<Vec<_>>(),
                manager.gateway.clone(),
            )
        };
        for descriptor in descriptors {
            let id = descriptor.id.clone();
            let old_timers = {
                let mut manager = self.inner.lock().await;
                manager.timer_tasks.remove(&id)
            };
            if let Some(tasks) = old_timers {
                stop_timer_tasks(BTreeMap::from([(id.clone(), tasks)])).await;
            }
            match ModuleProcess::start_with_gateway(descriptor.clone(), gateway.clone()).await {
                Ok(process) => {
                    let replaced = {
                        let mut manager = self.inner.lock().await;
                        manager
                            .processes
                            .insert(id.clone(), Arc::new(Mutex::new(process)))
                    };
                    if let Some(replaced) = replaced {
                        let mut replaced = replaced.lock().await;
                        replaced.terminate().await;
                    }
                    tracing::info!(event = "external_module_started", module_id = %id, "External module started");
                    if descriptor
                        .capabilities
                        .contains(&super::manifest::ExternalCapability::Timer)
                    {
                        self.start_timers(id, descriptor.timer_subscriptions).await;
                    }
                }
                Err(error) => {
                    tracing::warn!(event = "external_module_startup_failed", module_id = %id, error = %error, "Не удалось запустить внешний модуль")
                }
            }
        }
    }

    /// Removes the index before awaiting child shutdown, so status refresh and
    /// routing never wait behind a slow process shutdown.
    pub async fn shutdown_all(&self) {
        let (timer_tasks, processes) = {
            let mut manager = self.inner.lock().await;
            (
                std::mem::take(&mut manager.timer_tasks),
                std::mem::take(&mut manager.processes),
            )
        };
        stop_timer_tasks(timer_tasks).await;
        for (id, process) in processes {
            let mut process = process.lock().await;
            if process.status() != ProcessStatus::Running {
                continue;
            }
            if process.graceful_shutdown().await.is_ok() {
                continue;
            }
            tracing::warn!(event = "external_module_shutdown_forced", module_id = %id, "Forcefully terminating external module");
            process.terminate().await;
        }
    }

    async fn start_timers(
        &self,
        module_id: String,
        timers: Vec<super::manifest::TimerSubscription>,
    ) {
        if timers.is_empty() {
            return;
        }
        let mut tasks = Vec::with_capacity(timers.len());
        for timer in timers {
            let manager = Arc::downgrade(&self.inner);
            let id = module_id.clone();
            tasks.push(tokio::spawn(async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(timer.interval_seconds));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let Some(manager) = manager.upgrade() else {
                        break;
                    };
                    let process = {
                        let manager = manager.lock().await;
                        manager.processes.get(&id).cloned()
                    };
                    let Some(process) = process else { break; };
                    let Ok(mut process) = process.try_lock() else { continue; };
                    if process.status() != ProcessStatus::Running { break; }
                    if let Err(error) = process
                        .dispatch_timer(format!("timer-{}", super::protocol::request_id()))
                        .await
                    {
                        tracing::warn!(event = "external_timer_failed", module_id = %id, error = %error, "External timer disabled after module failure");
                        break;
                    }
                }
            }));
        }
        let replaced = {
            let mut manager = self.inner.lock().await;
            if manager.processes.contains_key(&module_id) {
                manager.timer_tasks.insert(module_id.clone(), tasks)
            } else {
                Some(tasks)
            }
        };
        if let Some(tasks) = replaced {
            stop_timer_tasks(BTreeMap::from([(module_id, tasks)])).await;
        }
    }

    pub async fn dispatch_event(
        &self,
        module_id: &str,
        event: super::protocol::MessageEventKind,
        payload: super::protocol::MessageEvent,
    ) -> Result<(String, Vec<super::protocol::EventAction>), ExternalError> {
        let process = {
            let manager = self.inner.lock().await;
            manager.processes.get(module_id).cloned()
        }
        .ok_or(ExternalError::Unavailable)?;
        let mut process = process.lock().await;
        if process.status() != ProcessStatus::Running || process.descriptor().protocol_version < 3 {
            return Err(ExternalError::Unavailable);
        }
        process.dispatch_event(event, payload).await
    }

    pub async fn execute(
        &self,
        module_id: &str,
        command_name: &str,
        arguments: &str,
        argument_entities: &[super::protocol::CustomEmojiEntity],
    ) -> Result<String, ExternalError> {
        let process = {
            let manager = self.inner.lock().await;
            manager.processes.get(module_id).cloned()
        }
        .ok_or(ExternalError::Unavailable)?;
        let mut process = process.lock().await;
        if process.status() != ProcessStatus::Running {
            return Err(ExternalError::Unavailable);
        }
        process
            .execute_with_entities(command_name, arguments, argument_entities)
            .await
    }
}

async fn stop_timer_tasks(tasks: BTreeMap<String, Vec<tokio::task::JoinHandle<()>>>) {
    for task in tasks.values().flatten() {
        task.abort();
    }
    for task in tasks.into_values().flatten() {
        let _ = task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::{ExternalManager, ExternalManagerHandle};
    use crate::external_modules::manifest::{ExternalCommandDescriptor, ExternalModuleDescriptor};
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    fn descriptor(id: &str, version: &str) -> ExternalModuleDescriptor {
        ExternalModuleDescriptor {
            protocol_version: 2,
            id: id.to_owned(),
            display_name: "Sample".to_owned(),
            version: version.to_owned(),
            author: "Author".to_owned(),
            entrypoint: PathBuf::from("run"),
            module_dir: PathBuf::new(),
            capabilities: vec![],
            default_command: None,
            subscriptions: vec![],
            timer_subscriptions: vec![],
            actions: vec![],
            commands: vec![ExternalCommandDescriptor {
                name: "run".to_owned(),
                summary_ru: "run".to_owned(),
                description_ru: "run".to_owned(),
                usage: "run".to_owned(),
                examples: vec![],
            }],
        }
    }

    #[test]
    fn discovered_but_not_running_module_is_disabled_with_descriptor_command_count() {
        let mut manager = ExternalManager::new();
        manager.set_descriptors(vec![descriptor("sample", "1.0")]);

        let statuses = manager.statuses();
        assert_eq!(statuses[0].status, "установлен, выключен");
        assert_eq!(statuses[0].command_count, 1);
    }

    #[test]
    fn installed_descriptor_registration_rejects_duplicates_without_starting_a_process() {
        let mut manager = ExternalManager::new();
        assert!(manager.register_installed_descriptor(descriptor("sample", "1.0")));
        assert!(!manager.register_installed_descriptor(descriptor("sample", "2.0")));

        assert_eq!(manager.descriptors().len(), 1);
        assert_eq!(manager.descriptor_by_id("sample").unwrap().version, "1.0");
        assert!(!manager.has_running_process("sample"));
        assert!(manager.command_refs().is_empty());
    }

    struct DropProbe(Arc<AtomicBool>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn shutdown_aborts_and_joins_registered_timer_tasks() {
        let handle = ExternalManagerHandle::new(ExternalManager::new());
        let stopped = Arc::new(AtomicBool::new(false));
        let probe = Arc::clone(&stopped);
        let (started, started_receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _probe = DropProbe(probe);
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        started_receiver.await.unwrap();
        {
            let mut manager = handle.lock().await;
            manager.timer_tasks.insert("timer".to_owned(), vec![task]);
        }
        handle.shutdown_all().await;
        assert!(stopped.load(Ordering::Acquire));
        assert!(handle.lock().await.timer_tasks.is_empty());
    }

    #[tokio::test]
    async fn timer_task_teardown_does_not_keep_manager_alive() {
        let handle = ExternalManagerHandle::new(ExternalManager::new());
        let weak = Arc::downgrade(&handle.inner);
        let task = tokio::spawn(async move {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        });
        drop(handle);
        task.await.unwrap();
    }

    #[cfg(all(feature = "fixture-tests", unix))]
    mod scheduler_tests {
        use super::*;
        use crate::external_modules::{
            manifest::{ExternalCapability, TimerSubscription},
            process::ModuleProcess,
        };
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            path::PathBuf,
            sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
            time::{Duration, SystemTime, UNIX_EPOCH},
        };
        use tokio::sync::Mutex;

        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

        const TIMER_MODULE: &str = r#"#!/usr/bin/env python3
import json, sys
marker = __MARKER__
fail = __FAIL__
for line in sys.stdin:
    message = json.loads(line)
    request_id = message["request_id"]
    if message["type"] == "initialize":
        print(json.dumps({"protocol_version": 5, "type": "initialized", "request_id": request_id, "module_id": message["module_id"]}), flush=True)
    elif message["type"] == "event" and message["event"] == "timer.tick":
        with open(marker, "a") as output:
            output.write("tick\n")
        if fail:
            sys.exit(0)
        print(json.dumps({"protocol_version": 5, "type": "event_result", "request_id": request_id, "actions": []}), flush=True)
    elif message["type"] == "shutdown":
        sys.exit(0)
"#;

        async fn timer_fixture(
            fail: bool,
        ) -> (String, Arc<Mutex<ModuleProcess>>, PathBuf, PathBuf) {
            let nonce = format!(
                "{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_FIXTURE.fetch_add(1, AtomicOrdering::Relaxed)
            );
            let directory = std::env::temp_dir().join(format!("lavis-manager-timer-{nonce}"));
            let bin = directory.join("bin");
            let marker = directory.join("ticks");
            fs::create_dir_all(&bin).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
            let entrypoint = bin.join("timer-module");
            let python = std::env::var_os("PATH")
                .into_iter()
                .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .map(|path| path.join("python3"))
                .find(|path| path.is_file())
                .expect("fixture tests require python3");
            let script = TIMER_MODULE
                .replacen(
                    "#!/usr/bin/env python3",
                    &format!("#!{}", python.display()),
                    1,
                )
                .replace("__MARKER__", &format!("{:?}", marker))
                .replace("__FAIL__", if fail { "True" } else { "False" });
            fs::write(&entrypoint, script).unwrap();
            fs::set_permissions(&entrypoint, fs::Permissions::from_mode(0o700)).unwrap();
            let id = format!(
                "timer{}",
                NEXT_FIXTURE.fetch_add(1, AtomicOrdering::Relaxed)
            );
            let descriptor = ExternalModuleDescriptor {
                protocol_version: 5,
                id: id.clone(),
                display_name: id.clone(),
                version: "test".to_owned(),
                author: "test".to_owned(),
                entrypoint,
                module_dir: directory.clone(),
                capabilities: vec![ExternalCapability::Timer],
                default_command: None,
                subscriptions: vec![],
                timer_subscriptions: vec![TimerSubscription {
                    interval_seconds: 1,
                }],
                actions: vec![],
                commands: vec![],
            };
            let process = ModuleProcess::start(descriptor).await.unwrap();
            (id, Arc::new(Mutex::new(process)), directory, marker)
        }

        async fn install_timer_process(
            handle: &ExternalManagerHandle,
            id: String,
            process: Arc<Mutex<ModuleProcess>>,
        ) {
            handle.lock().await.processes.insert(id, process);
        }

        #[tokio::test]
        async fn scheduler_starts_after_initialize_and_delays_first_tick() {
            let (id, process, directory, marker) = timer_fixture(false).await;
            let handle = ExternalManagerHandle::new(ExternalManager::new());
            install_timer_process(&handle, id.clone(), process).await;
            handle
                .start_timers(
                    id,
                    vec![TimerSubscription {
                        interval_seconds: 1,
                    }],
                )
                .await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(!marker.exists());
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert!(marker.exists());
            handle.shutdown_all().await;
            fs::remove_dir_all(directory).unwrap();
        }

        #[tokio::test]
        async fn scheduler_skips_busy_process_and_stops_after_dispatch_failure() {
            let (id, process, directory, marker) = timer_fixture(false).await;
            let handle = ExternalManagerHandle::new(ExternalManager::new());
            install_timer_process(&handle, id.clone(), Arc::clone(&process)).await;
            let busy = process.lock().await;
            handle
                .start_timers(
                    id.clone(),
                    vec![TimerSubscription {
                        interval_seconds: 1,
                    }],
                )
                .await;
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert!(!marker.exists());
            drop(busy);
            tokio::time::sleep(Duration::from_millis(1100)).await;
            assert!(marker.exists());
            handle.shutdown_all().await;
            fs::remove_dir_all(directory).unwrap();

            let (id, process, directory, _marker) = timer_fixture(true).await;
            let handle = ExternalManagerHandle::new(ExternalManager::new());
            install_timer_process(&handle, id.clone(), process).await;
            handle
                .start_timers(
                    id.clone(),
                    vec![TimerSubscription {
                        interval_seconds: 1,
                    }],
                )
                .await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            assert!(handle.lock().await.timer_tasks[&id][0].is_finished());
            handle.shutdown_all().await;
            fs::remove_dir_all(directory).unwrap();
        }

        #[tokio::test]
        async fn scheduler_replacement_and_shutdown_remove_actual_tasks() {
            let (id, process, directory, _marker) = timer_fixture(false).await;
            let handle = ExternalManagerHandle::new(ExternalManager::new());
            install_timer_process(&handle, id.clone(), process).await;
            handle
                .start_timers(
                    id.clone(),
                    vec![TimerSubscription {
                        interval_seconds: 1,
                    }],
                )
                .await;
            handle
                .start_timers(
                    id.clone(),
                    vec![TimerSubscription {
                        interval_seconds: 1,
                    }],
                )
                .await;
            assert_eq!(handle.lock().await.timer_tasks[&id].len(), 1);
            handle.shutdown_all().await;
            assert!(handle.lock().await.timer_tasks.is_empty());
            fs::remove_dir_all(directory).unwrap();
        }
    }
}

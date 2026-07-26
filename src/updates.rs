use anyhow::Context;
use grammers_client::{
    client::UpdateStream,
    message::InputReactions,
    update::{Message, Update},
};
use grammers_session::types::PeerId;
use std::future::Future;
use tokio::task::JoinSet;

use crate::{
    command::parse,
    commands::{Action, dispatch},
    runtime::{CreatedEventDispatchResult, RuntimeState, invocation_error_category},
};

const MAX_EVENT_DISPATCH_TASKS: usize = 32;

struct EventDispatches {
    tasks: JoinSet<()>,
}

enum UpdateOrEvent<U> {
    Update(U),
    Event(Option<Result<(), tokio::task::JoinError>>),
}

impl EventDispatches {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
        }
    }

    fn try_spawn(&mut self, task: impl Future<Output = ()> + Send + 'static) -> bool {
        if self.tasks.len() >= MAX_EVENT_DISPATCH_TASKS {
            return false;
        }
        self.tasks.spawn(task);
        true
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn has_capacity(&self) -> bool {
        self.tasks.len() < MAX_EVENT_DISPATCH_TASKS
    }

    async fn next_update_or_event<U>(
        &mut self,
        update: impl Future<Output = U>,
    ) -> UpdateOrEvent<U> {
        if self.is_empty() {
            return UpdateOrEvent::Update(update.await);
        }
        tokio::select! {
            update = update => UpdateOrEvent::Update(update),
            completed = self.tasks.join_next() => UpdateOrEvent::Event(completed),
        }
    }

    async fn abort_and_drain(&mut self) {
        self.tasks.abort_all();
        while self.tasks.join_next().await.is_some() {}
    }
}

pub async fn run(
    stream: &mut UpdateStream,
    self_user_id: PeerId,
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
) -> anyhow::Result<()> {
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut event_dispatches = EventDispatches::new();

    loop {
        tokio::select! {
            signal = &mut shutdown => {
                signal.context("failed to listen for Ctrl-C shutdown signal")?;
                event_dispatches.abort_and_drain().await;
                stream
                    .sync_update_state()
                    .await
                    .map_err(anyhow::Error::from_boxed)
                    .context("failed to synchronize Telegram update state")?;
                return Ok(());
            }
            next = event_dispatches.next_update_or_event(stream.next()) => {
                match next {
                    UpdateOrEvent::Event(Some(Ok(()))) => {}
                    UpdateOrEvent::Event(Some(Err(error))) => tracing::warn!(event = "external_event_task_failed", error = %error, "External event task failed"),
                    UpdateOrEvent::Event(None) => {}
                    UpdateOrEvent::Update(update) => {
                        if update.is_err() {
                            event_dispatches.abort_and_drain().await;
                        }
                        let update = update.context("Telegram update stream ended or failed")?;
                        process_update(
                            update,
                            self_user_id,
                            client,
                            runtime,
                            &mut event_dispatches,
                        ).await;
                    }
                }
            }
        }
    }
}

async fn process_update(
    update: Update,
    self_user_id: PeerId,
    client: &grammers_client::Client,
    runtime: &mut RuntimeState,
    event_dispatches: &mut EventDispatches,
) {
    let (message, edited) = match update {
        Update::NewMessage(message) => (message, false),
        Update::MessageEdited(message) => (message, true),
        _ => return,
    };
    let message_id = message.id();
    let peer_id = message.peer_id();
    if edited && runtime.consume_expected_self_edit(peer_id, message_id, message.text()) {
        tracing::debug!(
            event = "command_self_edit_suppressed",
            message_id,
            "Suppressed the expected command response edit"
        );
        return;
    }
    let outgoing = message.outgoing();
    let authored_by_self = is_self_authored(message.sender_id(), outgoing, self_user_id);
    tracing::debug!(
        event = "telegram_new_message",
        message_id,
        outgoing,
        authored_by_self,
        "Received Telegram message update"
    );

    if !edited {
        let entities = crate::external_modules::entities::project_custom_emoji_entities(
            message.fmt_entities(),
            0,
            message.text().encode_utf16().count(),
        );
        if !event_dispatches.has_capacity() {
            tracing::warn!(
                event = "external_event_task_skipped",
                capacity = MAX_EVENT_DISPATCH_TASKS,
                "Skipped external event dispatch because the task queue is full"
            );
        } else if let Some(dispatch) =
            runtime.prepare_created_event_dispatch(message.text(), outgoing, entities)
        {
            let reaction_message = message.clone();
            let spawned = event_dispatches.try_spawn(async move {
                let result = dispatch.execute().await;
                handle_event_dispatch(reaction_message, result).await;
            });
            debug_assert!(
                spawned,
                "event dispatch capacity was checked before spawning"
            );
        }
    }

    let Some(mut action) = route(authored_by_self, message.text(), runtime) else {
        return;
    };
    if let Action::External(invocation) = &mut action {
        invocation.argument_entities = command_argument_entities(
            message.text(),
            runtime.prefix(),
            &invocation.arguments,
            message.fmt_entities(),
        );
    }
    tracing::debug!(
        event = "command_matched",
        command = action.name(),
        message_id,
        "Matched authenticated command"
    );

    let response = runtime.execute(client, &action, message_id).await;
    let rendered_text = response.text;
    let input = grammers_client::message::InputMessage::new()
        .text(rendered_text.clone())
        .fmt_entities(response.entities);
    runtime.register_expected_self_edit(peer_id, message_id, rendered_text.clone());
    match message.edit(input).await {
        Ok(()) => {
            tracing::debug!(
                event = "command_edit_succeeded",
                command = action.name(),
                message_id,
                "Edited outgoing command message"
            );
        }
        Err(error) => {
            runtime.remove_expected_self_edit(peer_id, message_id, &rendered_text);
            tracing::warn!(
                event = "command_edit_failed",
                command = action.name(),
                message_id,
                error_category = invocation_error_category(&error),
                error = %error,
                "Failed to edit outgoing command message"
            );
        }
    }
}

async fn handle_event_dispatch(message: Message, result: CreatedEventDispatchResult) {
    for failure in result.failures {
        tracing::warn!(
            event = "external_event_failed",
            module_id = %failure.module_id,
            error_category = failure.category,
            "External event failed"
        );
    }
    for action in result.actions {
        let reaction = match action.reaction {
            crate::external_modules::protocol::ReactionSpec::Emoji(emoji) => {
                InputReactions::emoticon(emoji)
            }
            crate::external_modules::protocol::ReactionSpec::CustomEmoji { document_id } => {
                match document_id.parse::<i64>() {
                    Ok(document_id) => InputReactions::custom_emoji(document_id),
                    Err(_) => continue,
                }
            }
        };
        if let Err(error) = message.react(reaction).await {
            tracing::warn!(
                event = "external_reaction_failed",
                error_category = invocation_error_category(&error),
                "External reaction action failed"
            );
        }
    }
}

fn command_argument_entities(
    text: &str,
    prefix: &str,
    arguments: &str,
    entities: Option<&Vec<grammers_client::tl::enums::MessageEntity>>,
) -> Vec<crate::external_modules::protocol::CustomEmojiEntity> {
    if arguments.is_empty() {
        return Vec::new();
    }
    let Some(command_text) = text.strip_prefix(prefix) else {
        return Vec::new();
    };
    let command_text = command_text.trim_start();
    let Some((_, trailing)) = command_text.split_once(char::is_whitespace) else {
        return Vec::new();
    };
    let argument_text = trailing.trim();
    if argument_text != arguments {
        return Vec::new();
    }
    let start_byte = argument_text.as_ptr() as usize - text.as_ptr() as usize;
    let start_utf16 = text[..start_byte].encode_utf16().count();
    crate::external_modules::entities::project_custom_emoji_entities(
        entities,
        start_utf16,
        start_utf16 + argument_text.encode_utf16().count(),
    )
}

fn is_self_authored(sender_id: Option<PeerId>, outgoing: bool, self_user_id: PeerId) -> bool {
    match sender_id {
        Some(sender_id) if sender_id == PeerId::self_user() => outgoing,
        Some(sender_id) => sender_id == self_user_id,
        None => false,
    }
}

fn route(authored_by_self: bool, text: &str, runtime: &RuntimeState) -> Option<Action> {
    let command = authored_by_self
        .then(|| parse(text, runtime.prefix()))
        .flatten()?;
    // Order: built-in > external namespaced > external default > alias.
    dispatch(&command)
        .or_else(|| runtime.resolve_external(&command.name, &command.args))
        .or_else(|| {
            if runtime.has_external_module(&command.name) {
                runtime.resolve_external_default(&command.name, &command.args)
            } else {
                runtime.resolve_alias(&command.name, &command.args)
            }
        })
}

#[cfg(test)]
mod tests {
    use grammers_session::types::PeerId;
    use tokio::sync::oneshot;

    use super::{
        EventDispatches, MAX_EVENT_DISPATCH_TASKS, UpdateOrEvent, is_self_authored, route,
    };
    use crate::commands::{Action, ExternalInvocation, PrefixRequest};
    use crate::{
        aliases::{Alias, AliasStore},
        external_modules::{
            manager::ExternalRuntimeSnapshot,
            manifest::{ExternalCommandDescriptor, ExternalModuleDescriptor},
        },
        runtime::RuntimeState,
        settings::SettingsStore,
    };
    use std::{collections::HashMap, path::PathBuf, time::Instant};

    #[tokio::test]
    async fn event_dispatches_limit_pending_tasks_and_skip_overload() {
        let mut dispatches = EventDispatches::new();
        assert_eq!(MAX_EVENT_DISPATCH_TASKS, 32);

        for _ in 0..MAX_EVENT_DISPATCH_TASKS {
            assert!(dispatches.try_spawn(std::future::pending()));
        }
        assert!(!dispatches.try_spawn(std::future::pending()));

        dispatches.abort_and_drain().await;
        assert!(dispatches.is_empty());
    }

    #[tokio::test]
    async fn ready_update_is_processed_while_an_event_dispatch_is_pending() {
        let mut dispatches = EventDispatches::new();
        assert!(dispatches.try_spawn(std::future::pending()));

        assert!(matches!(
            dispatches.next_update_or_event(async { "update" }).await,
            UpdateOrEvent::Update("update")
        ));

        dispatches.abort_and_drain().await;
    }

    #[tokio::test]
    async fn completed_event_is_reaped_while_another_dispatch_is_pending() {
        let mut dispatches = EventDispatches::new();
        assert!(dispatches.try_spawn(std::future::pending()));
        assert!(dispatches.try_spawn(async {}));

        assert!(matches!(
            dispatches
                .next_update_or_event(std::future::pending::<()>())
                .await,
            UpdateOrEvent::Event(Some(Ok(())))
        ));
        assert_eq!(dispatches.tasks.len(), 1);

        dispatches.abort_and_drain().await;
    }

    #[tokio::test]
    async fn shutdown_aborts_and_drains_event_dispatches() {
        struct DropSignal(Option<oneshot::Sender<()>>);

        impl Drop for DropSignal {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let mut dispatches = EventDispatches::new();
        let (dropped, received_drop) = oneshot::channel();
        assert!(dispatches.try_spawn(async move {
            let _signal = DropSignal(Some(dropped));
            std::future::pending::<()>().await;
        }));
        tokio::task::yield_now().await;

        dispatches.abort_and_drain().await;
        assert!(dispatches.is_empty());
        assert_eq!(received_drop.await, Ok(()));
    }

    async fn runtime() -> RuntimeState {
        RuntimeState::new(
            Instant::now(),
            AliasStore::load(PathBuf::from("/nonexistent/lavis-updates-aliases.json"))
                .await
                .unwrap(),
            SettingsStore::load(PathBuf::from("/nonexistent/lavis-updates-settings.json"))
                .await
                .unwrap(),
            PathBuf::from("/nonexistent/lavis-updates-fastfetch.json"),
        )
    }

    #[tokio::test]
    async fn routes_outgoing_false_messages_authored_by_self() {
        let outgoing = false;
        let authored_by_self = true;

        assert!(!outgoing);
        assert_eq!(
            route(authored_by_self, ",ping", &runtime().await),
            Some(Action::Ping)
        );
    }

    #[tokio::test]
    async fn rejects_outgoing_true_messages_not_authored_by_self() {
        let outgoing = true;
        let authored_by_self = false;

        assert!(outgoing);
        assert_eq!(route(authored_by_self, ",ping", &runtime().await), None);
    }

    #[tokio::test]
    async fn ignores_self_authored_normal_unknown_and_dot_prefixed_text() {
        let runtime = runtime().await;
        assert_eq!(route(true, "ordinary outgoing text", &runtime), None);
        assert_eq!(route(true, ",unknown", &runtime), None);
        assert_eq!(route(true, ".ping", &runtime), None);
    }

    #[tokio::test]
    async fn routes_edited_style_text_with_the_active_prefix() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-prefix-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
        let mut settings = SettingsStore::load(directory.join("settings.json"))
            .await
            .unwrap();
        settings.set_prefix(".".to_owned()).await.unwrap();
        let runtime = RuntimeState::new(
            Instant::now(),
            aliases,
            settings,
            directory.join("fastfetch.json"),
        );
        assert_eq!(
            route(true, ".help", &runtime),
            Some(Action::Help(crate::commands::HelpRequest::Overview))
        );
        assert_eq!(route(true, ",help", &runtime), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn routes_modules_aliases_and_a_new_prefix_in_the_same_runtime() {
        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-routing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
        aliases
            .add(
                "mods",
                Alias {
                    target: "modules".to_owned(),
                    args: Vec::new(),
                },
            )
            .await
            .unwrap();
        let settings = SettingsStore::load(directory.join("settings.json"))
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(
            Instant::now(),
            aliases,
            settings,
            directory.join("fastfetch.json"),
        );

        assert_eq!(
            route(true, ",modules", &runtime),
            Some(Action::Modules(crate::commands::ModulesRequest::Overview))
        );
        assert_eq!(
            route(true, ",mods", &runtime),
            Some(Action::Modules(crate::commands::ModulesRequest::Overview))
        );
        runtime
            .execute_prefix(&PrefixRequest::Set(".".to_owned()))
            .await;
        assert_eq!(
            route(true, ".modules", &runtime),
            Some(Action::Modules(crate::commands::ModulesRequest::Overview))
        );
        assert_eq!(route(true, ",modules", &runtime), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn routes_builtins_externals_defaults_and_aliases_in_priority_order() {
        fn descriptor(id: &str, default_command: Option<&str>) -> ExternalModuleDescriptor {
            ExternalModuleDescriptor {
                protocol_version: 3,
                id: id.to_owned(),
                display_name: id.to_owned(),
                version: "test".to_owned(),
                author: "test".to_owned(),
                entrypoint: PathBuf::new(),
                module_dir: PathBuf::new(),
                capabilities: vec![],
                default_command: default_command.map(str::to_owned),
                subscriptions: vec![],
                actions: vec![],
                commands: vec![ExternalCommandDescriptor {
                    name: "run".to_owned(),
                    summary_ru: "test".to_owned(),
                    description_ru: "test".to_owned(),
                    usage: "".to_owned(),
                    examples: vec![],
                }],
            }
        }

        let directory = std::env::temp_dir().join(format!(
            "lavis-updates-routing-priority-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let mut aliases = AliasStore::load(directory.join("aliases.json"))
            .await
            .unwrap();
        for name in ["shortcut", "inactive", "crashed", "withoutdefault"] {
            aliases
                .add(
                    name,
                    Alias {
                        target: "ping".to_owned(),
                        args: vec![],
                    },
                )
                .await
                .unwrap();
        }
        let settings = SettingsStore::load(directory.join("settings.json"))
            .await
            .unwrap();
        let mut runtime = RuntimeState::new(
            Instant::now(),
            aliases,
            settings,
            directory.join("fastfetch.json"),
        );
        runtime.set_external_snapshot_for_tests(ExternalRuntimeSnapshot {
            active_commands: ["external.run".to_owned()].into(),
            active_defaults: HashMap::from([
                ("default".to_owned(), "run".to_owned()),
                ("ping".to_owned(), "run".to_owned()),
            ]),
            descriptors: vec![
                descriptor("external", None),
                descriptor("default", Some("run")),
                descriptor("ping", Some("run")),
                descriptor("inactive", Some("run")),
                descriptor("crashed", Some("run")),
                descriptor("withoutdefault", None),
            ],
            ..ExternalRuntimeSnapshot::new()
        });

        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert_eq!(
            route(true, ",external.run args", &runtime),
            Some(Action::External(ExternalInvocation {
                module_id: "external".to_owned(),
                command_name: "run".to_owned(),
                arguments: "args".to_owned(),
                argument_entities: vec![],
            }))
        );
        assert_eq!(
            route(true, ",default args", &runtime),
            Some(Action::External(ExternalInvocation {
                module_id: "default".to_owned(),
                command_name: "run".to_owned(),
                arguments: "args".to_owned(),
                argument_entities: vec![],
            }))
        );
        assert_eq!(route(true, ",shortcut", &runtime), Some(Action::Ping));
        assert_eq!(route(true, ",inactive", &runtime), None);
        assert_eq!(route(true, ",crashed", &runtime), None);
        assert_eq!(route(true, ",withoutdefault", &runtime), None);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn only_suppresses_the_exact_expected_edit_in_its_peer() {
        let mut runtime = runtime().await;
        let first_peer = PeerId::user(1).unwrap();
        let second_peer = PeerId::user(2).unwrap();
        runtime.register_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms".to_owned());

        assert!(!runtime.consume_expected_self_edit(first_peer, 7, ",ping"));
        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert!(!runtime.consume_expected_self_edit(second_peer, 7, "🏓 Pong: 1 ms"));
        assert_eq!(route(true, ",ping", &runtime), Some(Action::Ping));
        assert!(runtime.consume_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms"));
        assert!(!runtime.consume_expected_self_edit(first_peer, 7, "🏓 Pong: 1 ms"));
    }

    #[test]
    fn accepts_concrete_self_sender_for_saved_messages() {
        let self_user_id = PeerId::user(1).unwrap();

        assert!(is_self_authored(Some(self_user_id), false, self_user_id));
    }

    #[test]
    fn accepts_self_sender_sentinel_only_for_outgoing_messages() {
        let self_user_id = PeerId::user(1).unwrap();

        assert!(is_self_authored(
            Some(PeerId::self_user()),
            true,
            self_user_id
        ));
    }

    #[test]
    fn rejects_other_user_sender() {
        let self_user_id = PeerId::user(1).unwrap();
        let other_user_id = PeerId::user(2).unwrap();

        assert!(!is_self_authored(Some(other_user_id), true, self_user_id));
    }

    #[test]
    fn rejects_outgoing_channel_sender() {
        let self_user_id = PeerId::user(1).unwrap();
        let channel_id = PeerId::channel(1).unwrap();

        assert!(!is_self_authored(Some(channel_id), true, self_user_id));
    }

    #[test]
    fn rejects_missing_sender() {
        let self_user_id = PeerId::user(1).unwrap();

        assert!(!is_self_authored(None, true, self_user_id));
    }
}

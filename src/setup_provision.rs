//! Idempotent, transport-independent provisioning of the companion workspace.
//!
//! The raw MTProto adapter belongs at this narrow boundary.  The state machine
//! deliberately works with small owned identifiers so it can be tested without
//! a Telegram connection and persist progress after every remote operation.

use std::{future::Future, pin::Pin};

use crate::setup_store::PersistedSetupState;

pub const COMPANION_GROUP_TITLE: &str = "Lavis";
pub const COMPANION_TOPIC_TITLES: [&str; 3] = ["General", "Logs", "Backups"];
pub const COMPANION_FOLDER_TITLE: &str = "Lavis";

pub type ProvisionFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProvisionError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForumGroup {
    pub id: i64,
    pub access_hash: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForumTopic {
    pub id: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BotIdentity {
    pub id: i64,
    pub access_hash: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdminRights {
    pub manage_topics: bool,
    pub delete_messages: bool,
    pub pin_messages: bool,
}

impl AdminRights {
    pub const MINIMUM: Self = Self {
        manage_topics: true,
        delete_messages: true,
        pin_messages: true,
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DialogFolder {
    pub id: i32,
    pub title: String,
    /// Only regular filters can be owned and repaired by Lavis. Shared
    /// chatlists are never rewritten, even when they share the display name.
    pub regular: bool,
    pub included_chat_ids: Vec<i64>,
    pub pinned_chat_ids: Vec<i64>,
}

/// Server-derived dialog filter constraints. This module deliberately does not
/// encode Telegram's current limits or the first valid folder ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FolderCapacity {
    pub maximum: usize,
    pub first_valid_id: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FolderPlan {
    Existing { id: i32, folder: DialogFolder },
    Create { id: i32, folder: DialogFolder },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionError {
    Telegram,
    Storage,
    GroupUnavailable,
    GroupChanged,
    GeneralTopicLookup,
    FolderCapacity,
    FolderNameConflict,
}

/// The only transport surface the staged provisioner needs. The production
/// implementation must map these calls directly to CreateChannel,
/// Get/CreateForumTopic, InviteToChannel/EditAdmin, dialog-filter calls, and
/// GetAppConfig; raw peers stay out of the state machine.
pub trait ProvisionTransport: Send + Sync {
    fn save_state<'a>(&'a self, state: &'a PersistedSetupState) -> ProvisionFuture<'a, ()>;
    fn resolve_bot<'a>(&'a self, username: &'a str) -> ProvisionFuture<'a, BotIdentity>;
    fn start_bot<'a>(&'a self, bot: BotIdentity) -> ProvisionFuture<'a, ()>;
    fn get_app_config<'a>(&'a self) -> ProvisionFuture<'a, FolderCapacity>;
    fn get_forum_group<'a>(&'a self, group: ForumGroup) -> ProvisionFuture<'a, Option<ForumGroup>>;
    fn create_forum_group<'a>(&'a self, title: &'a str) -> ProvisionFuture<'a, ForumGroup>;
    fn get_forum_topic<'a>(
        &'a self,
        group: ForumGroup,
        title: &'a str,
    ) -> ProvisionFuture<'a, Option<ForumTopic>>;
    fn create_forum_topic<'a>(
        &'a self,
        group: ForumGroup,
        title: &'a str,
    ) -> ProvisionFuture<'a, ForumTopic>;
    fn invite_to_channel<'a>(
        &'a self,
        group: ForumGroup,
        bot_username: &'a str,
    ) -> ProvisionFuture<'a, ()>;
    fn edit_admin<'a>(
        &'a self,
        group: ForumGroup,
        bot_username: &'a str,
        rights: AdminRights,
    ) -> ProvisionFuture<'a, ()>;
    fn get_dialog_filters<'a>(&'a self) -> ProvisionFuture<'a, Vec<DialogFolder>>;
    fn update_dialog_filter<'a>(&'a self, folder: DialogFolder) -> ProvisionFuture<'a, ()>;
    fn update_dialog_filters_order<'a>(&'a self, order: Vec<i32>) -> ProvisionFuture<'a, ()>;
}

/// Returns a deterministic folder action without mutating Telegram state.
pub fn plan_folder(
    filters: &[DialogFolder],
    companion_chat_id: i64,
    capacity: FolderCapacity,
    recorded_id: Option<i32>,
) -> Result<FolderPlan, ProvisionError> {
    if let Some(folder) = filters
        .iter()
        .find(|folder| folder.title == COMPANION_FOLDER_TITLE)
    {
        if recorded_id != Some(folder.id) || !folder.regular {
            return Err(ProvisionError::FolderNameConflict);
        }
        return Ok(FolderPlan::Existing {
            id: folder.id,
            folder: folder.clone(),
        });
    }
    if filters
        .iter()
        .any(|folder| folder.included_chat_ids.contains(&companion_chat_id))
    {
        return Err(ProvisionError::FolderNameConflict);
    }
    if filters.len() >= capacity.maximum {
        return Err(ProvisionError::FolderCapacity);
    }
    let id = (capacity.first_valid_id..)
        .find(|id| filters.iter().all(|folder| folder.id != *id))
        .ok_or(ProvisionError::FolderCapacity)?;
    Ok(FolderPlan::Create {
        id,
        folder: DialogFolder {
            id,
            title: COMPANION_FOLDER_TITLE.to_owned(),
            regular: true,
            included_chat_ids: vec![companion_chat_id],
            pinned_chat_ids: Vec::new(),
        },
    })
}

pub async fn provision(
    transport: &impl ProvisionTransport,
    state: &mut PersistedSetupState,
    bot_username: &str,
) -> Result<(), ProvisionError> {
    let bot = if let (Some(id), Some(access_hash)) = (
        state.identities.bot_user_id,
        state.identities.bot_access_hash,
    ) {
        BotIdentity { id, access_hash }
    } else {
        let bot = transport.resolve_bot(bot_username).await?;
        state.identities.bot_username = Some(bot_username.to_owned());
        state.identities.bot_user_id = Some(bot.id);
        state.identities.bot_access_hash = Some(bot.access_hash);
        persist(transport, state).await?;
        bot
    };
    if !state.stages.bot_dialog_initialized {
        transport.start_bot(bot).await?;
        state.stages.bot_dialog_initialized = true;
        persist(transport, state).await?;
    }
    if !state.stages.app_config_checked {
        transport.get_app_config().await?;
        state.stages.app_config_checked = true;
        persist(transport, state).await?;
    }

    let recorded_group = state.identities.companion_chat_id.map(|id| ForumGroup {
        id,
        access_hash: state.identities.companion_chat_access_hash,
    });
    let group = match recorded_group {
        Some(group) => transport
            .get_forum_group(group)
            .await?
            .ok_or(ProvisionError::GroupUnavailable)?,
        None => {
            let group = transport.create_forum_group(COMPANION_GROUP_TITLE).await?;
            state.identities.companion_chat_id = Some(group.id);
            state.identities.companion_chat_access_hash = group.access_hash;
            state.stages.forum_group_created = true;
            persist(transport, state).await?;
            group
        }
    };

    if !state.stages.forum_group_created {
        state.stages.forum_group_created = true;
        persist(transport, state).await?;
    }
    {
        // General is kept as the durable topic identity. The remaining fixed
        // topics are nevertheless always checked before this stage is saved.
        let mut general = None;
        let mut logs = None;
        let mut backups = None;
        for title in COMPANION_TOPIC_TITLES {
            let topic = match transport.get_forum_topic(group, title).await? {
                Some(topic) => topic,
                None if title == "General" => return Err(ProvisionError::GeneralTopicLookup),
                None => transport.create_forum_topic(group, title).await?,
            };
            if title == "General" {
                general = Some(topic.id);
            } else if title == "Logs" {
                logs = Some(topic.id);
            } else {
                backups = Some(topic.id);
            }
        }
        state.identities.companion_topic_id = general;
        state.identities.companion_logs_topic_id = logs;
        state.identities.companion_backups_topic_id = backups;
        state.stages.forum_topic_created = true;
        persist(transport, state).await?;
    }
    transport.invite_to_channel(group, bot_username).await?;
    state.stages.bot_invited = true;
    persist(transport, state).await?;
    transport
        .edit_admin(group, bot_username, AdminRights::MINIMUM)
        .await?;
    state.stages.bot_rights_configured = true;
    persist(transport, state).await?;
    {
        let folder_capacity = transport.get_app_config().await?;
        let filters = transport.get_dialog_filters().await?;
        let plan = match plan_folder(
            &filters,
            group.id,
            folder_capacity,
            state.identities.companion_folder_id,
        ) {
            Ok(plan) => plan,
            Err(ProvisionError::FolderCapacity) => {
                state.status = "completed_without_folder_capacity".to_owned();
                state.stages.companion_configured = true;
                persist(transport, state).await?;
                return Ok(());
            }
            Err(ProvisionError::FolderNameConflict) => {
                state.status = "completed_without_folder_name_conflict".to_owned();
                state.stages.companion_configured = true;
                persist(transport, state).await?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let (folder_id, mut folder) = match plan {
            FolderPlan::Existing { id, folder } | FolderPlan::Create { id, folder } => (id, folder),
        };
        // This is a dedicated operational folder, not a broad query. Keep its
        // complete selection deterministic so repairs also remove stale peers.
        folder.included_chat_ids = vec![group.id, bot.id];
        folder.pinned_chat_ids = vec![group.id, bot.id];
        transport.update_dialog_filter(folder).await?;
        let mut order = filters
            .iter()
            .map(|folder| folder.id)
            .filter(|id| *id != folder_id)
            .collect::<Vec<_>>();
        order.push(folder_id);
        transport.update_dialog_filters_order(order).await?;
        let verified = transport
            .get_dialog_filters()
            .await?
            .into_iter()
            .any(|folder| {
                folder.id == folder_id
                    && folder.title == COMPANION_FOLDER_TITLE
                    && folder.included_chat_ids == [group.id, bot.id]
                    && folder.pinned_chat_ids == [group.id, bot.id]
            });
        if !verified {
            return Err(ProvisionError::Telegram);
        }
        state.identities.companion_folder_id = Some(folder_id);
        state.stages.folder_configured = true;
        state.stages.companion_configured = true;
        state.status = "companion_configured".to_owned();
        persist(transport, state).await?;
    }
    Ok(())
}

async fn persist(
    transport: &impl ProvisionTransport,
    state: &PersistedSetupState,
) -> Result<(), ProvisionError> {
    transport.save_state(state).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Mock {
        calls: Mutex<Vec<String>>,
        filters: Mutex<Vec<DialogFolder>>,
        topic: Option<ForumTopic>,
    }
    impl Mock {
        fn call(&self, call: &str) {
            self.calls.lock().unwrap().push(call.into());
        }
    }
    impl ProvisionTransport for Mock {
        fn save_state<'a>(&'a self, _: &'a PersistedSetupState) -> ProvisionFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn resolve_bot<'a>(&'a self, _: &'a str) -> ProvisionFuture<'a, BotIdentity> {
            Box::pin(async {
                Ok(BotIdentity {
                    id: 99,
                    access_hash: 1,
                })
            })
        }
        fn start_bot<'a>(&'a self, _: BotIdentity) -> ProvisionFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn get_app_config<'a>(&'a self) -> ProvisionFuture<'a, FolderCapacity> {
            Box::pin(async move {
                self.call("config");
                Ok(FolderCapacity {
                    maximum: 100,
                    first_valid_id: 1000,
                })
            })
        }
        fn get_forum_group<'a>(
            &'a self,
            group: ForumGroup,
        ) -> ProvisionFuture<'a, Option<ForumGroup>> {
            Box::pin(async move { Ok(Some(group)) })
        }
        fn create_forum_group<'a>(&'a self, _: &'a str) -> ProvisionFuture<'a, ForumGroup> {
            Box::pin(async move {
                self.call("group");
                Ok(ForumGroup {
                    id: 42,
                    access_hash: Some(1),
                })
            })
        }
        fn get_forum_topic<'a>(
            &'a self,
            _: ForumGroup,
            title: &'a str,
        ) -> ProvisionFuture<'a, Option<ForumTopic>> {
            Box::pin(async move {
                self.call("get-topic");
                Ok(if title == "General" {
                    Some(ForumTopic { id: 1 })
                } else {
                    self.topic
                })
            })
        }
        fn create_forum_topic<'a>(
            &'a self,
            _: ForumGroup,
            _: &'a str,
        ) -> ProvisionFuture<'a, ForumTopic> {
            Box::pin(async move {
                self.call("topic");
                Ok(ForumTopic { id: 7 })
            })
        }
        fn invite_to_channel<'a>(&'a self, _: ForumGroup, _: &'a str) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                self.call("invite");
                Ok(())
            })
        }
        fn edit_admin<'a>(
            &'a self,
            _: ForumGroup,
            _: &'a str,
            rights: AdminRights,
        ) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(rights, AdminRights::MINIMUM);
                self.call("rights");
                Ok(())
            })
        }
        fn get_dialog_filters<'a>(&'a self) -> ProvisionFuture<'a, Vec<DialogFolder>> {
            Box::pin(async move {
                self.call("filters");
                Ok(self.filters.lock().unwrap().clone())
            })
        }
        fn update_dialog_filter<'a>(&'a self, folder: DialogFolder) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(folder.included_chat_ids, vec![42, 99]);
                assert_eq!(folder.pinned_chat_ids, vec![42, 99]);
                self.filters
                    .lock()
                    .unwrap()
                    .retain(|current| current.id != folder.id);
                self.filters.lock().unwrap().push(folder);
                self.call("filter");
                Ok(())
            })
        }
        fn update_dialog_filters_order<'a>(&'a self, order: Vec<i32>) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(order, vec![1000]);
                self.call("order");
                Ok(())
            })
        }
    }
    #[tokio::test]
    async fn progresses_idempotently_and_persists_each_stage() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        assert_eq!(
            *transport.calls.lock().unwrap(),
            [
                "config",
                "group",
                "get-topic",
                "get-topic",
                "topic",
                "get-topic",
                "topic",
                "invite",
                "rights",
                "config",
                "filters",
                "filter",
                "order",
                "filters"
            ]
        );
        transport.calls.lock().unwrap().clear();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let repair_calls = transport.calls.lock().unwrap().clone();
        assert!(repair_calls.contains(&"get-topic".to_owned()));
        assert!(repair_calls.contains(&"filters".to_owned()));
    }

    #[test]
    fn folder_capacity_conflict_and_order_are_deterministic() {
        let group = 42;
        let filters = vec![DialogFolder {
            id: 4,
            title: "Other".into(),
            regular: true,
            included_chat_ids: vec![group],
            pinned_chat_ids: vec![],
        }];
        assert_eq!(
            plan_folder(
                &filters,
                group,
                FolderCapacity {
                    maximum: 10,
                    first_valid_id: 50,
                },
                None,
            ),
            Err(ProvisionError::FolderNameConflict)
        );
        let full = [50, 51]
            .into_iter()
            .map(|id| DialogFolder {
                id,
                title: id.to_string(),
                regular: true,
                included_chat_ids: vec![],
                pinned_chat_ids: vec![],
            })
            .collect::<Vec<_>>();
        assert_eq!(
            plan_folder(
                &full,
                group,
                FolderCapacity {
                    maximum: full.len(),
                    first_valid_id: 50,
                },
                None,
            ),
            Err(ProvisionError::FolderCapacity)
        );
        let plan = plan_folder(
            &[DialogFolder {
                id: 2,
                title: "Other".into(),
                regular: true,
                included_chat_ids: vec![],
                pinned_chat_ids: vec![],
            }],
            group,
            FolderCapacity {
                maximum: 100,
                first_valid_id: 50,
            },
            None,
        )
        .unwrap();
        assert!(matches!(plan, FolderPlan::Create { id: 50, .. }));

        let named_chatlist = DialogFolder {
            id: 9,
            title: COMPANION_FOLDER_TITLE.to_owned(),
            regular: false,
            included_chat_ids: vec![],
            pinned_chat_ids: vec![],
        };
        assert_eq!(
            plan_folder(
                &[named_chatlist],
                group,
                FolderCapacity {
                    maximum: 10,
                    first_valid_id: 2
                },
                Some(9),
            ),
            Err(ProvisionError::FolderNameConflict)
        );
    }
}

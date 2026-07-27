//! Idempotent, transport-independent provisioning of the companion workspace.
//!
//! The raw MTProto adapter belongs at this narrow boundary.  The state machine
//! deliberately works with small owned identifiers so it can be tested without
//! a Telegram connection and persist progress after every remote operation.

use std::{future::Future, pin::Pin};

use crate::setup_store::{PersistedSetupState, SetupStore};

pub const COMPANION_GROUP_TITLE: &str = "Lavis";
pub const COMPANION_TOPIC_TITLES: [&str; 3] = ["General", "Logs", "Backups"];
pub const COMPANION_FOLDER_TITLE: &str = "Lavis";

pub type ProvisionFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ProvisionError>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForumGroup {
    pub id: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForumTopic {
    pub id: i32,
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
    pub included_chat_ids: Vec<i64>,
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
    FolderCapacity,
    FolderConflict,
}

/// The only transport surface the staged provisioner needs. The production
/// implementation must map these calls directly to CreateChannel,
/// Get/CreateForumTopic, InviteToChannel/EditAdmin, dialog-filter calls, and
/// GetAppConfig; raw peers stay out of the state machine.
pub trait ProvisionTransport: Send + Sync {
    fn get_app_config<'a>(&'a self) -> ProvisionFuture<'a, ()>;
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

pub trait ProvisionStateStore {
    fn save(&mut self, state: &PersistedSetupState) -> Result<(), ProvisionError>;
}

impl ProvisionStateStore for SetupStore {
    fn save(&mut self, state: &PersistedSetupState) -> Result<(), ProvisionError> {
        self.save_state(state).map_err(|_| ProvisionError::Storage)
    }
}

/// Returns a deterministic folder action without mutating Telegram state.
pub fn plan_folder(
    filters: &[DialogFolder],
    companion_chat_id: i64,
    capacity: FolderCapacity,
) -> Result<FolderPlan, ProvisionError> {
    if let Some(folder) = filters
        .iter()
        .find(|folder| folder.included_chat_ids.contains(&companion_chat_id))
    {
        if folder.title != COMPANION_FOLDER_TITLE {
            return Err(ProvisionError::FolderConflict);
        }
        return Ok(FolderPlan::Existing {
            id: folder.id,
            folder: folder.clone(),
        });
    }
    if let Some(folder) = filters
        .iter()
        .find(|folder| folder.title == COMPANION_FOLDER_TITLE)
    {
        let mut folder = folder.clone();
        if !folder.included_chat_ids.contains(&companion_chat_id) {
            folder.included_chat_ids.push(companion_chat_id);
        }
        return Ok(FolderPlan::Existing {
            id: folder.id,
            folder,
        });
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
            included_chat_ids: vec![companion_chat_id],
        },
    })
}

pub async fn provision(
    transport: &impl ProvisionTransport,
    store: &mut impl ProvisionStateStore,
    state: &mut PersistedSetupState,
    bot_username: &str,
    folder_capacity: FolderCapacity,
) -> Result<(), ProvisionError> {
    if !state.stages.app_config_checked {
        transport.get_app_config().await?;
        state.stages.app_config_checked = true;
        persist(store, state)?;
    }

    let group = match state.identities.companion_chat_id {
        Some(id) => ForumGroup { id },
        None => {
            let group = transport.create_forum_group(COMPANION_GROUP_TITLE).await?;
            state.identities.companion_chat_id = Some(group.id);
            state.stages.forum_group_created = true;
            persist(store, state)?;
            group
        }
    };

    if !state.stages.forum_group_created {
        state.stages.forum_group_created = true;
        persist(store, state)?;
    }
    if state.identities.companion_topic_id.is_none()
        || state.identities.companion_logs_topic_id.is_none()
        || state.identities.companion_backups_topic_id.is_none()
    {
        // General is kept as the durable topic identity. The remaining fixed
        // topics are nevertheless always checked before this stage is saved.
        let mut general = None;
        let mut logs = None;
        let mut backups = None;
        for title in COMPANION_TOPIC_TITLES {
            let topic = match transport.get_forum_topic(group, title).await? {
                Some(topic) => topic,
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
        persist(store, state)?;
    }
    if !state.stages.bot_invited {
        transport.invite_to_channel(group, bot_username).await?;
        state.stages.bot_invited = true;
        persist(store, state)?;
    }
    if !state.stages.bot_rights_configured {
        transport
            .edit_admin(group, bot_username, AdminRights::MINIMUM)
            .await?;
        state.stages.bot_rights_configured = true;
        persist(store, state)?;
    }
    if !state.stages.folder_configured {
        let filters = transport.get_dialog_filters().await?;
        let plan = match plan_folder(&filters, group.id, folder_capacity) {
            Ok(plan) => plan,
            Err(ProvisionError::FolderCapacity | ProvisionError::FolderConflict) => {
                state.status = "completed_without_folder".to_owned();
                state.stages.companion_configured = true;
                persist(store, state)?;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let (folder_id, folder) = match plan {
            FolderPlan::Existing { id, folder } | FolderPlan::Create { id, folder } => (id, folder),
        };
        transport.update_dialog_filter(folder).await?;
        let mut order = filters
            .iter()
            .map(|folder| folder.id)
            .filter(|id| *id != folder_id)
            .collect::<Vec<_>>();
        order.push(folder_id);
        transport.update_dialog_filters_order(order).await?;
        state.identities.companion_folder_id = Some(folder_id);
        state.stages.folder_configured = true;
        state.stages.companion_configured = true;
        state.status = "companion_configured".to_owned();
        persist(store, state)?;
    }
    Ok(())
}

fn persist(
    store: &mut impl ProvisionStateStore,
    state: &PersistedSetupState,
) -> Result<(), ProvisionError> {
    store.save(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Mock {
        calls: Mutex<Vec<String>>,
        filters: Vec<DialogFolder>,
        topic: Option<ForumTopic>,
    }
    impl Mock {
        fn call(&self, call: &str) {
            self.calls.lock().unwrap().push(call.into());
        }
    }
    impl ProvisionTransport for Mock {
        fn get_app_config<'a>(&'a self) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                self.call("config");
                Ok(())
            })
        }
        fn create_forum_group<'a>(&'a self, _: &'a str) -> ProvisionFuture<'a, ForumGroup> {
            Box::pin(async move {
                self.call("group");
                Ok(ForumGroup { id: 42 })
            })
        }
        fn get_forum_topic<'a>(
            &'a self,
            _: ForumGroup,
            _: &'a str,
        ) -> ProvisionFuture<'a, Option<ForumTopic>> {
            Box::pin(async move {
                self.call("get-topic");
                Ok(self.topic)
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
                Ok(self.filters.clone())
            })
        }
        fn update_dialog_filter<'a>(&'a self, folder: DialogFolder) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(folder.included_chat_ids, vec![42]);
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
    #[derive(Default)]
    struct Store(Vec<PersistedSetupState>);
    impl ProvisionStateStore for Store {
        fn save(&mut self, state: &PersistedSetupState) -> Result<(), ProvisionError> {
            self.0.push(state.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn progresses_idempotently_and_persists_each_stage() {
        let transport = Mock::default();
        let mut store = Store::default();
        let mut state = PersistedSetupState::default();
        provision(
            &transport,
            &mut store,
            &mut state,
            "lavis_test_bot",
            FolderCapacity {
                maximum: 100,
                first_valid_id: 1000,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            *transport.calls.lock().unwrap(),
            [
                "config",
                "group",
                "get-topic",
                "topic",
                "get-topic",
                "topic",
                "get-topic",
                "topic",
                "invite",
                "rights",
                "filters",
                "filter",
                "order"
            ]
        );
        assert_eq!(store.0.len(), 6);
        transport.calls.lock().unwrap().clear();
        provision(
            &transport,
            &mut store,
            &mut state,
            "lavis_test_bot",
            FolderCapacity {
                maximum: 100,
                first_valid_id: 1000,
            },
        )
        .await
        .unwrap();
        assert!(transport.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn folder_capacity_conflict_and_order_are_deterministic() {
        let group = 42;
        let filters = vec![DialogFolder {
            id: 4,
            title: "Other".into(),
            included_chat_ids: vec![group],
        }];
        assert_eq!(
            plan_folder(
                &filters,
                group,
                FolderCapacity {
                    maximum: 10,
                    first_valid_id: 50,
                },
            ),
            Err(ProvisionError::FolderConflict)
        );
        let full = [50, 51]
            .into_iter()
            .map(|id| DialogFolder {
                id,
                title: id.to_string(),
                included_chat_ids: vec![],
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
            ),
            Err(ProvisionError::FolderCapacity)
        );
        let plan = plan_folder(
            &[DialogFolder {
                id: 2,
                title: "Other".into(),
                included_chat_ids: vec![],
            }],
            group,
            FolderCapacity {
                maximum: 100,
                first_valid_id: 50,
            },
        )
        .unwrap();
        assert!(matches!(plan, FolderPlan::Create { id: 50, .. }));
    }
}

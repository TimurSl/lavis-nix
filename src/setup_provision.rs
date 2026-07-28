//! Idempotent, transport-independent provisioning of the companion workspace.
//!
//! The raw MTProto adapter belongs at this narrow boundary.  The state machine
//! deliberately works with small owned identifiers so it can be tested without
//! a Telegram connection and persist progress after every remote operation.

use std::{collections::BTreeSet, future::Future, pin::Pin};

use crate::setup_store::PersistedSetupState;

pub const COMPANION_GROUP_TITLE: &str = "Lavis";
pub const COMPANION_TOPIC_TITLES: [&str; 3] = ["General", "Logs", "Backups"];
pub const COMPANION_FOLDER_TITLE: &str = "Lavis";
pub const COMMUNITY_USERNAME: &str = "lavis_userbot";

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
pub struct CommunityIdentity {
    pub id: i64,
    pub access_hash: i64,
    pub public: bool,
    pub megagroup: bool,
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
    ResolveBot,
    StartBot,
    AppConfig,
    CreateGroup,
    Storage,
    GroupUnavailable,
    GroupChanged,
    GeneralTopicLookup,
    CreateTopic,
    InviteBot,
    PromoteBot,
    DialogFilters,
    FolderCapacity,
    FolderNameConflict,
    CommunityResolve,
    CommunityInvalidPeer,
    CommunityJoin,
    CommunityUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletedWithoutFolder {
    Capacity,
    NameOrOwnershipConflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionResult {
    Completed,
    CompletedWithoutFolder(CompletedWithoutFolder),
    CompletedWithoutCommunity(ProvisionError),
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
    fn resolve_community<'a>(&'a self, username: &'a str)
    -> ProvisionFuture<'a, CommunityIdentity>;
    fn get_community<'a>(
        &'a self,
        community: CommunityIdentity,
    ) -> ProvisionFuture<'a, Option<CommunityIdentity>>;
    fn join_community<'a>(&'a self, community: CommunityIdentity) -> ProvisionFuture<'a, ()>;
    fn get_dialog_filters<'a>(&'a self) -> ProvisionFuture<'a, Vec<DialogFolder>>;
    fn update_dialog_filter<'a>(&'a self, folder: DialogFolder) -> ProvisionFuture<'a, ()>;
    fn update_dialog_filters_order<'a>(&'a self, order: Vec<i32>) -> ProvisionFuture<'a, ()>;
}

/// Returns a deterministic folder action without mutating Telegram state.
pub fn plan_folder(
    filters: &[DialogFolder],
    companion_chat_id: i64,
    companion_bot_id: i64,
    capacity: FolderCapacity,
    recorded_id: Option<i32>,
) -> Result<FolderPlan, ProvisionError> {
    let named = filters
        .iter()
        .filter(|folder| folder.title == COMPANION_FOLDER_TITLE)
        .collect::<Vec<_>>();
    if let Some(recorded_id) = recorded_id {
        if let Some(folder) = filters.iter().find(|folder| folder.id == recorded_id) {
            if !folder.regular
                || folder.title != COMPANION_FOLDER_TITLE
                || named.iter().any(|candidate| candidate.id != recorded_id)
            {
                return Err(ProvisionError::FolderNameConflict);
            }
            return Ok(FolderPlan::Existing {
                id: folder.id,
                folder: folder.clone(),
            });
        }
        if !named.is_empty() || filters.len() >= capacity.maximum {
            return Err(ProvisionError::FolderNameConflict);
        }
        return Ok(FolderPlan::Create {
            id: recorded_id,
            folder: DialogFolder {
                id: recorded_id,
                title: COMPANION_FOLDER_TITLE.to_owned(),
                regular: true,
                included_chat_ids: Vec::new(),
                pinned_chat_ids: Vec::new(),
            },
        });
    }
    if named.iter().any(|folder| !folder.regular) {
        return Err(ProvisionError::FolderNameConflict);
    }
    let candidates = named
        .into_iter()
        .filter(|folder| {
            folder.included_chat_ids.contains(&companion_chat_id)
                && folder.included_chat_ids.contains(&companion_bot_id)
        })
        .collect::<Vec<_>>();
    if candidates.len() == 1 {
        let folder = candidates[0];
        return Ok(FolderPlan::Existing {
            id: folder.id,
            folder: folder.clone(),
        });
    }
    if !candidates.is_empty()
        || filters
            .iter()
            .any(|folder| folder.title == COMPANION_FOLDER_TITLE)
    {
        return Err(ProvisionError::FolderNameConflict);
    }
    if filters.iter().any(|folder| {
        folder.included_chat_ids.contains(&companion_chat_id)
            || folder.included_chat_ids.contains(&companion_bot_id)
    }) {
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

fn normalized_peer_set(peers: &[i64]) -> Option<BTreeSet<i64>> {
    let set = peers.iter().copied().collect::<BTreeSet<_>>();
    (set.len() == peers.len()).then_some(set)
}

fn contains_expected_peers(actual: &[i64], expected: &[i64]) -> bool {
    let Some(actual) = normalized_peer_set(actual) else {
        return false;
    };
    let Some(expected) = normalized_peer_set(expected) else {
        return false;
    };
    expected.is_subset(&actual)
}

pub async fn provision(
    transport: &impl ProvisionTransport,
    state: &mut PersistedSetupState,
    bot_username: &str,
) -> Result<ProvisionResult, ProvisionError> {
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
    let community_result = onboard_community(transport, state).await;
    {
        let folder_capacity = transport.get_app_config().await?;
        let filters = transport.get_dialog_filters().await?;
        let plan = match plan_folder(
            &filters,
            group.id,
            bot.id,
            folder_capacity,
            state.identities.companion_folder_id,
        ) {
            Ok(plan) => plan,
            Err(ProvisionError::FolderCapacity) => {
                state.status = "completed_without_folder_capacity".to_owned();
                state.stages.companion_configured = true;
                persist(transport, state).await?;
                return Ok(ProvisionResult::CompletedWithoutFolder(
                    CompletedWithoutFolder::Capacity,
                ));
            }
            Err(ProvisionError::FolderNameConflict) => {
                state.status = "completed_without_folder_name_conflict".to_owned();
                state.stages.companion_configured = true;
                persist(transport, state).await?;
                return Ok(ProvisionResult::CompletedWithoutFolder(
                    CompletedWithoutFolder::NameOrOwnershipConflict,
                ));
            }
            Err(error) => return Err(error),
        };
        let (folder_id, mut folder) = match plan {
            FolderPlan::Existing { id, folder } | FolderPlan::Create { id, folder } => (id, folder),
        };
        state.identities.companion_folder_id = Some(folder_id);
        state.stages.folder_configured = false;
        persist(transport, state).await?;
        let mut managed = vec![group.id, bot.id];
        if let Some(community_id) = state.identities.community_chat_id {
            managed.push(community_id);
        }
        managed.sort_unstable();
        if normalized_peer_set(&managed).is_none() {
            return Err(ProvisionError::DialogFilters);
        }
        folder.included_chat_ids.extend(managed.iter().copied());
        folder.included_chat_ids.sort_unstable();
        folder.included_chat_ids.dedup();
        folder.pinned_chat_ids.extend(managed.iter().copied());
        folder.pinned_chat_ids.sort_unstable();
        folder.pinned_chat_ids.dedup();
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
                    && folder.regular
                    && contains_expected_peers(&folder.included_chat_ids, &managed)
                    && contains_expected_peers(&folder.pinned_chat_ids, &managed)
            });
        if !verified {
            return Err(ProvisionError::DialogFilters);
        }
        state.stages.folder_configured = true;
        state.stages.companion_configured = true;
        state.status = if community_result.is_ok() {
            "companion_and_community_configured"
        } else {
            "companion_configured_community_pending"
        }
        .to_owned();
        persist(transport, state).await?;
    }
    match community_result {
        Ok(()) => Ok(ProvisionResult::Completed),
        Err(error) => Ok(ProvisionResult::CompletedWithoutCommunity(error)),
    }
}

async fn onboard_community(
    transport: &impl ProvisionTransport,
    state: &mut PersistedSetupState,
) -> Result<(), ProvisionError> {
    let community = match (
        state.identities.community_chat_id,
        state.identities.community_access_hash,
    ) {
        (Some(id), Some(access_hash)) => {
            let recorded = CommunityIdentity {
                id,
                access_hash,
                public: true,
                megagroup: true,
            };
            transport
                .get_community(recorded)
                .await?
                .ok_or(ProvisionError::CommunityUnavailable)?
        }
        _ => {
            let resolved = transport.resolve_community(COMMUNITY_USERNAME).await?;
            if !resolved.public || !resolved.megagroup {
                return Err(ProvisionError::CommunityInvalidPeer);
            }
            state.identities.community_chat_id = Some(resolved.id);
            state.identities.community_access_hash = Some(resolved.access_hash);
            persist(transport, state).await?;
            resolved
        }
    };
    if !community.public || !community.megagroup {
        return Err(ProvisionError::CommunityInvalidPeer);
    }
    if !state.stages.community_joined {
        transport.join_community(community).await?;
        state.stages.community_joined = true;
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
        start_fails: bool,
        community_fails: bool,
        update_failures: Mutex<usize>,
        saved_states: Mutex<Vec<PersistedSetupState>>,
    }
    impl Mock {
        fn call(&self, call: &str) {
            self.calls.lock().unwrap().push(call.into());
        }
    }
    impl ProvisionTransport for Mock {
        fn save_state<'a>(&'a self, state: &'a PersistedSetupState) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                self.saved_states.lock().unwrap().push(state.clone());
                Ok(())
            })
        }
        fn resolve_bot<'a>(&'a self, _: &'a str) -> ProvisionFuture<'a, BotIdentity> {
            Box::pin(async move {
                self.call("resolve");
                Ok(BotIdentity {
                    id: 99,
                    access_hash: 1,
                })
            })
        }
        fn start_bot<'a>(&'a self, _: BotIdentity) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                self.call("start");
                if self.start_fails {
                    return Err(ProvisionError::StartBot);
                }
                Ok(())
            })
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
        fn resolve_community<'a>(
            &'a self,
            username: &'a str,
        ) -> ProvisionFuture<'a, CommunityIdentity> {
            Box::pin(async move {
                assert_eq!(username, COMMUNITY_USERNAME);
                self.call("resolve-community");
                if self.community_fails {
                    return Err(ProvisionError::CommunityResolve);
                }
                Ok(CommunityIdentity {
                    id: 77,
                    access_hash: 2,
                    public: true,
                    megagroup: true,
                })
            })
        }
        fn get_community<'a>(
            &'a self,
            community: CommunityIdentity,
        ) -> ProvisionFuture<'a, Option<CommunityIdentity>> {
            Box::pin(async move {
                self.call("get-community");
                Ok(Some(community))
            })
        }
        fn join_community<'a>(&'a self, community: CommunityIdentity) -> ProvisionFuture<'a, ()> {
            Box::pin(async move {
                assert_eq!(community.id, 77);
                self.call("join-community");
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
                assert!(folder.included_chat_ids.contains(&42));
                assert!(folder.included_chat_ids.contains(&99));
                let mut failures = self.update_failures.lock().unwrap();
                if *failures > 0 {
                    *failures -= 1;
                    return Err(ProvisionError::DialogFilters);
                }
                drop(failures);
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
                assert_eq!(order.len(), 1);
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
                "resolve",
                "start",
                "config",
                "group",
                "get-topic",
                "get-topic",
                "topic",
                "get-topic",
                "topic",
                "invite",
                "rights",
                "resolve-community",
                "join-community",
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
        assert!(!repair_calls.contains(&"resolve".to_owned()));
        assert!(!repair_calls.contains(&"start".to_owned()));
        assert!(repair_calls.contains(&"get-topic".to_owned()));
        assert!(repair_calls.contains(&"filters".to_owned()));
    }

    #[tokio::test]
    async fn start_failure_does_not_stage_bot_dialog_initialization() {
        let transport = Mock {
            start_fails: true,
            ..Mock::default()
        };
        let mut state = PersistedSetupState::default();

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Err(ProvisionError::StartBot)
        );
        assert!(!state.stages.bot_dialog_initialized);
        assert_eq!(*transport.calls.lock().unwrap(), ["resolve", "start"]);
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
                99,
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
                99,
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
            99,
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
                99,
                FolderCapacity {
                    maximum: 10,
                    first_valid_id: 2
                },
                Some(9),
            ),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    fn capacity() -> FolderCapacity {
        FolderCapacity {
            maximum: 100,
            first_valid_id: 2,
        }
    }

    fn folder(id: i32, regular: bool, included: Vec<i64>) -> DialogFolder {
        DialogFolder {
            id,
            title: COMPANION_FOLDER_TITLE.to_owned(),
            regular,
            included_chat_ids: included.clone(),
            pinned_chat_ids: included,
        }
    }

    #[test]
    fn adopts_unique_regular_lavis_folder_with_both_core_peers() {
        assert!(matches!(
            plan_folder(&[folder(7, true, vec![42, 99])], 42, 99, capacity(), None),
            Ok(FolderPlan::Existing { id: 7, .. })
        ));
    }

    #[test]
    fn rejects_title_only_folder_without_both_managed_peers() {
        assert_eq!(
            plan_folder(&[folder(7, true, vec![42])], 42, 99, capacity(), None),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn rejects_shared_chatlist_named_lavis() {
        assert_eq!(
            plan_folder(&[folder(7, false, vec![42, 99])], 42, 99, capacity(), None),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn multiple_adoption_candidates_fail_closed() {
        assert_eq!(
            plan_folder(
                &[folder(7, true, vec![42, 99]), folder(8, true, vec![42, 99]),],
                42,
                99,
                capacity(),
                None
            ),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn conflicting_recorded_folder_id_is_rejected() {
        assert_eq!(
            plan_folder(
                &[folder(7, true, vec![42, 99])],
                42,
                99,
                capacity(),
                Some(8)
            ),
            Err(ProvisionError::FolderNameConflict)
        );
    }

    #[test]
    fn reversed_peer_order_passes_verification() {
        assert!(contains_expected_peers(&[99, 77, 42], &[42, 77, 99]));
    }

    #[test]
    fn missing_expected_peer_fails_verification() {
        assert!(!contains_expected_peers(&[42, 99], &[42, 77, 99]));
    }

    #[test]
    fn duplicate_managed_peer_ids_are_rejected() {
        assert!(!contains_expected_peers(&[42, 99], &[42, 42, 99]));
        assert!(!contains_expected_peers(&[42, 42, 99], &[42, 99]));
    }

    #[tokio::test]
    async fn persisted_folder_id_survives_failure_and_repair_reuses_it() {
        let transport = Mock {
            update_failures: Mutex::new(1),
            ..Mock::default()
        };
        let mut state = PersistedSetupState::default();
        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Err(ProvisionError::DialogFilters)
        );
        assert_eq!(state.identities.companion_folder_id, Some(1000));
        assert!(!state.stages.folder_configured);
        assert!(
            transport
                .saved_states
                .lock()
                .unwrap()
                .iter()
                .any(|saved| saved.identities.companion_folder_id == Some(1000))
        );

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert_eq!(state.identities.companion_folder_id, Some(1000));
        assert_eq!(transport.filters.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn repair_adopts_folder_without_creating_a_second_one() {
        let transport = Mock::default();
        transport
            .filters
            .lock()
            .unwrap()
            .push(folder(12, true, vec![42, 99]));
        let mut state = PersistedSetupState::default();
        state.identities.bot_user_id = Some(99);
        state.identities.bot_access_hash = Some(1);
        state.identities.companion_chat_id = Some(42);
        state.identities.companion_chat_access_hash = Some(1);

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert_eq!(state.identities.companion_folder_id, Some(12));
        assert_eq!(transport.filters.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn authenticated_user_joins_official_community_once() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "join-community")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "resolve-community")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn already_participant_is_successful_and_idempotent() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert!(state.stages.community_joined);
    }

    #[tokio::test]
    async fn companion_bot_is_never_invited_to_official_community() {
        let transport = Mock::default();
        let mut state = PersistedSetupState::default();
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let calls = transport.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "invite")
                .count(),
            1
        );
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.as_str() == "join-community")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn folder_contains_all_managed_peers_and_preserves_user_peers() {
        let transport = Mock::default();
        transport
            .filters
            .lock()
            .unwrap()
            .push(folder(12, true, vec![42, 99, 555]));
        let mut state = PersistedSetupState::default();
        state.identities.bot_user_id = Some(99);
        state.identities.bot_access_hash = Some(1);
        state.identities.companion_chat_id = Some(42);
        state.identities.companion_chat_access_hash = Some(1);
        provision(&transport, &mut state, "lavis_test_bot")
            .await
            .unwrap();
        let filters = transport.filters.lock().unwrap();
        assert_eq!(filters[0].included_chat_ids, [42, 77, 99, 555]);
        assert_eq!(filters[0].pinned_chat_ids, [42, 77, 99, 555]);
    }

    #[tokio::test]
    async fn community_failure_is_partial_and_preserves_core_workspace() {
        let transport = Mock {
            community_fails: true,
            ..Mock::default()
        };
        let mut state = PersistedSetupState::default();
        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::CompletedWithoutCommunity(
                ProvisionError::CommunityResolve
            ))
        );
        assert!(state.stages.companion_configured);
        assert!(state.stages.folder_configured);
        assert!(!state.stages.community_joined);
        assert_eq!(transport.filters.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn pr15_pr16_state_migrates_without_reset() {
        let transport = Mock::default();
        transport
            .filters
            .lock()
            .unwrap()
            .push(folder(12, true, vec![42, 99]));
        let mut state = PersistedSetupState::default();
        state.identities.bot_username = Some("lavis_test_bot".into());
        state.identities.bot_user_id = Some(99);
        state.identities.bot_access_hash = Some(1);
        state.identities.companion_chat_id = Some(42);
        state.identities.companion_chat_access_hash = Some(1);
        state.stages.bot_dialog_initialized = true;
        state.stages.app_config_checked = true;
        state.stages.forum_group_created = true;
        state.stages.forum_topic_created = true;
        state.stages.bot_invited = true;
        state.stages.bot_rights_configured = true;

        assert_eq!(
            provision(&transport, &mut state, "lavis_test_bot").await,
            Ok(ProvisionResult::Completed)
        );
        assert_eq!(state.identities.companion_folder_id, Some(12));
        assert!(state.stages.companion_configured);
    }
}

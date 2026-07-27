//! Raw MTProto companion workspace provisioning.

use std::path::{Path, PathBuf};

use grammers_client::{Client, tl};

use crate::{
    setup_provision::{COMPANION_FOLDER_TITLE, COMPANION_GROUP_TITLE, COMPANION_TOPIC_TITLES},
    setup_store::{PersistedSetupState, SetupStore},
};

/// The public group description is deliberately fixed: it explains the three
/// private operational topics without exposing a bot token or local path.
pub const COMPANION_GROUP_ABOUT: &str = "Lavis companion workspace";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionError {
    ResolveBot,
    StartBot,
    AppConfig,
    CreateGroup,
    CreatedGroupMissing,
    GroupUnavailable,
    ForumTopics,
    CreateTopic,
    InviteBot,
    PromoteBot,
    DialogFilters,
    FolderCapacity,
    FolderConflict,
    Storage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputIdentity {
    id: i64,
    access_hash: i64,
}

/// Provision all resources idempotently. Every successful remote action is
/// recorded before the next action, so a crash can only require repair of the
/// current stage; it never deletes an existing Telegram resource.
pub async fn provision(
    client: &Client,
    state_path: PathBuf,
    token_path: PathBuf,
    bot_username: &str,
) -> Result<(), ProvisionError> {
    let mut state = load_state(state_path.clone(), token_path.clone()).await?;
    let bot = resolve_bot(client, bot_username).await?;
    state.identities.bot_username = Some(bot_username.to_owned());
    state.identities.bot_user_id = Some(bot.id);
    state.identities.bot_access_hash = Some(bot.access_hash);
    persist(&state_path, &token_path, &state).await?;

    if !state.stages.bot_dialog_initialized {
        client
            .invoke(&tl::functions::messages::StartBot {
                bot: input_user(bot),
                peer: input_peer_user(bot),
                random_id: random_id().map_err(|_| ProvisionError::StartBot)?,
                start_param: String::new(),
            })
            .await
            .map_err(|_| ProvisionError::StartBot)?;
        state.stages.bot_dialog_initialized = true;
        persist(&state_path, &token_path, &state).await?;
    }

    if !state.stages.app_config_checked {
        client
            .invoke(&tl::functions::help::GetAppConfig { hash: 0 })
            .await
            .map_err(|_| ProvisionError::AppConfig)?;
        state.stages.app_config_checked = true;
        persist(&state_path, &token_path, &state).await?;
    }

    let group = match group_from_state(&state) {
        Some(group) => {
            let chats = client
                .invoke(&tl::functions::channels::GetChannels {
                    id: vec![input_channel(group)],
                })
                .await
                .map_err(|_| ProvisionError::GroupUnavailable)?;
            let exists = chats.chats().iter().any(|chat| {
                matches!(chat,
                    tl::enums::Chat::Channel(channel)
                        if channel.id == group.id && channel.megagroup && channel.forum
                )
            });
            if !exists {
                return Err(ProvisionError::GroupUnavailable);
            }
            group
        }
        None => {
            let updates = client
                .invoke(&tl::functions::channels::CreateChannel {
                    broadcast: false,
                    megagroup: true,
                    for_import: false,
                    forum: true,
                    title: COMPANION_GROUP_TITLE.to_owned(),
                    about: COMPANION_GROUP_ABOUT.to_owned(),
                    geo_point: None,
                    address: None,
                    ttl_period: None,
                })
                .await
                .map_err(|_| ProvisionError::CreateGroup)?;
            let group = extract_created_channel(&updates, COMPANION_GROUP_TITLE)
                .ok_or(ProvisionError::CreatedGroupMissing)?;
            state.identities.companion_chat_id = Some(group.id);
            state.identities.companion_chat_access_hash = Some(group.access_hash);
            state.stages.forum_group_created = true;
            persist(&state_path, &token_path, &state).await?;
            group
        }
    };

    let peer = input_peer_channel(group);
    let topics = client
        .invoke(&tl::functions::messages::GetForumTopics {
            q: None,
            peer: peer.clone(),
            offset_date: 0,
            offset_id: 0,
            offset_topic: 0,
            limit: 100,
        })
        .await
        .map_err(|_| ProvisionError::ForumTopics)?;
    for title in COMPANION_TOPIC_TITLES {
        let topic_id = find_topic_id(&topics, title);
        let topic_id = match topic_id {
            Some(id) => id,
            None => {
                client
                    .invoke(&tl::functions::messages::CreateForumTopic {
                        title_missing: false,
                        peer: peer.clone(),
                        title: title.to_owned(),
                        icon_color: None,
                        icon_emoji_id: None,
                        random_id: random_id().map_err(|_| ProvisionError::CreateTopic)?,
                        send_as: None,
                    })
                    .await
                    .map_err(|_| ProvisionError::CreateTopic)?;
                let refreshed = client
                    .invoke(&tl::functions::messages::GetForumTopics {
                        q: Some(title.to_owned()),
                        peer: peer.clone(),
                        offset_date: 0,
                        offset_id: 0,
                        offset_topic: 0,
                        limit: 20,
                    })
                    .await
                    .map_err(|_| ProvisionError::ForumTopics)?;
                find_topic_id(&refreshed, title).ok_or(ProvisionError::CreateTopic)?
            }
        };
        set_topic_id(&mut state, title, topic_id);
        persist(&state_path, &token_path, &state).await?;
    }
    state.stages.forum_topic_created = true;
    persist(&state_path, &token_path, &state).await?;

    // Invite and promote are deliberately reconciled on every repair. A crash
    // after Telegram accepted either call must not leave state booleans as the
    // source of truth.
    match client
        .invoke(&tl::functions::channels::InviteToChannel {
            channel: input_channel(group),
            users: vec![input_user(bot)],
        })
        .await
    {
        Ok(_) => {}
        Err(error) if error.is("USER_ALREADY_PARTICIPANT") => {}
        Err(_) => return Err(ProvisionError::InviteBot),
    }
    state.stages.bot_invited = true;
    persist(&state_path, &token_path, &state).await?;
    client
        .invoke(&tl::functions::channels::EditAdmin {
            channel: input_channel(group),
            user_id: input_user(bot),
            admin_rights: tl::enums::ChatAdminRights::Rights(minimum_rights()),
            rank: None,
        })
        .await
        .map_err(|_| ProvisionError::PromoteBot)?;
    state.stages.bot_rights_configured = true;
    persist(&state_path, &token_path, &state).await?;

    let filters = client
        .invoke(&tl::functions::messages::GetDialogFilters {})
        .await
        .map_err(|_| ProvisionError::DialogFilters)?;
    let (filter_id, mut order, existing_folder) = configure_folder(
        client,
        filters,
        group,
        bot,
        state.identities.companion_folder_id,
    )
    .await?;
    if !existing_folder {
        order.push(filter_id); // preserve every existing custom-folder order.
        client
            .invoke(&tl::functions::messages::UpdateDialogFiltersOrder { order })
            .await
            .map_err(|_| ProvisionError::DialogFilters)?;
    }
    state.identities.companion_folder_id = Some(filter_id);
    state.stages.folder_configured = true;
    state.stages.companion_configured = true;
    state.status = "companion_configured".to_owned();
    persist(&state_path, &token_path, &state).await
}

async fn configure_folder(
    client: &Client,
    filters: tl::enums::messages::DialogFilters,
    group: InputIdentity,
    bot: InputIdentity,
    recorded_id: Option<i32>,
) -> Result<(i32, Vec<i32>, bool), ProvisionError> {
    let filters = match filters {
        tl::enums::messages::DialogFilters::Filters(value) => value.filters,
    };
    let mut order = Vec::new();
    let mut folders = Vec::new();
    for filter in &filters {
        match filter {
            tl::enums::DialogFilter::Filter(filter) => {
                order.push(filter.id);
                folders.push((filter.id, text_with_entities(&filter.title)));
            }
            tl::enums::DialogFilter::Chatlist(filter) => {
                order.push(filter.id);
                folders.push((filter.id, text_with_entities(&filter.title)));
            }
            tl::enums::DialogFilter::Default => {}
        }
    }
    let id = plan_lavis_folder(&folders, recorded_id)?;
    let filter = tl::types::DialogFilter {
        contacts: false,
        non_contacts: false,
        groups: false,
        broadcasts: false,
        bots: false,
        exclude_muted: false,
        exclude_read: false,
        exclude_archived: false,
        title_noanimate: false,
        id,
        title: tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities {
            text: COMPANION_FOLDER_TITLE.to_owned(),
            entities: Vec::new(),
        }),
        emoticon: Some("🤖".to_owned()),
        color: None,
        pinned_peers: Vec::new(),
        include_peers: vec![input_peer_channel(group), input_peer_user(bot)],
        exclude_peers: Vec::new(),
    };
    client
        .invoke(&tl::functions::messages::UpdateDialogFilter {
            id,
            filter: Some(tl::enums::DialogFilter::Filter(filter)),
        })
        .await
        .map_err(|_| ProvisionError::DialogFilters)?;
    let existing_folder = folders
        .iter()
        .any(|(folder_id, title)| *folder_id == id && *title == COMPANION_FOLDER_TITLE);
    Ok((id, order, existing_folder))
}

async fn load_state(
    state_path: PathBuf,
    token_path: PathBuf,
) -> Result<PersistedSetupState, ProvisionError> {
    tokio::task::spawn_blocking(move || {
        let store = SetupStore::new(state_path, token_path);
        match store.load_state() {
            Ok(state) => Ok(state),
            Err(crate::error::SetupStoreError::NotFound) => Ok(PersistedSetupState::default()),
            Err(_) => Err(ProvisionError::Storage),
        }
    })
    .await
    .map_err(|_| ProvisionError::Storage)?
}

async fn persist(
    state_path: &Path,
    token_path: &Path,
    state: &PersistedSetupState,
) -> Result<(), ProvisionError> {
    let state_path = state_path.to_path_buf();
    let token_path = token_path.to_path_buf();
    let state = state.clone();
    tokio::task::spawn_blocking(move || SetupStore::new(state_path, token_path).save_state(&state))
        .await
        .map_err(|_| ProvisionError::Storage)?
        .map_err(|_| ProvisionError::Storage)
}

async fn resolve_bot(client: &Client, username: &str) -> Result<InputIdentity, ProvisionError> {
    let resolved = client
        .invoke(&tl::functions::contacts::ResolveUsername {
            username: username.to_owned(),
            referer: None,
        })
        .await
        .map_err(|_| ProvisionError::ResolveBot)?;
    let tl::enums::contacts::ResolvedPeer::Peer(resolved) = resolved;
    resolved
        .users
        .into_iter()
        .find_map(|user| match user {
            tl::enums::User::User(user)
                if user
                    .username
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(username)) =>
            {
                user.access_hash.map(|access_hash| InputIdentity {
                    id: user.id,
                    access_hash,
                })
            }
            _ => None,
        })
        .ok_or(ProvisionError::ResolveBot)
}

fn extract_created_channel(updates: &tl::enums::Updates, title: &str) -> Option<InputIdentity> {
    updates_chats(updates)?.iter().find_map(|chat| match chat {
        tl::enums::Chat::Channel(channel)
            if channel.title == title && channel.megagroup && channel.forum =>
        {
            channel.access_hash.map(|access_hash| InputIdentity {
                id: channel.id,
                access_hash,
            })
        }
        _ => None,
    })
}

fn updates_chats(updates: &tl::enums::Updates) -> Option<&Vec<tl::enums::Chat>> {
    match updates {
        tl::enums::Updates::Combined(updates) => Some(&updates.chats),
        tl::enums::Updates::Updates(updates) => Some(&updates.chats),
        _ => None,
    }
}

fn group_from_state(state: &PersistedSetupState) -> Option<InputIdentity> {
    Some(InputIdentity {
        id: state.identities.companion_chat_id?,
        access_hash: state.identities.companion_chat_access_hash?,
    })
}

fn text_with_entities(value: &tl::enums::TextWithEntities) -> &str {
    match value {
        tl::enums::TextWithEntities::Entities(value) => &value.text,
    }
}

/// Returns only the recorded Lavis folder. A coincidental user folder called
/// Lavis is a conflict, never an invitation to replace its peer selection.
fn plan_lavis_folder(
    folders: &[(i32, &str)],
    recorded_id: Option<i32>,
) -> Result<i32, ProvisionError> {
    if let Some((id, _)) = folders
        .iter()
        .find(|(_, title)| *title == COMPANION_FOLDER_TITLE)
    {
        return (recorded_id == Some(*id))
            .then_some(*id)
            .ok_or(ProvisionError::FolderConflict);
    }
    (2..i32::MAX)
        .find(|candidate| folders.iter().all(|(id, _)| id != candidate))
        .ok_or(ProvisionError::FolderCapacity)
}

fn find_topic_id(topics: &tl::enums::messages::ForumTopics, title: &str) -> Option<i32> {
    let tl::enums::messages::ForumTopics::Topics(topics) = topics;
    topics.topics.iter().find_map(|topic| match topic {
        tl::enums::ForumTopic::Topic(topic) if topic.title == title => Some(topic.id),
        _ => None,
    })
}

#[cfg(test)]
fn state_topic_id(state: &PersistedSetupState, title: &str) -> Option<i32> {
    match title {
        "General" => state.identities.companion_topic_id,
        "Logs" => state.identities.companion_logs_topic_id,
        "Backups" => state.identities.companion_backups_topic_id,
        _ => None,
    }
}

fn set_topic_id(state: &mut PersistedSetupState, title: &str, id: i32) {
    match title {
        "General" => state.identities.companion_topic_id = Some(id),
        "Logs" => state.identities.companion_logs_topic_id = Some(id),
        "Backups" => state.identities.companion_backups_topic_id = Some(id),
        _ => {}
    }
}

fn input_channel(identity: InputIdentity) -> tl::enums::InputChannel {
    tl::enums::InputChannel::Channel(tl::types::InputChannel {
        channel_id: identity.id,
        access_hash: identity.access_hash,
    })
}
fn input_peer_channel(identity: InputIdentity) -> tl::enums::InputPeer {
    tl::enums::InputPeer::Channel(tl::types::InputPeerChannel {
        channel_id: identity.id,
        access_hash: identity.access_hash,
    })
}
fn input_user(identity: InputIdentity) -> tl::enums::InputUser {
    tl::enums::InputUser::User(tl::types::InputUser {
        user_id: identity.id,
        access_hash: identity.access_hash,
    })
}
fn input_peer_user(identity: InputIdentity) -> tl::enums::InputPeer {
    tl::enums::InputPeer::User(tl::types::InputPeerUser {
        user_id: identity.id,
        access_hash: identity.access_hash,
    })
}
fn random_id() -> Result<i64, getrandom::Error> {
    Ok(getrandom::u64()? as i64)
}
fn minimum_rights() -> tl::types::ChatAdminRights {
    tl::types::ChatAdminRights {
        change_info: false,
        post_messages: false,
        edit_messages: false,
        delete_messages: true,
        ban_users: false,
        invite_users: false,
        pin_messages: true,
        add_admins: false,
        anonymous: false,
        manage_call: false,
        other: false,
        manage_topics: true,
        post_stories: false,
        edit_stories: false,
        delete_stories: false,
        manage_direct_messages: false,
        manage_ranks: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{ProvisionError, plan_lavis_folder, set_topic_id, state_topic_id};
    use crate::setup_store::PersistedSetupState;

    #[test]
    fn topic_identity_mapping_is_fixed_and_lossless() {
        let mut state = PersistedSetupState::default();
        for (title, id) in [("General", 1), ("Logs", 2), ("Backups", 3)] {
            set_topic_id(&mut state, title, id);
        }
        assert_eq!(state_topic_id(&state, "General"), Some(1));
        assert_eq!(state_topic_id(&state, "Logs"), Some(2));
        assert_eq!(state_topic_id(&state, "Backups"), Some(3));
    }

    #[test]
    fn named_user_folder_is_never_replaced_and_all_ids_reserve_order_slots() {
        assert_eq!(
            plan_lavis_folder(&[(2, "Lavis")], None),
            Err(ProvisionError::FolderConflict)
        );
        assert_eq!(plan_lavis_folder(&[(2, "Lavis")], Some(2)), Ok(2));
        assert_eq!(plan_lavis_folder(&[(2, "Chatlist")], None), Ok(3));
    }
}

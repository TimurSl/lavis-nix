use super::protocol::CustomEmojiEntity;

pub fn project_custom_emoji_entities(
    entities: Option<&Vec<grammers_client::tl::enums::MessageEntity>>,
    start_utf16: usize,
    end_utf16: usize,
) -> Vec<CustomEmojiEntity> {
    let Some(entities) = entities else {
        return Vec::new();
    };
    entities
        .iter()
        .filter_map(|entity| {
            let grammers_client::tl::enums::MessageEntity::CustomEmoji(entity) = entity else {
                return None;
            };
            let offset = usize::try_from(entity.offset).ok()?;
            let length = usize::try_from(entity.length).ok()?;
            let end = offset.checked_add(length)?;
            (offset >= start_utf16 && end <= end_utf16 && length > 0).then(|| CustomEmojiEntity {
                offset_utf16: offset - start_utf16,
                length_utf16: length,
                document_id: entity.document_id.to_string(),
            })
        })
        .collect()
}

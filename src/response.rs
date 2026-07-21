pub const MAX_UTF16_UNITS: usize = 4096;
pub const TRUNCATION_SUFFIX: &str = "… output truncated";

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub text: String,
    pub entities: Vec<grammers_client::tl::enums::MessageEntity>,
}

pub struct RenderedResponse {
    pub response: Response,
    pub entity_fallback: bool,
}

impl Response {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: truncate_utf16(&text.into()),
            entities: Vec::new(),
        }
    }

    pub fn preformatted(text: impl Into<String>) -> Self {
        let text = truncate_utf16(&text.into());
        let Some(length) = utf16_i32_len(&text) else {
            return Self::plain(text);
        };
        if text.is_empty() {
            return Self::plain(text);
        }

        Self {
            text,
            entities: vec![
                grammers_client::tl::types::MessageEntityPre {
                    offset: 0,
                    length,
                    language: String::new(),
                }
                .into(),
            ],
        }
    }

    pub fn collapsed(heading: String, body: String) -> RenderedResponse {
        let prefix = format!("{heading}\n\n");
        let text = truncate_utf16(&format!("{prefix}{body}"));
        let Some(body) = text.strip_prefix(&prefix) else {
            return RenderedResponse {
                response: Self::plain(text),
                entity_fallback: true,
            };
        };
        let Some(offset) = utf16_i32_len(&prefix) else {
            return RenderedResponse {
                response: Self::plain(text),
                entity_fallback: true,
            };
        };
        let Some(length) = utf16_i32_len(body) else {
            return RenderedResponse {
                response: Self::plain(text),
                entity_fallback: true,
            };
        };
        if length == 0 {
            return RenderedResponse {
                response: Self::plain(text),
                entity_fallback: true,
            };
        }

        RenderedResponse {
            response: Self {
                text,
                entities: vec![
                    grammers_client::tl::types::MessageEntityBlockquote {
                        offset,
                        length,
                        collapsed: true,
                    }
                    .into(),
                ],
            },
            entity_fallback: false,
        }
    }
}

pub fn truncate_utf16(text: &str) -> String {
    if text.encode_utf16().count() <= MAX_UTF16_UNITS {
        return text.to_owned();
    }
    let suffix_units = TRUNCATION_SUFFIX.encode_utf16().count();
    let limit = MAX_UTF16_UNITS.saturating_sub(suffix_units);
    let mut end = 0;
    let mut units = 0usize;
    let mut last_newline = None;
    for (index, character) in text.char_indices() {
        let character_units = character.len_utf16();
        if units.saturating_add(character_units) > limit {
            break;
        }
        units += character_units;
        end = index + character.len_utf8();
        if character == '\n' {
            last_newline = Some(end);
        }
    }
    let end = last_newline.unwrap_or(end);
    format!("{}{}", &text[..end], TRUNCATION_SUFFIX)
}

fn utf16_i32_len(text: &str) -> Option<i32> {
    i32::try_from(text.encode_utf16().count()).ok()
}

#[cfg(test)]
mod tests {
    use super::{MAX_UTF16_UNITS, Response, TRUNCATION_SUFFIX, truncate_utf16};

    #[test]
    fn truncates_non_bmp_text_at_utf16_boundaries() {
        let text = "🦀".repeat(MAX_UTF16_UNITS);
        let output = truncate_utf16(&text);

        assert!(output.ends_with(TRUNCATION_SUFFIX));
        assert!(output.encode_utf16().count() <= MAX_UTF16_UNITS);
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }

    #[test]
    fn preformatted_entity_spans_final_text() {
        let response = Response::preformatted("  output\n");
        let grammers_client::tl::enums::MessageEntity::Pre(entity) = &response.entities[0] else {
            panic!("expected a preformatted entity");
        };

        assert_eq!(entity.offset, 0);
        assert_eq!(
            usize::try_from(entity.length).unwrap(),
            response.text.encode_utf16().count()
        );
    }

    #[test]
    fn empty_collapsed_body_does_not_create_an_entity() {
        let rendered = Response::collapsed("heading".to_owned(), String::new());

        assert!(rendered.response.entities.is_empty());
        assert!(rendered.entity_fallback);
    }
}

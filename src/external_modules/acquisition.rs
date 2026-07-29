//! Bounded acquisition of `.lmod` documents from Saved Messages only.

use grammers_client::{Client, InvocationError, media::Media, session::types::PeerId};
use thiserror::Error;

use super::source_inspection::AcquiredLmod;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AcquisitionLimits {
    pub max_archive_bytes: usize,
}
impl Default for AcquisitionLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum AcquisitionError {
    #[error("module source must be a newly sent message in Saved Messages")]
    NotSavedMessage,
    #[error("module source must be a document named with the exact .lmod extension")]
    NotLmodDocument,
    #[error("declared document size exceeds the archive limit")]
    DeclaredSizeExceeded,
    #[error("archive exceeds the archive limit")]
    SizeExceeded,
    #[error("failed to download module document")]
    Download(#[source] InvocationError),
}

/// Acquires one outgoing document from the account's Saved Messages. The
/// message object is a fresh update snapshot; callers must not substitute an
/// arbitrary message id or peer. Declared size is only a preflight: every
/// downloaded chunk is counted before it is retained.
pub(crate) struct ModuleSourceAcquirer<'a> {
    client: &'a Client,
    saved_messages_peer: PeerId,
    limits: AcquisitionLimits,
}
impl<'a> ModuleSourceAcquirer<'a> {
    pub(crate) fn new(
        client: &'a Client,
        saved_messages_peer: PeerId,
        limits: AcquisitionLimits,
    ) -> Self {
        Self {
            client,
            saved_messages_peer,
            limits,
        }
    }

    pub(crate) async fn acquire(
        &self,
        message: &grammers_client::message::Message,
    ) -> Result<AcquiredLmod, AcquisitionError> {
        if !message.outgoing() || message.id() <= 0 || message.peer_id() != self.saved_messages_peer
        {
            return Err(AcquisitionError::NotSavedMessage);
        }
        let media = message.media().ok_or(AcquisitionError::NotLmodDocument)?;
        let Media::Document(document) = media else {
            return Err(AcquisitionError::NotLmodDocument);
        };
        if !document.name().is_some_and(|name| name.ends_with(".lmod")) {
            return Err(AcquisitionError::NotLmodDocument);
        }
        let maximum = self.limits.max_archive_bytes;
        if document.size().is_some_and(|size| size > maximum) {
            return Err(AcquisitionError::DeclaredSizeExceeded);
        }
        let mut download = self.client.iter_download(&document);
        let mut bytes = Vec::with_capacity(maximum.min(64 * 1024));
        while let Some(chunk) = download.next().await.map_err(AcquisitionError::Download)? {
            let total = bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(AcquisitionError::SizeExceeded)?;
            if total > maximum {
                return Err(AcquisitionError::SizeExceeded);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(AcquiredLmod::archive(bytes))
    }
}

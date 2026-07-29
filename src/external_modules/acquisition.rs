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

/// Transport-independent, bounded document aggregation. A caller only obtains
/// the completed bytes after every chunk has passed the actual-size limit.
struct BoundedDocumentBytes {
    maximum: usize,
    bytes: Vec<u8>,
}

impl BoundedDocumentBytes {
    fn new(declared_size: Option<usize>, maximum: usize) -> Result<Self, AcquisitionError> {
        if declared_size.is_some_and(|size| size > maximum) {
            return Err(AcquisitionError::DeclaredSizeExceeded);
        }
        Ok(Self {
            maximum,
            bytes: Vec::with_capacity(maximum.min(64 * 1024)),
        })
    }

    fn push(&mut self, chunk: &[u8]) -> Result<(), AcquisitionError> {
        let total = self
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(AcquisitionError::SizeExceeded)?;
        if total > self.maximum {
            return Err(AcquisitionError::SizeExceeded);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

enum AggregateError<E> {
    Acquisition(AcquisitionError),
    Transport(E),
}

/// A synchronous adapter for tests and non-Telegram byte-stream boundaries.
fn aggregate_document_chunks<E>(
    declared_size: Option<usize>,
    maximum: usize,
    mut next: impl FnMut() -> Result<Option<Vec<u8>>, E>,
) -> Result<Vec<u8>, AggregateError<E>> {
    let mut aggregate =
        BoundedDocumentBytes::new(declared_size, maximum).map_err(AggregateError::Acquisition)?;
    while let Some(chunk) = next().map_err(AggregateError::Transport)? {
        aggregate
            .push(&chunk)
            .map_err(AggregateError::Acquisition)?;
    }
    Ok(aggregate.finish())
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
        let mut aggregate =
            BoundedDocumentBytes::new(document.size(), self.limits.max_archive_bytes)?;
        let mut download = self.client.iter_download(&document);
        while let Some(chunk) = download.next().await.map_err(AcquisitionError::Download)? {
            aggregate.push(&chunk)?;
        }
        Ok(AcquiredLmod::archive(aggregate.finish()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn missing_declared_size_allows_a_bounded_download() {
        let mut chunks = [Some(vec![1, 2]), Some(vec![3]), None].into_iter();
        assert_eq!(
            aggregate_document_chunks(None, 3, || Ok::<_, ()>(chunks.next().flatten())).unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn declared_oversize_rejects_before_reader_is_consumed() {
        let reads = Cell::new(0);
        let result = aggregate_document_chunks(Some(4), 3, || {
            reads.set(reads.get() + 1);
            Ok::<_, ()>(Some(vec![1]))
        });
        assert!(matches!(
            result,
            Err(AggregateError::Acquisition(
                AcquisitionError::DeclaredSizeExceeded
            ))
        ));
        assert_eq!(reads.get(), 0);
    }

    #[test]
    fn exact_actual_limit_succeeds() {
        let mut chunks = [Some(vec![1, 2]), Some(vec![3]), None].into_iter();
        assert_eq!(
            aggregate_document_chunks(Some(3), 3, || Ok::<_, ()>(chunks.next().flatten())).unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn actual_oversize_stops_without_consuming_the_remaining_stream() {
        let reads = Cell::new(0);
        let mut chunks = [Some(vec![1, 2]), Some(vec![3]), Some(vec![4]), None].into_iter();
        let result = aggregate_document_chunks(None, 2, || {
            reads.set(reads.get() + 1);
            Ok::<_, ()>(chunks.next().flatten())
        });
        assert!(matches!(
            result,
            Err(AggregateError::Acquisition(AcquisitionError::SizeExceeded))
        ));
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn transport_errors_are_preserved() {
        let result = aggregate_document_chunks::<&'static str>(None, 3, || Err("offline"));
        assert!(matches!(result, Err(AggregateError::Transport("offline"))));
    }
}

//! Bounded acquisition of `.lmod` documents from Saved Messages only.

use grammers_client::{
    Client, InvocationError, client::DownloadIter, media::Media, session::types::PeerId,
};
use std::future::Future;
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

#[derive(Debug)]
enum AggregateError<E> {
    Acquisition(AcquisitionError),
    Transport(E),
}

/// A transport-independent asynchronous byte-stream adapter. It is kept
/// separate from Telegram message validation so its bounds are testable
/// without a `Client`.
trait DocumentChunkSource {
    type Error;

    fn next_chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + '_;
}

async fn aggregate_document_chunks<S: DocumentChunkSource>(
    declared_size: Option<usize>,
    maximum: usize,
    source: &mut S,
) -> Result<Vec<u8>, AggregateError<S::Error>> {
    let mut aggregate =
        BoundedDocumentBytes::new(declared_size, maximum).map_err(AggregateError::Acquisition)?;
    while let Some(chunk) = source
        .next_chunk()
        .await
        .map_err(AggregateError::Transport)?
    {
        aggregate
            .push(&chunk)
            .map_err(AggregateError::Acquisition)?;
    }
    Ok(aggregate.finish())
}

struct TelegramDownload<'a> {
    download: &'a mut DownloadIter,
}

impl DocumentChunkSource for TelegramDownload<'_> {
    type Error = InvocationError;

    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
        self.download.next().await
    }
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
        let mut download = self.client.iter_download(&document);
        let mut chunks = TelegramDownload {
            download: &mut download,
        };
        let bytes =
            aggregate_document_chunks(document.size(), self.limits.max_archive_bytes, &mut chunks)
                .await
                .map_err(|error| match error {
                    AggregateError::Acquisition(error) => error,
                    AggregateError::Transport(error) => AcquisitionError::Download(error),
                })?;
        Ok(AcquiredLmod::archive(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::VecDeque};

    struct TestChunks<E> {
        reads: Cell<usize>,
        chunks: VecDeque<Result<Option<Vec<u8>>, E>>,
    }

    impl<E> TestChunks<E> {
        fn new(chunks: impl IntoIterator<Item = Result<Option<Vec<u8>>, E>>) -> Self {
            Self {
                reads: Cell::new(0),
                chunks: chunks.into_iter().collect(),
            }
        }
    }

    impl<E> DocumentChunkSource for TestChunks<E> {
        type Error = E;

        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
            self.reads.set(self.reads.get() + 1);
            self.chunks.pop_front().unwrap_or(Ok(None))
        }
    }

    #[tokio::test]
    async fn missing_declared_size_allows_a_bounded_download() {
        let mut chunks =
            TestChunks::new([Ok::<_, ()>(Some(vec![1, 2])), Ok(Some(vec![3])), Ok(None)]);
        assert_eq!(
            aggregate_document_chunks(None, 3, &mut chunks)
                .await
                .unwrap(),
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn declared_oversize_rejects_before_reader_is_consumed() {
        let mut chunks = TestChunks::new([Ok::<_, ()>(Some(vec![1]))]);
        let result = aggregate_document_chunks(Some(4), 3, &mut chunks).await;
        assert!(matches!(
            result,
            Err(AggregateError::Acquisition(
                AcquisitionError::DeclaredSizeExceeded
            ))
        ));
        assert_eq!(chunks.reads.get(), 0);
    }

    #[tokio::test]
    async fn exact_actual_limit_succeeds() {
        let mut chunks =
            TestChunks::new([Ok::<_, ()>(Some(vec![1, 2])), Ok(Some(vec![3])), Ok(None)]);
        assert_eq!(
            aggregate_document_chunks(Some(3), 3, &mut chunks)
                .await
                .unwrap(),
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn actual_oversize_stops_without_consuming_the_remaining_stream() {
        let mut chunks = TestChunks::new([
            Ok::<_, ()>(Some(vec![1, 2])),
            Ok(Some(vec![3])),
            Ok(Some(vec![4])),
            Ok(None),
        ]);
        let result = aggregate_document_chunks(None, 2, &mut chunks).await;
        assert!(matches!(
            result,
            Err(AggregateError::Acquisition(AcquisitionError::SizeExceeded))
        ));
        assert_eq!(chunks.reads.get(), 2);
    }

    #[tokio::test]
    async fn transport_errors_are_preserved() {
        let mut chunks = TestChunks::new([Err::<Option<Vec<u8>>, _>("offline")]);
        let result = aggregate_document_chunks(None, 3, &mut chunks).await;
        assert!(matches!(result, Err(AggregateError::Transport("offline"))));
    }
}

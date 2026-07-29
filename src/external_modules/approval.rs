//! One-shot, in-memory approvals for inspected module staging.

use super::source_inspection::{Clock, ModuleInstallPlan, PendingInspection, RandomSource};
use std::{
    collections::BTreeMap,
    fmt,
    time::{Duration, SystemTime},
};
use thiserror::Error;

pub const APPROVAL_ID_BYTES: usize = 10;
pub const APPROVAL_ID_ENCODED_CHARS: usize = 16;
pub const DEFAULT_APPROVAL_TTL: Duration = Duration::from_secs(600);

const APPROVAL_ID_DISPLAY_CHARS: usize = APPROVAL_ID_ENCODED_CHARS + 3;
const MAX_COLLISION_ATTEMPTS: usize = 32;
const CROCKFORD_BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// An 80-bit approval id in canonical, grouped Crockford Base32 form.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ApprovalId([u8; APPROVAL_ID_BYTES]);

impl ApprovalId {
    pub(crate) fn from_bytes(bytes: [u8; APPROVAL_ID_BYTES]) -> Self {
        Self(bytes)
    }

    /// Parses only canonical uppercase `XXXX-XXXX-XXXX-XXXX` identifiers.
    pub(crate) fn parse(value: &str) -> Result<Self, ApprovalError> {
        let bytes = value.as_bytes();
        if bytes.len() != APPROVAL_ID_DISPLAY_CHARS
            || [4, 9, 14].iter().any(|&index| bytes[index] != b'-')
        {
            return Err(ApprovalError::InvalidId);
        }

        let mut encoded = [0_u8; APPROVAL_ID_ENCODED_CHARS];
        let mut index = 0;
        for &byte in bytes {
            if byte != b'-' {
                encoded[index] = decode_crockford(byte).ok_or(ApprovalError::InvalidId)?;
                index += 1;
            }
        }

        let mut bits = 0_u128;
        for digit in encoded {
            bits = (bits << 5) | u128::from(digit);
        }
        let mut output = [0_u8; APPROVAL_ID_BYTES];
        for (index, byte) in output.iter_mut().enumerate() {
            *byte = (bits >> (72 - index * 8)) as u8;
        }
        Ok(Self(output))
    }
}

fn decode_crockford(byte: u8) -> Option<u8> {
    CROCKFORD_BASE32
        .iter()
        .position(|&candidate| candidate == byte)
        .map(|index| index as u8)
}

impl fmt::Display for ApprovalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bits = u128::from_be_bytes([
            0, 0, 0, 0, 0, 0, self.0[0], self.0[1], self.0[2], self.0[3], self.0[4],
            self.0[5], self.0[6], self.0[7], self.0[8], self.0[9],
        ]);
        for index in 0..APPROVAL_ID_ENCODED_CHARS {
            if index != 0 && index % 4 == 0 {
                formatter.write_str("-")?;
            }
            let digit = ((bits >> (75 - index * 5)) & 31) as usize;
            formatter.write_str(&(CROCKFORD_BASE32[digit] as char).to_string())?;
        }
        Ok(())
    }
}

impl fmt::Debug for ApprovalId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ApprovalId").field(&self.to_string()).finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApprovalLimits {
    pub(crate) max_entries: usize,
    pub(crate) max_pending_expanded_bytes: u64,
}

impl Default for ApprovalLimits {
    fn default() -> Self {
        Self {
            max_entries: 16,
            max_pending_expanded_bytes: 128 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ApprovalError {
    #[error("invalid approval id")]
    InvalidId,
    #[error("approval is unavailable")]
    Unavailable,
    #[error("approval quota exceeded")]
    QuotaExceeded,
    #[error("approval id collision retry limit exceeded")]
    CollisionLimit,
    #[error("approval accounting invariant failed")]
    Accounting,
    #[error("secure randomness failed")]
    Entropy,
}

struct Entry {
    expires_at: SystemTime,
    pending: PendingInspection,
}

/// The only retention state is `entries`; removed entries are never retained as
/// tombstones. Clock and entropy are generic for deterministic tests.
pub(crate) struct ApprovalStore<C: Clock, R: RandomSource> {
    clock: C,
    random: R,
    ttl: Duration,
    limits: ApprovalLimits,
    pending_expanded_bytes: u64,
    entries: BTreeMap<ApprovalId, Entry>,
}

impl<C: Clock, R: RandomSource> ApprovalStore<C, R> {
    pub(crate) fn new(clock: C, random: R, ttl: Duration, limits: ApprovalLimits) -> Self {
        Self {
            clock,
            random,
            ttl,
            limits,
            pending_expanded_bytes: 0,
            entries: BTreeMap::new(),
        }
    }

    pub(crate) fn issue(
        &mut self,
        pending: PendingInspection,
    ) -> Result<(ApprovalId, ModuleInstallPlan), ApprovalError> {
        self.purge_expired()?;
        let expanded_bytes = pending.expanded_bytes();
        let next_bytes = match self.pending_expanded_bytes.checked_add(expanded_bytes) {
            Some(value) => value,
            None => {
                self.cleanup_pending(None, pending);
                return Err(ApprovalError::Accounting);
            }
        };
        if self.entries.len() >= self.limits.max_entries
            || next_bytes > self.limits.max_pending_expanded_bytes
        {
            self.cleanup_pending(None, pending);
            return Err(ApprovalError::QuotaExceeded);
        }

        let approval_id = match self.fresh_id() {
            Ok(value) => value,
            Err(error) => {
                self.cleanup_pending(None, pending);
                return Err(error);
            }
        };
        let expires_at = match self.clock.now().checked_add(self.ttl) {
            Some(value) => value,
            None => {
                self.cleanup_pending(None, pending);
                return Err(ApprovalError::Accounting);
            }
        };
        let plan = pending.plan.clone();
        self.entries.insert(
            approval_id,
            Entry {
                expires_at,
                pending,
            },
        );
        self.pending_expanded_bytes = next_bytes;
        Ok((approval_id, plan))
    }

    /// Returns review data only; a stage path is never exposed.
    pub(crate) fn get(&mut self, approval_id: ApprovalId) -> Result<&ModuleInstallPlan, ApprovalError> {
        self.purge_expired()?;
        self.entries
            .get(&approval_id)
            .map(|entry| &entry.pending.plan)
            .ok_or(ApprovalError::Unavailable)
    }

    /// Transfers stage ownership to the installer. Redeeming does not clean it.
    pub(crate) fn redeem(
        &mut self,
        approval_id: ApprovalId,
    ) -> Result<PendingInspection, ApprovalError> {
        self.purge_expired()?;
        self.remove(approval_id)?.ok_or(ApprovalError::Unavailable)
    }

    pub(crate) fn revoke(&mut self, approval_id: ApprovalId) -> Result<bool, ApprovalError> {
        self.purge_expired()?;
        if let Some(pending) = self.remove(approval_id)? {
            self.cleanup_pending(Some(approval_id), pending);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(crate) fn list_pending(
        &mut self,
    ) -> Result<Vec<(ApprovalId, &ModuleInstallPlan)>, ApprovalError> {
        self.purge_expired()?;
        Ok(self
            .entries
            .iter()
            .map(|(approval_id, entry)| (*approval_id, &entry.pending.plan))
            .collect())
    }

    /// Expiry is inclusive: an entry is expired when `now >= expires_at`.
    pub(crate) fn purge_expired(&mut self) -> Result<usize, ApprovalError> {
        let now = self.clock.now();
        let expired: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(approval_id, entry)| {
                is_expired(now, entry.expires_at).then_some(*approval_id)
            })
            .collect();
        for approval_id in &expired {
            if let Some(pending) = self.remove(*approval_id)? {
                self.cleanup_pending(Some(*approval_id), pending);
            }
        }
        Ok(expired.len())
    }

    pub(crate) fn shutdown(&mut self) -> Result<usize, ApprovalError> {
        self.purge_expired()?;
        let entries = std::mem::take(&mut self.entries);
        self.pending_expanded_bytes = 0;
        let count = entries.len();
        for (approval_id, entry) in entries {
            self.cleanup_pending(Some(approval_id), entry.pending);
        }
        Ok(count)
    }

    fn remove(&mut self, approval_id: ApprovalId) -> Result<Option<PendingInspection>, ApprovalError> {
        let Some(entry) = self.entries.remove(&approval_id) else {
            return Ok(None);
        };
        self.pending_expanded_bytes = match self
            .pending_expanded_bytes
            .checked_sub(entry.pending.expanded_bytes())
        {
            Some(value) => value,
            None => {
                self.cleanup_pending(Some(approval_id), entry.pending);
                return Err(ApprovalError::Accounting);
            }
        };
        Ok(Some(entry.pending))
    }

    fn cleanup_pending(&self, approval_id: Option<ApprovalId>, pending: PendingInspection) {
        tracing::warn!(
            approval_id = ?approval_id,
            "removing approval staging; cleanup is best effort"
        );
        pending.cleanup();
    }

    fn fresh_id(&mut self) -> Result<ApprovalId, ApprovalError> {
        generate_approval_id(&mut self.random, |approval_id| {
            self.entries.contains_key(&approval_id)
        })
    }
}

fn generate_approval_id<R: RandomSource>(
    random: &mut R,
    is_live: impl Fn(ApprovalId) -> bool,
) -> Result<ApprovalId, ApprovalError> {
    for _ in 0..MAX_COLLISION_ATTEMPTS {
        let mut bytes = [0_u8; APPROVAL_ID_BYTES];
        random.fill(&mut bytes).map_err(|_| ApprovalError::Entropy)?;
        let approval_id = ApprovalId::from_bytes(bytes);
        if !is_live(approval_id) {
            return Ok(approval_id);
        }
    }
    Err(ApprovalError::CollisionLimit)
}

fn is_expired(now: SystemTime, expires_at: SystemTime) -> bool {
    now >= expires_at
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_modules::source_inspection::SourceInspectionError;
    use std::collections::{BTreeMap, HashSet};

    struct SequenceRandom(Vec<[u8; APPROVAL_ID_BYTES]>);

    impl RandomSource for SequenceRandom {
        fn fill(&mut self, output: &mut [u8]) -> Result<(), SourceInspectionError> {
            let bytes = self.0.remove(0);
            output.copy_from_slice(&bytes);
            Ok(())
        }
    }

    #[test]
    fn approval_id_round_trips_only_in_canonical_form() {
        let id = ApprovalId::from_bytes([0x12; APPROVAL_ID_BYTES]);
        let text = id.to_string();
        assert_eq!(text.len(), APPROVAL_ID_DISPLAY_CHARS);
        assert_eq!(ApprovalId::parse(&text), Ok(id));
        assert!(ApprovalId::parse(&text.to_lowercase()).is_err());
        assert!(ApprovalId::parse("0123-4567-89AB-CDEF-").is_err());
        assert!(ApprovalId::parse("0123-4567-89AB-CDO0").is_err());
    }

    #[test]
    fn approval_id_has_value_traits_for_maps_and_sets() {
        let first = ApprovalId::from_bytes([1; APPROVAL_ID_BYTES]);
        let second = ApprovalId::from_bytes([2; APPROVAL_ID_BYTES]);
        let mut ordered = BTreeMap::new();
        ordered.insert(second, "second");
        ordered.insert(first, "first");
        assert_eq!(ordered.keys().next(), Some(&first));
        let mut hashed = HashSet::new();
        hashed.insert(first);
        assert!(hashed.contains(&first));
    }

    #[test]
    fn live_collisions_retry_without_tombstones() {
        let occupied = ApprovalId::from_bytes([7; APPROVAL_ID_BYTES]);
        let fresh = ApprovalId::from_bytes([8; APPROVAL_ID_BYTES]);
        let mut random = SequenceRandom(vec![occupied.0, fresh.0]);
        assert_eq!(
            generate_approval_id(&mut random, |candidate| candidate == occupied),
            Ok(fresh)
        );
    }

    #[test]
    fn removed_ids_are_not_stale_live_references() {
        let id = ApprovalId::from_bytes([9; APPROVAL_ID_BYTES]);
        let mut live = BTreeMap::new();
        live.insert(id, ());
        assert!(live.remove(&id).is_some());
        let mut random = SequenceRandom(vec![id.0]);
        assert_eq!(
            generate_approval_id(&mut random, |candidate| live.contains_key(&candidate)),
            Ok(id)
        );
    }

    #[test]
    fn expiry_boundary_is_inclusive() {
        let now = SystemTime::UNIX_EPOCH;
        assert!(is_expired(now, now));
        assert!(!is_expired(now, now + Duration::from_secs(1)));
    }
}

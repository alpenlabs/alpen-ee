//! Alpen protocol spec versions and their activation schedule.
//!
//! The EE upgrades EVM-style: logic is gated on spec versions with activation
//! coordinates, so a single binary can execute both sides of an upgrade
//! boundary. [`AlpenSpecId`] names the versions and [`AlpenSpecSchedule`]
//! carries the activation schedule that gates them.

use core::convert::identity;
use std::{collections::BTreeMap, fmt};

use num_enum::{IntoPrimitive, TryFromPrimitive};
use serde::{Deserialize, Serialize, Serializer};
use thiserror::Error;

/// Identifies an Alpen protocol spec version.
///
/// One variant per protocol revision, in activation order, starting with the
/// genesis rules as [`AlpenSpecId::V0`]. The numeric discriminant is the
/// stable identity: it orders versions, making "the successor of a version"
/// well-defined. Discriminants MUST stay contiguous from 0 —
/// [`AlpenSpecSchedule`] indexes its activation coordinates by discriminant,
/// so adding a variant is only this one line, but a gap would desynchronize
/// the schedule. The variant name is the human-readable form: it is this
/// type's serde representation (snake_case) and keys the schedule's
/// serialized form, so an unknown or misspelled spec name fails to decode
/// instead of being accepted as an activation the node cannot honor.
///
/// The primitive conversions are derived so they cannot go stale when a
/// variant is added; [`TryFrom<u16>`] errs with the raw id it has no variant
/// for.
///
/// What a version *means* for the EVM is the per-version chain spec derived
/// in [`EvmSpec`](crate::EvmSpec); this type only names versions and orders
/// them.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Deserialize,
    IntoPrimitive,
    TryFromPrimitive,
)]
#[serde(rename_all = "snake_case")]
#[num_enum(error_type(name = u16, constructor = identity))]
#[repr(u16)]
pub enum AlpenSpecId {
    /// Genesis spec version: the rules in force from the genesis block
    /// onward, active since genesis in every schedule.
    V0 = 0,

    /// First protocol upgrade; placeholder name until that upgrade is
    /// defined.
    V1 = 1,
}

impl AlpenSpecId {
    /// Returns the version that succeeds this one in discriminant order.
    ///
    /// Errs with the raw id (mirroring [`TryFrom<u16>`]) when this binary has
    /// no variant for the successor — an upgrade this binary cannot execute.
    pub fn successor(self) -> Result<Self, u16> {
        Self::try_from(u16::from(self) + 1)
    }
}

/// Serializes as the snake_case variant name, matching what the derived
/// [`Deserialize`] accepts.
///
/// Written out rather than derived because a derived unit-variant impl
/// serializes through `serialize_unit_variant`, which TOML refuses as a
/// table key. Config files key their per-version tables by this type (see
/// `[sequencer.prover.programs.<spec_version>]`), so it has to serialize as
/// a plain string.
impl Serialize for AlpenSpecId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl fmt::Display for AlpenSpecId {
    /// Matches this type's snake_case serde representation (e.g. `v0`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V0 => f.write_str("v0"),
            Self::V1 => f.write_str("v1"),
        }
    }
}

/// The Alpen spec activation schedule: which versions are scheduled and from
/// which activation coordinate each one applies.
///
/// [`AlpenSpecId::V0`] is the genesis version, always active from coordinate
/// 0; the schedule only tracks the upgrades after it, as the activation
/// coordinates of a contiguous run of successors (`upgrades[i]` belongs to
/// the version with discriminant `i + 1`). Versions activate strictly in
/// succession at nondecreasing coordinates, so a gapped schedule ("v2
/// scheduled, v1 disabled") or an inverted one ("v2 active while v1 is not")
/// is unrepresentable, and a new [`AlpenSpecId`] variant needs no change
/// here — every method derives its answer from the discriminant.
///
/// A version with activation coordinate `n` is active at coordinate `c` iff
/// `c >= n`; versions past the scheduled run are disabled. The params
/// artifact carries the base schedule; an upgrade's real activation
/// coordinate is derived at runtime from where the VK-update message lands in
/// the inbox ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AlpenSpecScheduleRepr", into = "AlpenSpecScheduleRepr")]
pub struct AlpenSpecSchedule {
    /// Activation coordinate of each scheduled post-genesis version, indexed
    /// by predecessor count: `upgrades[i]` activates discriminant `i + 1`.
    upgrades: Vec<u64>,
}

/// A schedule change that would violate [`AlpenSpecSchedule`]'s invariants,
/// whether applied at runtime or decoded from a params artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AlpenSpecScheduleError {
    /// [`AlpenSpecId::V0`] is the genesis version: it is always active from
    /// coordinate 0 and can be neither rescheduled nor disabled.
    #[error(
        "v0 is the genesis version, always active from coordinate 0; it cannot be rescheduled or disabled"
    )]
    GenesisFixed,

    /// The artifact schedules `spec` while its predecessor is unscheduled,
    /// leaving a gap in the activation sequence.
    #[error(
        "cannot schedule {spec:?} while its predecessor is unscheduled (latest scheduled: {latest:?})"
    )]
    Gap {
        /// The version whose scheduling was rejected.
        spec: AlpenSpecId,
        /// The newest version scheduled before the gap.
        latest: AlpenSpecId,
    },

    /// Scheduling the successor at `coord` would activate it before its
    /// predecessor at `prev`; successors activate at nondecreasing
    /// coordinates.
    #[error(
        "cannot schedule the successor at {coord}, before its predecessor's activation at {prev}"
    )]
    OutOfOrder {
        /// The rejected activation coordinate.
        coord: u64,
        /// The predecessor's activation coordinate.
        prev: u64,
    },

    /// The version to schedule has no [`AlpenSpecId`] variant in this
    /// binary — old software has hit an upgrade it cannot execute.
    #[error("no spec version with id {0} in this binary")]
    UnknownSuccessor(u16),
}

impl AlpenSpecSchedule {
    /// The genesis schedule: [`AlpenSpecId::V0`] active since genesis, every
    /// later version unscheduled until an upgrade activates it.
    pub const fn genesis() -> Self {
        Self {
            upgrades: Vec::new(),
        }
    }

    /// Returns the newest scheduled version (regardless of whether its
    /// activation coordinate has been reached). Its successor is the version
    /// the next upgrade activates.
    pub fn latest_scheduled(&self) -> AlpenSpecId {
        AlpenSpecId::try_from(self.upgrades.len() as u16).expect(
            "AlpenSpecSchedule invariant: every scheduled version has an AlpenSpecId variant",
        )
    }

    /// Returns the activation coordinate of `spec`, or `None` if unscheduled.
    pub fn activation_of(&self, spec: AlpenSpecId) -> Option<u64> {
        match u16::from(spec) {
            0 => Some(0),
            d => self.upgrades.get(usize::from(d) - 1).copied(),
        }
    }

    /// Returns whether `spec` is active at `coord`.
    pub fn is_active(&self, spec: AlpenSpecId, coord: u64) -> bool {
        self.activation_of(spec)
            .is_some_and(|activation| coord >= activation)
    }

    /// Schedules the successor of the newest scheduled version at `coord`
    /// and returns which version that is.
    ///
    /// This is the discovery-side entry point: a VK-update boundary does not
    /// name the version it activates, so the activating version is *defined*
    /// as the successor. Errs when `coord` precedes the newest scheduled
    /// activation ([`AlpenSpecScheduleError::OutOfOrder`]) or when this
    /// binary has no [`AlpenSpecId`] variant for the successor
    /// ([`AlpenSpecScheduleError::UnknownSuccessor`]).
    pub fn schedule_successor(
        &mut self,
        coord: u64,
    ) -> Result<AlpenSpecId, AlpenSpecScheduleError> {
        if let Some(&prev) = self.upgrades.last() {
            if coord < prev {
                return Err(AlpenSpecScheduleError::OutOfOrder { coord, prev });
            }
        }
        let successor = AlpenSpecId::try_from(self.upgrades.len() as u16 + 1)
            .map_err(AlpenSpecScheduleError::UnknownSuccessor)?;
        self.upgrades.push(coord);
        Ok(successor)
    }
}

impl Default for AlpenSpecSchedule {
    fn default() -> Self {
        Self::genesis()
    }
}

/// Serialized form of [`AlpenSpecSchedule`]: one entry per *scheduled*
/// version (e.g. `{"v0": 0, "v1": 7}`); an absent version is unscheduled.
/// Conversion back re-validates the invariants, so a gapped, inverted, or
/// v0-disabled schedule is rejected at load.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
struct AlpenSpecScheduleRepr(BTreeMap<AlpenSpecId, u64>);

/// Every known version, in discriminant order.
pub(crate) fn known_versions() -> impl Iterator<Item = AlpenSpecId> {
    (0u16..).map_while(|d| AlpenSpecId::try_from(d).ok())
}

impl From<AlpenSpecSchedule> for AlpenSpecScheduleRepr {
    fn from(schedule: AlpenSpecSchedule) -> Self {
        Self(
            known_versions()
                .filter_map(|spec| Some((spec, schedule.activation_of(spec)?)))
                .collect(),
        )
    }
}

impl TryFrom<AlpenSpecScheduleRepr> for AlpenSpecSchedule {
    type Error = AlpenSpecScheduleError;

    fn try_from(repr: AlpenSpecScheduleRepr) -> Result<Self, Self::Error> {
        let coord_of = |spec| repr.0.get(&spec).copied();
        if coord_of(AlpenSpecId::V0) != Some(0) {
            return Err(AlpenSpecScheduleError::GenesisFixed);
        }
        let mut schedule = AlpenSpecSchedule::genesis();
        let mut run_ended = false;
        for spec in known_versions().skip(1) {
            match coord_of(spec) {
                // An unscheduled version ends the scheduled run; anything
                // scheduled past it would be a gap.
                None => run_ended = true,
                Some(_) if run_ended => {
                    return Err(AlpenSpecScheduleError::Gap {
                        spec,
                        latest: schedule.latest_scheduled(),
                    });
                }
                Some(coord) => {
                    schedule.schedule_successor(coord)?;
                }
            }
        }
        Ok(schedule)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schedule with `AlpenSpecId::V1` activating at `coord`.
    fn v1_at(coord: u64) -> AlpenSpecSchedule {
        let mut schedule = AlpenSpecSchedule::genesis();
        assert_eq!(schedule.schedule_successor(coord), Ok(AlpenSpecId::V1));
        schedule
    }

    /// Serde is the human-readable form (the variant name). The stable
    /// numeric identity is exercised by [`spec_id_u16_roundtrip`] instead.
    #[test]
    fn spec_id_serde_is_the_variant_name() {
        assert_eq!(serde_json::to_string(&AlpenSpecId::V0).unwrap(), r#""v0""#);
        assert_eq!(
            serde_json::from_str::<AlpenSpecId>(r#""v1""#).unwrap(),
            AlpenSpecId::V1
        );
        assert!(serde_json::from_str::<AlpenSpecId>(r#""nope""#).is_err());
    }

    /// Raw spec ids round-trip through the enum; unknown ids surface as
    /// errors instead of misparsing. Pinning the *first* unknown discriminant
    /// also guards contiguity: when a new variant lands, this assertion fails
    /// and must be bumped alongside it.
    #[test]
    fn spec_id_u16_roundtrip() {
        assert_eq!(u16::from(AlpenSpecId::V0), 0);
        assert_eq!(u16::from(AlpenSpecId::V1), 1);
        assert_eq!(AlpenSpecId::try_from(0u16).unwrap(), AlpenSpecId::V0);
        assert_eq!(AlpenSpecId::try_from(1u16).unwrap(), AlpenSpecId::V1);
        assert_eq!(AlpenSpecId::try_from(2u16), Err(2));
        assert_eq!(AlpenSpecId::try_from(0xFFFFu16), Err(0xFFFF));
    }

    #[test]
    fn successor_chains_and_errs_past_known_versions() {
        assert_eq!(AlpenSpecId::V0.successor(), Ok(AlpenSpecId::V1));
        assert_eq!(AlpenSpecId::V1.successor(), Err(2));
    }

    #[test]
    fn is_active_boundaries() {
        let schedule = v1_at(100);
        assert!(!schedule.is_active(AlpenSpecId::V1, 99));
        assert!(schedule.is_active(AlpenSpecId::V1, 100));
        assert!(schedule.is_active(AlpenSpecId::V1, 101));
    }

    #[test]
    fn v0_is_always_active() {
        let schedule = AlpenSpecSchedule::genesis();
        assert_eq!(schedule.activation_of(AlpenSpecId::V0), Some(0));
        assert!(schedule.is_active(AlpenSpecId::V0, 0));
        assert!(schedule.is_active(AlpenSpecId::V0, u64::MAX));
    }

    #[test]
    fn unscheduled_means_never_active() {
        let schedule = AlpenSpecSchedule::genesis();
        assert_eq!(schedule.activation_of(AlpenSpecId::V1), None);
        assert!(!schedule.is_active(AlpenSpecId::V1, 0));
        assert!(!schedule.is_active(AlpenSpecId::V1, u64::MAX));
    }

    #[test]
    fn latest_scheduled_is_the_newest_scheduled_version() {
        assert_eq!(
            AlpenSpecSchedule::genesis().latest_scheduled(),
            AlpenSpecId::V0
        );
        // A scheduled-but-unreached coordinate still counts: the successor is
        // relative to what the schedule knows, not to what is active yet.
        assert_eq!(v1_at(u64::MAX).latest_scheduled(), AlpenSpecId::V1);
    }

    #[test]
    fn schedule_successor_chains_and_errs_past_known_versions() {
        let mut schedule = AlpenSpecSchedule::genesis();
        assert_eq!(schedule.schedule_successor(42), Ok(AlpenSpecId::V1));
        assert_eq!(schedule.activation_of(AlpenSpecId::V1), Some(42));
        // Every known version is scheduled, so the next successor's raw id
        // has no variant.
        assert_eq!(
            schedule.schedule_successor(43),
            Err(AlpenSpecScheduleError::UnknownSuccessor(2))
        );
        assert_eq!(schedule, v1_at(42), "failed call must not mutate");
    }

    /// A coordinate behind the newest scheduled activation would activate
    /// the successor before its predecessor.
    #[test]
    fn schedule_successor_rejects_regressing_coordinates() {
        let mut schedule = v1_at(100);
        assert_eq!(
            schedule.schedule_successor(50),
            Err(AlpenSpecScheduleError::OutOfOrder {
                coord: 50,
                prev: 100
            })
        );
        assert_eq!(schedule, v1_at(100), "failed call must not mutate");
    }

    #[test]
    fn serde_is_the_scheduled_version_map_format() {
        let schedule = v1_at(7);
        let json = serde_json::to_string(&schedule).unwrap();
        assert_eq!(json, r#"{"v0":0,"v1":7}"#);
        let back: AlpenSpecSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(back, schedule);

        // Unscheduled versions are absent, not null.
        let genesis = AlpenSpecSchedule::genesis();
        let json = serde_json::to_string(&genesis).unwrap();
        assert_eq!(json, r#"{"v0":0}"#);
        let back: AlpenSpecSchedule = serde_json::from_str(&json).unwrap();
        assert_eq!(back, genesis);
    }

    #[test]
    fn deserialize_rejects_invalid_schedules() {
        // V0 missing entirely, missing while v1 is scheduled, moved off
        // genesis, or a null coordinate (absence is the only spelling of
        // "unscheduled").
        for json in [
            r#"{}"#,
            r#"{"v1":7}"#,
            r#"{"v0":5}"#,
            r#"{"v0":null}"#,
            r#"{"v0":0,"v1":null}"#,
        ] {
            assert!(
                serde_json::from_str::<AlpenSpecSchedule>(json).is_err(),
                "{json}"
            );
        }
        // A version this binary has no variant for.
        assert!(serde_json::from_str::<AlpenSpecSchedule>(r#"{"v0":0,"v7":9}"#).is_err());
    }
}

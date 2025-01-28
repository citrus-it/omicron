// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use omicron_uuid_kinds::BlueprintUuid;
use omicron_uuid_kinds::CollectionUuid;
use omicron_uuid_kinds::SupportBundleUuid;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use uuid::Uuid;

/// The status of a `region_replacement` background task activation
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct RegionReplacementStatus {
    pub requests_created_ok: Vec<String>,
    pub start_invoked_ok: Vec<String>,
    pub requests_completed_ok: Vec<String>,
    pub errors: Vec<String>,
}

/// The status of a `region_replacement_drive` background task activation
#[derive(Serialize, Deserialize, Default)]
pub struct RegionReplacementDriverStatus {
    pub drive_invoked_ok: Vec<String>,
    pub finish_invoked_ok: Vec<String>,
    pub errors: Vec<String>,
}

/// The status of a `lookup_region_port` background task activation
#[derive(Serialize, Deserialize, Default)]
pub struct LookupRegionPortStatus {
    pub found_port_ok: Vec<String>,
    pub errors: Vec<String>,
}

/// The status of a `region_snapshot_replacement_start` background task
/// activation
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
pub struct RegionSnapshotReplacementStartStatus {
    pub requests_created_ok: Vec<String>,
    pub start_invoked_ok: Vec<String>,
    pub requests_completed_ok: Vec<String>,
    pub errors: Vec<String>,
}

/// The status of a `region_snapshot_replacement_garbage_collect` background
/// task activation
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
pub struct RegionSnapshotReplacementGarbageCollectStatus {
    pub garbage_collect_requested: Vec<String>,
    pub errors: Vec<String>,
}

/// The status of a `region_snapshot_replacement_step` background task
/// activation
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
pub struct RegionSnapshotReplacementStepStatus {
    pub step_records_created_ok: Vec<String>,
    pub step_garbage_collect_invoked_ok: Vec<String>,
    pub step_invoked_ok: Vec<String>,
    pub step_set_volume_deleted_ok: Vec<String>,
    pub errors: Vec<String>,
}

/// The status of a `region_snapshot_replacement_finish` background task activation
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
pub struct RegionSnapshotReplacementFinishStatus {
    pub finish_invoked_ok: Vec<String>,
    pub errors: Vec<String>,
}

/// The status of an `abandoned_vmm_reaper` background task activation.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
pub struct AbandonedVmmReaperStatus {
    pub vmms_found: usize,
    pub sled_reservations_deleted: usize,
    pub vmms_deleted: usize,
    pub vmms_already_deleted: usize,
    pub errors: Vec<String>,
}

/// The status of an `instance_updater` background task activation.
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
pub struct InstanceUpdaterStatus {
    /// if `true`, background instance updates have been explicitly disabled.
    pub disabled: bool,

    /// number of instances found with destroyed active VMMs
    pub destroyed_active_vmms: usize,

    /// number of instances found with failed active VMMs
    pub failed_active_vmms: usize,

    /// number of instances found with terminated active migrations
    pub terminated_active_migrations: usize,

    /// number of update sagas started.
    pub sagas_started: usize,

    /// number of sagas completed successfully
    pub sagas_completed: usize,

    /// errors returned by instance update sagas which failed, and the UUID of
    /// the instance which could not be updated.
    pub saga_errors: Vec<(Option<Uuid>, String)>,

    /// errors which occurred while querying the database for instances in need
    /// of updates.
    pub query_errors: Vec<String>,
}

impl InstanceUpdaterStatus {
    pub fn errors(&self) -> usize {
        self.saga_errors.len() + self.query_errors.len()
    }

    pub fn total_instances_found(&self) -> usize {
        self.destroyed_active_vmms
            + self.failed_active_vmms
            + self.terminated_active_migrations
    }
}

/// The status of an `instance_reincarnation` background task activation.
#[derive(Default, Serialize, Deserialize, Debug)]
pub struct InstanceReincarnationStatus {
    /// If `true`, then instance reincarnation has been explicitly disabled by
    /// the config file.
    pub disabled: bool,
    /// Total number of instances in need of reincarnation on this activation.
    /// This is broken down by the reason that the instance needed
    /// reincarnation.
    pub instances_found: BTreeMap<ReincarnationReason, usize>,
    /// UUIDs of instances reincarnated successfully by this activation.
    pub instances_reincarnated: Vec<ReincarnatableInstance>,
    /// UUIDs of instances which changed state before they could be
    /// reincarnated.
    pub changed_state: Vec<ReincarnatableInstance>,
    /// Any errors that occured while finding instances in need of reincarnation.
    pub errors: Vec<String>,
    /// Errors that occurred while restarting individual instances.
    pub restart_errors: Vec<(ReincarnatableInstance, String)>,
}

impl InstanceReincarnationStatus {
    pub fn total_instances_found(&self) -> usize {
        self.instances_found.values().sum()
    }

    pub fn total_errors(&self) -> usize {
        self.errors.len() + self.restart_errors.len()
    }

    pub fn total_sagas_started(&self) -> usize {
        self.instances_reincarnated.len()
            + self.changed_state.len()
            + self.restart_errors.len()
    }
}

/// Describes a reason why an instance needs reincarnation.
#[derive(
    Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Ord, PartialOrd,
)]
pub enum ReincarnationReason {
    /// The instance is Failed.
    Failed,
    /// A previous instance-start saga for this instance has failed.
    SagaUnwound,
}

impl std::fmt::Display for ReincarnationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Failed => "instance failed",
            Self::SagaUnwound => "start saga failed",
        })
    }
}

/// An instance eligible for reincarnation
#[derive(
    Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize, Ord, PartialOrd,
)]
pub struct ReincarnatableInstance {
    /// The instance's UUID
    pub instance_id: Uuid,
    /// Why the instance required reincarnation
    pub reason: ReincarnationReason,
}

impl std::fmt::Display for ReincarnatableInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { instance_id, reason } = self;
        write!(f, "{instance_id} ({reason})")
    }
}

/// Describes what happened while attempting to clean up Support Bundles.
#[derive(Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct SupportBundleCleanupReport {
    // Responses from Sled Agents
    pub sled_bundles_deleted_ok: usize,
    pub sled_bundles_deleted_not_found: usize,
    pub sled_bundles_delete_failed: usize,

    // Results from updating our database records
    pub db_destroying_bundles_removed: usize,
    pub db_failing_bundles_updated: usize,
}

/// Identifies what we could or could not store within a support bundle.
///
/// This struct will get emitted as part of the background task infrastructure.
#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SupportBundleCollectionReport {
    pub bundle: SupportBundleUuid,

    /// True iff we could list in-service sleds
    pub listed_in_service_sleds: bool,

    /// True iff the bundle was successfully made 'active' in the database.
    pub activated_in_db_ok: bool,
}

impl SupportBundleCollectionReport {
    pub fn new(bundle: SupportBundleUuid) -> Self {
        Self {
            bundle,
            listed_in_service_sleds: false,
            activated_in_db_ok: false,
        }
    }
}

/// The status of an `blueprint_rendezvous` background task activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintRendezvousStatus {
    /// ID of the target blueprint during this activation.
    pub blueprint_id: BlueprintUuid,
    /// ID of the inventory collection used by this activation.
    pub inventory_collection_id: CollectionUuid,
    /// Counts of operations performed.
    pub stats: BlueprintRendezvousStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlueprintRendezvousStats {
    pub debug_dataset: DebugDatasetsRendezvousStats,
    pub crucible_dataset: CrucibleDatasetsRendezvousStats,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize,
)]
pub struct CrucibleDatasetsRendezvousStats {
    /// Number of new Crucible datasets recorded.
    ///
    /// This is a count of in-service Crucible datasets that were also present
    /// in inventory and newly-inserted into `crucible_dataset`.
    pub num_inserted: usize,
    /// Number of Crucible datasets that would have been inserted, except
    /// records for them already existed.
    pub num_already_exist: usize,
    /// Number of Crucible datasets that the current blueprint says are
    /// in-service, but we did not attempt to insert them because they're not
    /// present in the latest inventory collection.
    pub num_not_in_inventory: usize,
}

impl slog::KV for CrucibleDatasetsRendezvousStats {
    fn serialize(
        &self,
        _record: &slog::Record,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        let Self { num_inserted, num_already_exist, num_not_in_inventory } =
            *self;
        serializer.emit_usize("num_inserted".into(), num_inserted)?;
        serializer.emit_usize("num_already_exist".into(), num_already_exist)?;
        serializer
            .emit_usize("num_not_in_inventory".into(), num_not_in_inventory)?;
        Ok(())
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize,
)]
pub struct DebugDatasetsRendezvousStats {
    /// Number of new Debug datasets recorded.
    ///
    /// This is a count of in-service Debug datasets that were also present
    /// in inventory and newly-inserted into `rendezvous_debug_dataset`.
    pub num_inserted: usize,
    /// Number of Debug datasets that would have been inserted, except
    /// records for them already existed.
    pub num_already_exist: usize,
    /// Number of Debug datasets that the current blueprint says are
    /// in-service, but we did not attempt to insert them because they're not
    /// present in the latest inventory collection.
    pub num_not_in_inventory: usize,
    /// Number of Debug datasets that we tombstoned based on their disposition
    /// in the current blueprint being expunged.
    pub num_tombstoned: usize,
    /// Number of Debug datasets that we would have tombstoned, except they were
    /// already tombstoned or deleted.
    pub num_already_tombstoned: usize,
}

impl slog::KV for DebugDatasetsRendezvousStats {
    fn serialize(
        &self,
        _record: &slog::Record,
        serializer: &mut dyn slog::Serializer,
    ) -> slog::Result {
        let Self {
            num_inserted,
            num_already_exist,
            num_not_in_inventory,
            num_tombstoned,
            num_already_tombstoned,
        } = *self;
        serializer.emit_usize("num_inserted".into(), num_inserted)?;
        serializer.emit_usize("num_already_exist".into(), num_already_exist)?;
        serializer
            .emit_usize("num_not_in_inventory".into(), num_not_in_inventory)?;
        serializer.emit_usize("num_tombstoned".into(), num_tombstoned)?;
        serializer.emit_usize(
            "num_already_tombstoned".into(),
            num_already_tombstoned,
        )?;
        Ok(())
    }
}

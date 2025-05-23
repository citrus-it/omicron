// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Implementation of queries for provisioning regions.

use crate::db::column_walker::AllColumnsOf;
use crate::db::model::{CrucibleDataset, Region, RegionReservationPercent};
use crate::db::raw_query_builder::{QueryBuilder, TypedSqlQuery};
use crate::db::true_or_cast_error::matches_sentinel;
use const_format::concatcp;
use diesel::pg::Pg;
use diesel::result::Error as DieselError;
use diesel::sql_types;
use nexus_config::RegionAllocationStrategy;
use nexus_db_schema::enums::RegionReservationPercentEnum;
use nexus_db_schema::schema;
use omicron_common::api::external;
use omicron_uuid_kinds::GenericUuid;
use omicron_uuid_kinds::VolumeUuid;

type AllColumnsOfRegion = AllColumnsOf<schema::region::table>;
type AllColumnsOfCrucibleDataset =
    AllColumnsOf<schema::crucible_dataset::table>;

const NOT_ENOUGH_DATASETS_SENTINEL: &'static str = "Not enough datasets";
const NOT_ENOUGH_ZPOOL_SPACE_SENTINEL: &'static str = "Not enough space";
const NOT_ENOUGH_UNIQUE_ZPOOLS_SENTINEL: &'static str =
    "Not enough unique zpools selected";

/// Translates a generic pool error to an external error based
/// on messages which may be emitted during region provisioning.
pub fn from_diesel(e: DieselError) -> external::Error {
    let sentinels = [
        NOT_ENOUGH_DATASETS_SENTINEL,
        NOT_ENOUGH_ZPOOL_SPACE_SENTINEL,
        NOT_ENOUGH_UNIQUE_ZPOOLS_SENTINEL,
    ];
    if let Some(sentinel) = matches_sentinel(&e, &sentinels) {
        let external_message = "Not enough storage";
        match sentinel {
            NOT_ENOUGH_DATASETS_SENTINEL => {
                return external::Error::insufficient_capacity(
                    external_message,
                    "Not enough datasets to allocate disks",
                );
            }
            NOT_ENOUGH_ZPOOL_SPACE_SENTINEL => {
                return external::Error::insufficient_capacity(
                    external_message,
                    "Not enough zpool space to allocate disks. There may not \
                    be enough disks with space for the requested region. You \
                    may also see this if your rack is in a degraded state, or \
                    you're running the default multi-rack topology \
                    configuration in a 1-sled development environment.",
                );
            }
            NOT_ENOUGH_UNIQUE_ZPOOLS_SENTINEL => {
                return external::Error::insufficient_capacity(
                    external_message,
                    "Not enough unique zpools selected while allocating disks",
                );
            }
            // Fall-through to the generic error conversion.
            _ => {}
        }
    }

    nexus_db_errors::public_error_from_diesel(
        e,
        nexus_db_errors::ErrorHandler::Server,
    )
}

type SelectableSql<T> = <
    <T as diesel::Selectable<Pg>>::SelectExpression as diesel::Expression
>::SqlType;

/// Parameters for the region(s) being allocated
#[derive(Debug, Clone, Copy)]
pub struct RegionParameters {
    pub block_size: u64,
    pub blocks_per_extent: u64,
    pub extent_count: u64,

    /// True if the region will be filled with a Clone operation and is meant to
    /// be read-only.
    pub read_only: bool,
}

type AllocationQuery =
    TypedSqlQuery<(SelectableSql<CrucibleDataset>, SelectableSql<Region>)>;

/// Currently the largest region that can be allocated matches the largest disk
/// that can be requested, but separate this constant so that when
/// MAX_DISK_SIZE_BYTES is increased the region allocation query will still use
/// this as a maximum size.
pub const MAX_REGION_SIZE_BYTES: u64 = 1098437885952; // 1023 * (1 << 30);

#[derive(Debug)]
pub enum AllocationQueryError {
    /// Region size multiplication overflowed u64
    RegionSizeOverflow,

    /// Requested region size larger than maximum
    RequestedRegionOverMaxSize { request: u64, maximum: u64 },

    /// Requested size not divisible by reservation factor
    RequestedRegionNotDivisibleByFactor { request: i64, factor: i64 },

    /// Adding the overhead to the requested size overflowed
    RequestedRegionOverheadOverflow { request: i64, overhead: i64 },

    /// Converting from u64 to i64 truncated
    RequestedRegionSizeTruncated { request: u64, e: String },
}

impl From<AllocationQueryError> for external::Error {
    fn from(e: AllocationQueryError) -> external::Error {
        match e {
            AllocationQueryError::RegionSizeOverflow => {
                external::Error::invalid_value(
                    "region allocation",
                    "region size overflowed u64",
                )
            }

            AllocationQueryError::RequestedRegionOverMaxSize {
                request,
                maximum,
            } => external::Error::invalid_value(
                "region allocation",
                format!("region size {request} over maximum {maximum}"),
            ),

            AllocationQueryError::RequestedRegionNotDivisibleByFactor {
                request,
                factor,
            } => external::Error::invalid_value(
                "region allocation",
                format!("region size {request} not divisible by {factor}"),
            ),

            AllocationQueryError::RequestedRegionOverheadOverflow {
                request,
                overhead,
            } => external::Error::invalid_value(
                "region allocation",
                format!(
                    "adding {overhead} to region size {request} overflowed"
                ),
            ),

            AllocationQueryError::RequestedRegionSizeTruncated {
                request,
                e,
            } => external::Error::internal_error(&format!(
                "converting {request} to i64 failed! {e}"
            )),
        }
    }
}

/// For a given volume, idempotently allocate enough regions (according to some
/// allocation strategy) to meet some redundancy level. This should only be used
/// for the region set that is in the top level of the Volume (not the deeper
/// layers of the hierarchy). If that volume has region snapshots in the region
/// set, a `snapshot_id` should be supplied matching those entries.
///
/// Depending on the call site, it may not safe for multiple callers to call
/// this function concurrently for the same volume id. Care is required!
pub fn allocation_query(
    volume_id: VolumeUuid,
    snapshot_id: Option<uuid::Uuid>,
    params: RegionParameters,
    allocation_strategy: &RegionAllocationStrategy,
    redundancy: usize,
) -> Result<AllocationQuery, AllocationQueryError> {
    let (seed, distinct_sleds) = {
        let (input_seed, distinct_sleds) = match allocation_strategy {
            RegionAllocationStrategy::Random { seed } => (seed, false),
            RegionAllocationStrategy::RandomWithDistinctSleds { seed } => {
                (seed, true)
            }
        };
        (
            input_seed.map_or_else(
                || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                },
                |seed| u128::from(seed),
            ),
            distinct_sleds,
        )
    };

    let seed = seed.to_le_bytes().to_vec();

    // Ensure that the multiplication doesn't overflow.
    let requested_size: u64 = params
        .block_size
        .checked_mul(params.blocks_per_extent)
        .ok_or(AllocationQueryError::RegionSizeOverflow)?
        .checked_mul(params.extent_count)
        .ok_or(AllocationQueryError::RegionSizeOverflow)?;

    if requested_size > MAX_REGION_SIZE_BYTES {
        return Err(AllocationQueryError::RequestedRegionOverMaxSize {
            request: requested_size,
            maximum: MAX_REGION_SIZE_BYTES,
        });
    }

    // After the above check, cast from u64 to i64. The value is low enough
    // (after the check above) that try_into should always return Ok.
    let requested_size: i64 = match requested_size.try_into() {
        Ok(v) => v,
        Err(e) => {
            return Err(AllocationQueryError::RequestedRegionSizeTruncated {
                request: requested_size,
                e: e.to_string(),
            });
        }
    };

    let reservation_percent = RegionReservationPercent::TwentyFive;

    let size_delta: i64 = match reservation_percent {
        RegionReservationPercent::TwentyFive => {
            // Check first that the requested region size is divisible by this.
            // This should basically never fail because all block sizes are
            // divisible by 4.
            if requested_size % 4 != 0 {
                return Err(
                    AllocationQueryError::RequestedRegionNotDivisibleByFactor {
                        request: requested_size,
                        factor: 4,
                    },
                );
            }

            let overhead: i64 = requested_size.checked_div(4).ok_or(
                AllocationQueryError::RequestedRegionNotDivisibleByFactor {
                    request: requested_size,
                    factor: 4,
                },
            )?;

            requested_size.checked_add(overhead).ok_or(
                AllocationQueryError::RequestedRegionOverheadOverflow {
                    request: requested_size,
                    overhead,
                },
            )?
        }
    };

    let redundancy: i64 = i64::try_from(redundancy).unwrap();

    let mut builder = QueryBuilder::new();

    builder.sql(
    // Find all old regions associated with a particular volume
"WITH
  old_regions AS (
    SELECT ").sql(AllColumnsOfRegion::with_prefix("region")).sql("
    FROM region WHERE (region.volume_id = ").param().sql(")),")
    .bind::<sql_types::Uuid, _>(*volume_id.as_untyped_uuid())

    // Calculates the old size being used by zpools under consideration as targets for region
    // allocation.
    .sql("
  old_zpool_usage AS (
    SELECT
      crucible_dataset.pool_id,
      sum(crucible_dataset.size_used) AS size_used
    FROM crucible_dataset
    WHERE
      ((crucible_dataset.size_used IS NOT NULL) AND (crucible_dataset.time_deleted IS NULL))
    GROUP BY crucible_dataset.pool_id),");

    if let Some(snapshot_id) = snapshot_id {
        // Any zpool already have this volume's existing regions, or host the
        // snapshot volume's regions?
        builder.sql("
      existing_zpools AS ((
        SELECT
          crucible_dataset.pool_id
        FROM
          crucible_dataset INNER JOIN old_regions ON (old_regions.dataset_id = crucible_dataset.id)
      ) UNION (
       select crucible_dataset.pool_id from
 crucible_dataset inner join region_snapshot on (region_snapshot.dataset_id = crucible_dataset.id)
 where region_snapshot.snapshot_id = ").param().sql(")),")
        .bind::<sql_types::Uuid, _>(snapshot_id);
    } else {
        // Any zpool already have this volume's existing regions?
        builder.sql("
      existing_zpools AS (
        SELECT
          crucible_dataset.pool_id
        FROM
          crucible_dataset INNER JOIN old_regions ON (old_regions.dataset_id = crucible_dataset.id)
      ),");
    }

    // If `distinct_sleds` is selected, then take note of the sleds used by
    // existing allocations, and filter those out later. This step is required
    // when taking an existing allocation of regions and increasing the
    // redundancy in order to _not_ allocate to sleds already used.

    if distinct_sleds {
        builder.sql(
            "
        existing_sleds AS (
          SELECT
            zpool.sled_id as id
          FROM
            zpool
          WHERE
            zpool.id = ANY(SELECT pool_id FROM existing_zpools)
        ),",
        );
    }

    // Identifies zpools with enough space for region allocation, that are not
    // currently used by this Volume's existing regions.
    //
    // NOTE: 'distinct_sleds' changes the format of the underlying SQL query, as it uses
    // distinct bind parameters depending on the conditional branch.
    builder.sql(
        "
  candidate_zpools AS (",
    );
    if distinct_sleds {
        builder.sql("SELECT DISTINCT ON (zpool.sled_id) ")
    } else {
        builder.sql("SELECT ")
    };
    builder.sql("
        old_zpool_usage.pool_id
    FROM
        old_zpool_usage
        INNER JOIN
        (zpool INNER JOIN sled ON (zpool.sled_id = sled.id)) ON (zpool.id = old_zpool_usage.pool_id)
        INNER JOIN
        physical_disk ON (zpool.physical_disk_id = physical_disk.id)
        INNER JOIN
        crucible_dataset ON (crucible_dataset.pool_id = zpool.id)
    WHERE (
      (old_zpool_usage.size_used + ").param().sql(" + zpool.control_plane_storage_buffer) <=
         (SELECT total_size FROM omicron.public.inv_zpool WHERE
          inv_zpool.id = old_zpool_usage.pool_id
          ORDER BY inv_zpool.time_collected DESC LIMIT 1)
      AND sled.sled_policy = 'in_service'
      AND sled.sled_state = 'active'
      AND physical_disk.disk_policy = 'in_service'
      AND physical_disk.disk_state = 'active'
      AND NOT(zpool.id = ANY(SELECT existing_zpools.pool_id FROM existing_zpools))
      AND (crucible_dataset.time_deleted is NULL)
      AND (crucible_dataset.no_provision = false)
    "
    ).bind::<sql_types::BigInt, _>(size_delta);

    if distinct_sleds {
        builder
            .sql("AND NOT(sled.id = ANY(SELECT existing_sleds.id FROM existing_sleds)))
            ORDER BY zpool.sled_id, md5((CAST(zpool.id as BYTEA) || ")
            .param()
            .sql("))")
            .bind::<sql_types::Bytea, _>(seed.clone())
    } else {
        builder.sql(")")
    }
    .sql("),");

    // Find datasets which could be used for provisioning regions.
    //
    // We select only one dataset from each zpool.
    builder.sql("
  candidate_datasets AS (
    SELECT DISTINCT ON (crucible_dataset.pool_id)
      crucible_dataset.id,
      crucible_dataset.pool_id
    FROM (crucible_dataset INNER JOIN candidate_zpools ON (crucible_dataset.pool_id = candidate_zpools.pool_id))
    WHERE (crucible_dataset.time_deleted IS NULL) AND (crucible_dataset.no_provision = false)
    ORDER BY crucible_dataset.pool_id, md5((CAST(crucible_dataset.id as BYTEA) || ").param().sql("))
  ),")
    .bind::<sql_types::Bytea, _>(seed.clone())

    // We order by md5 to shuffle the ordering of the datasets.
    // md5 has a uniform output distribution so it does the job.
    .sql("
  shuffled_candidate_datasets AS (
    SELECT
      candidate_datasets.id,
      candidate_datasets.pool_id
    FROM candidate_datasets
    ORDER BY md5((CAST(candidate_datasets.id as BYTEA) || ").param().sql(")) LIMIT ").param().sql("
  ),")
    .bind::<sql_types::Bytea, _>(seed)
    .bind::<sql_types::BigInt, _>(redundancy)

    // Create the regions-to-be-inserted for the volume.
    .sql("
  candidate_regions AS (
    SELECT
      gen_random_uuid() AS id,
      now() AS time_created,
      now() AS time_modified,
      shuffled_candidate_datasets.id AS dataset_id,
      ").param().sql(" AS volume_id,
      ").param().sql(" AS block_size,
      ").param().sql(" AS blocks_per_extent,
      ").param().sql(" AS extent_count,
      NULL AS port,
      ").param().sql(" AS read_only,
      FALSE as deleting,
      ").param().sql(" AS reservation_percent
    FROM shuffled_candidate_datasets")
  // Only select the *additional* number of candidate regions for the required
  // redundancy level
  .sql("
    LIMIT (").param().sql(" - (
      SELECT COUNT(*) FROM old_regions
    ))
  ),")
    .bind::<sql_types::Uuid, _>(*volume_id.as_untyped_uuid())
    .bind::<sql_types::BigInt, _>(params.block_size as i64)
    .bind::<sql_types::BigInt, _>(params.blocks_per_extent as i64)
    .bind::<sql_types::BigInt, _>(params.extent_count as i64)
    .bind::<sql_types::Bool, _>(params.read_only)
    .bind::<RegionReservationPercentEnum, _>(reservation_percent)
    .bind::<sql_types::BigInt, _>(redundancy)

    // A subquery which summarizes the changes we intend to make, showing:
    //
    // 1. Which datasets will have size adjustments
    // 2. Which pools those datasets belong to
    // 3. The delta in size-used
    .sql("
  proposed_dataset_changes AS (
    SELECT
      candidate_regions.dataset_id AS id,
      crucible_dataset.pool_id AS pool_id,
      ").param().sql(" AS size_used_delta
    FROM (candidate_regions INNER JOIN crucible_dataset ON (crucible_dataset.id = candidate_regions.dataset_id))
  ),")
    .bind::<sql_types::BigInt, _>(size_delta)

    // Confirms whether or not the insertion and updates should
    // occur.
    //
    // This subquery additionally exits the CTE early with an error if either:
    // 1. Not enough datasets exist to provision regions with our required
    //    redundancy, or
    // 2. Not enough space exists on zpools to perform the provisioning.
    //
    // We want to ensure that we do not allocate on two datasets in the same
    // zpool, for two reasons
    // - Data redundancy: If a drive fails it should only take one of the 3
    //   regions with it
    // - Risk of overallocation: We only check that each zpool as enough
    //   room for one region, so we should not allocate more than one region
    //   to it.
    //
    // Selecting two datasets on the same zpool will not initially be
    // possible, as at the time of writing each zpool only has one dataset.
    // Additionally, provide a configuration option ("distinct_sleds") to modify
    // the allocation strategy to select from 3 distinct sleds, removing the
    // possibility entirely. But, if we introduce a change that adds another
    // crucible dataset to zpools before we improve the allocation strategy,
    // this check will make sure we don't violate drive redundancy, and generate
    // an error instead.
    .sql("
  do_insert AS (
    SELECT (((")
    // There's regions not allocated yet
    .sql("
        ((SELECT COUNT(*) FROM old_regions LIMIT 1) < ").param().sql(") AND")
    // Enough filtered candidate zpools + existing zpools to meet redundancy
    .sql("
        CAST(IF(((
          (
            (SELECT COUNT(*) FROM candidate_zpools LIMIT 1) +
            (SELECT COUNT(*) FROM existing_zpools LIMIT 1)
          )
        ) >= ").param().sql(concatcp!("), 'TRUE', '", NOT_ENOUGH_ZPOOL_SPACE_SENTINEL, "') AS BOOL)) AND"))
    // Enough candidate regions + existing regions to meet redundancy
    .sql("
        CAST(IF(((
          (
            (SELECT COUNT(*) FROM candidate_regions LIMIT 1) +
            (SELECT COUNT(*) FROM old_regions LIMIT 1)
          )
        ) >= ").param().sql(concatcp!("), 'TRUE', '", NOT_ENOUGH_DATASETS_SENTINEL, "') AS BOOL)) AND"))
    // Enough unique zpools (looking at both existing and new) to meet redundancy
    .sql("
        CAST(IF(((
         (
           SELECT
             COUNT(DISTINCT pool_id)
           FROM
            (
              (
               SELECT
                 crucible_dataset.pool_id
               FROM
                 candidate_regions
                   INNER JOIN crucible_dataset ON (candidate_regions.dataset_id = crucible_dataset.id)
              )
              UNION
              (
               SELECT
                 crucible_dataset.pool_id
               FROM
                 old_regions
                   INNER JOIN crucible_dataset ON (old_regions.dataset_id = crucible_dataset.id)
              )
            )
           LIMIT 1
         )
        ) >= ").param().sql(concatcp!("), 'TRUE', '", NOT_ENOUGH_UNIQUE_ZPOOLS_SENTINEL, "') AS BOOL)
     ) AS insert
   ),"))
    .bind::<sql_types::BigInt, _>(redundancy)
    .bind::<sql_types::BigInt, _>(redundancy)
    .bind::<sql_types::BigInt, _>(redundancy)
    .bind::<sql_types::BigInt, _>(redundancy)

    .sql("
  inserted_regions AS (
    INSERT INTO region
      (id, time_created, time_modified, dataset_id, volume_id, block_size, blocks_per_extent, extent_count, port, read_only, deleting, reservation_percent)
    SELECT ").sql(AllColumnsOfRegion::with_prefix("candidate_regions")).sql("
    FROM candidate_regions
    WHERE
      (SELECT do_insert.insert FROM do_insert LIMIT 1)
    RETURNING ").sql(AllColumnsOfRegion::with_prefix("region")).sql("
  ),
  updated_datasets AS (
    UPDATE crucible_dataset SET
      size_used = (crucible_dataset.size_used + (SELECT proposed_dataset_changes.size_used_delta FROM proposed_dataset_changes WHERE (proposed_dataset_changes.id = crucible_dataset.id) LIMIT 1))
    WHERE (
      (crucible_dataset.id = ANY(SELECT proposed_dataset_changes.id FROM proposed_dataset_changes)) AND
      (SELECT do_insert.insert FROM do_insert LIMIT 1))
    RETURNING ").sql(AllColumnsOfCrucibleDataset::with_prefix("crucible_dataset")).sql("
  )
(
  SELECT ")
    .sql(AllColumnsOfCrucibleDataset::with_prefix("crucible_dataset"))
    .sql(", ")
    .sql(AllColumnsOfRegion::with_prefix("old_regions")).sql("
  FROM
    (old_regions INNER JOIN crucible_dataset ON (old_regions.dataset_id = crucible_dataset.id))
)
UNION
(
  SELECT ")
    .sql(AllColumnsOfCrucibleDataset::with_prefix("updated_datasets"))
    .sql(", ")
    .sql(AllColumnsOfRegion::with_prefix("inserted_regions")).sql("
  FROM (inserted_regions INNER JOIN updated_datasets ON (inserted_regions.dataset_id = updated_datasets.id))
)"
    );

    Ok(builder.query())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::db::datastore::REGION_REDUNDANCY_THRESHOLD;
    use crate::db::explain::ExplainableAsync;
    use crate::db::pub_test_utils::TestDatabase;
    use crate::db::raw_query_builder::expectorate_query_contents;
    use omicron_test_utils::dev;
    use uuid::Uuid;

    // This test is a bit of a "change detector", but it's here to help with
    // debugging too. If you change this query, it can be useful to see exactly
    // how the output SQL has been altered.
    #[tokio::test]
    async fn expectorate_query() {
        let volume_id = VolumeUuid::nil();
        let params = RegionParameters {
            block_size: 512,
            blocks_per_extent: 4,
            extent_count: 8,
            read_only: false,
        };

        // Start with snapshot_id = None

        let snapshot_id = None;

        // First structure: "RandomWithDistinctSleds"

        let region_allocate = allocation_query(
            volume_id,
            snapshot_id,
            params,
            &RegionAllocationStrategy::RandomWithDistinctSleds {
                seed: Some(1),
            },
            REGION_REDUNDANCY_THRESHOLD,
        )
        .unwrap();

        expectorate_query_contents(
            &region_allocate,
            "tests/output/region_allocate_distinct_sleds.sql",
        )
        .await;

        // Second structure: "Random"

        let region_allocate = allocation_query(
            volume_id,
            snapshot_id,
            params,
            &RegionAllocationStrategy::Random { seed: Some(1) },
            REGION_REDUNDANCY_THRESHOLD,
        )
        .unwrap();
        expectorate_query_contents(
            &region_allocate,
            "tests/output/region_allocate_random_sleds.sql",
        )
        .await;

        // Next, put a value in for snapshot_id

        let snapshot_id = Some(Uuid::new_v4());

        // First structure: "RandomWithDistinctSleds"

        let region_allocate = allocation_query(
            volume_id,
            snapshot_id,
            params,
            &RegionAllocationStrategy::RandomWithDistinctSleds {
                seed: Some(1),
            },
            REGION_REDUNDANCY_THRESHOLD,
        )
        .unwrap();
        expectorate_query_contents(
            &region_allocate,
            "tests/output/region_allocate_with_snapshot_distinct_sleds.sql",
        )
        .await;

        // Second structure: "Random"

        let region_allocate = allocation_query(
            volume_id,
            snapshot_id,
            params,
            &RegionAllocationStrategy::Random { seed: Some(1) },
            REGION_REDUNDANCY_THRESHOLD,
        )
        .unwrap();
        expectorate_query_contents(
            &region_allocate,
            "tests/output/region_allocate_with_snapshot_random_sleds.sql",
        )
        .await;
    }

    // Explain the possible forms of the SQL query to ensure that it
    // creates a valid SQL string.
    #[tokio::test]
    async fn explainable() {
        let logctx = dev::test_setup_log("explainable");
        let db = TestDatabase::new_with_pool(&logctx.log).await;
        let pool = db.pool();
        let conn = pool.claim().await.unwrap();

        let volume_id = VolumeUuid::new_v4();
        let params = RegionParameters {
            block_size: 512,
            blocks_per_extent: 4,
            extent_count: 8,
            read_only: false,
        };

        // First structure: Explain the query with "RandomWithDistinctSleds"

        let region_allocate = allocation_query(
            volume_id,
            None,
            params,
            &RegionAllocationStrategy::RandomWithDistinctSleds { seed: None },
            REGION_REDUNDANCY_THRESHOLD,
        )
        .unwrap();
        let _ = region_allocate
            .explain_async(&conn)
            .await
            .expect("Failed to explain query - is it valid SQL?");

        // Second structure: Explain the query with "Random"

        let region_allocate = allocation_query(
            volume_id,
            None,
            params,
            &RegionAllocationStrategy::Random { seed: None },
            REGION_REDUNDANCY_THRESHOLD,
        )
        .unwrap();
        let _ = region_allocate
            .explain_async(&conn)
            .await
            .expect("Failed to explain query - is it valid SQL?");

        db.terminate().await;
        logctx.cleanup_successful();
    }

    #[test]
    fn allocation_query_region_size_overflow() {
        let volume_id = VolumeUuid::nil();
        let snapshot_id = None;

        let params = RegionParameters {
            block_size: 512,
            blocks_per_extent: 4294967296,
            extent_count: 8388609, // should cause an overflow!
            read_only: false,
        };

        let Err(e) = allocation_query(
            volume_id,
            snapshot_id,
            params,
            &RegionAllocationStrategy::RandomWithDistinctSleds {
                seed: Some(1),
            },
            REGION_REDUNDANCY_THRESHOLD,
        ) else {
            panic!("expected error");
        };

        assert!(matches!(e, AllocationQueryError::RegionSizeOverflow));
    }

    #[test]
    fn allocation_query_region_size_too_large() {
        let volume_id = VolumeUuid::nil();
        let snapshot_id = None;

        let params = RegionParameters {
            block_size: 512,
            blocks_per_extent: 8388608, // 2^32 / 512
            extent_count: 256,          // 255 would be ok, 256 is too large
            read_only: false,
        };

        let Err(e) = allocation_query(
            volume_id,
            snapshot_id,
            params,
            &RegionAllocationStrategy::RandomWithDistinctSleds {
                seed: Some(1),
            },
            REGION_REDUNDANCY_THRESHOLD,
        ) else {
            panic!("expected error!");
        };

        assert!(matches!(
            e,
            AllocationQueryError::RequestedRegionOverMaxSize {
                request: 1099511627776u64,
                maximum: MAX_REGION_SIZE_BYTES,
            }
        ));
    }
}

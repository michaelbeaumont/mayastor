pub mod common;

use once_cell::sync::OnceCell;

use common::{bdev_io, compose::MayastorTest};

use io_engine::{
    bdev::device_open,
    core::{
        CloneParams,
        CloneXattrs,
        LogicalVolume,
        MayastorCliArgs,
        SnapshotParams,
        SnapshotXattrs,
        UntypedBdev,
    },
    lvs::{Lvol, Lvs, LvsLvol},
    pool_backend::PoolArgs,
};

use chrono::Utc;
use io_engine::core::{
    snapshot::VolumeSnapshotDescriptor,
    SnapshotDescriptor,
    SnapshotOps,
};
use log::info;
use std::{convert::TryFrom, str};
use uuid::Uuid;
static MAYASTOR: OnceCell<MayastorTest> = OnceCell::new();

/// Get the global Mayastor test suite instance.
fn get_ms() -> &'static MayastorTest<'static> {
    MAYASTOR.get_or_init(|| MayastorTest::new(MayastorCliArgs::default()))
}

/// Must be called only in Mayastor context !s
async fn create_test_pool(pool_name: &str, disk: String) -> Lvs {
    Lvs::create_or_import(PoolArgs {
        name: pool_name.to_string(),
        disks: vec![disk],
        uuid: None,
    })
    .await
    .expect("Failed to create test pool");

    Lvs::lookup(pool_name).expect("Failed to lookup test pool")
}

async fn find_snapshot_device(name: &String) -> Option<Lvol> {
    let bdev = UntypedBdev::bdev_first().expect("Failed to enumerate devices");

    let mut devices = bdev
        .into_iter()
        .filter(|b| b.driver() == "lvol" && b.name() == name)
        .map(|b| Lvol::try_from(b).expect("Can't create Lvol from device"))
        .collect::<Vec<Lvol>>();

    assert!(
        devices.len() <= 1,
        "Found more than one snapshot with name '{}'",
        name
    );

    match devices.len() {
        0 => None,
        _ => Some(devices.remove(0)),
    }
}

async fn check_snapshot(params: SnapshotParams) {
    let attrs = [
        (SnapshotXattrs::TxId, params.txn_id().unwrap()),
        (SnapshotXattrs::EntityId, params.entity_id().unwrap()),
        (SnapshotXattrs::ParentId, params.parent_id().unwrap()),
        (
            SnapshotXattrs::SnapshotUuid,
            params.snapshot_uuid().unwrap(),
        ),
    ];

    // Locate snapshot device.
    let lvol = find_snapshot_device(&params.name().unwrap())
        .await
        .expect("Can't find target snapshot device");

    for (attr_name, attr_value) in attrs {
        let v = Lvol::get_blob_xattr(&lvol, attr_name.name())
            .expect("Failed to get snapshot attribute");
        assert_eq!(v, attr_value, "Snapshot attr doesn't match");
    }
}

async fn check_clone(clone_lvol: Lvol, params: CloneParams) {
    let attrs = [
        (CloneXattrs::SourceUuid, params.source_uuid().unwrap()),
        (
            CloneXattrs::CloneCreateTime,
            params.clone_create_time().unwrap(),
        ),
        (CloneXattrs::CloneUuid, params.clone_uuid().unwrap()),
    ];
    for (attr_name, attr_value) in attrs {
        let v = Lvol::get_blob_xattr(&clone_lvol, attr_name.name())
            .expect("Failed to get clone attribute");
        assert_eq!(v, attr_value, "clone attr doesn't match");
    }
}

async fn clean_snapshots(snapshot_list: Vec<VolumeSnapshotDescriptor>) {
    for snapshot in snapshot_list {
        let snap_lvol = UntypedBdev::lookup_by_uuid_str(
            &snapshot
                .snapshot_params()
                .snapshot_uuid()
                .unwrap_or_default(),
        )
        .map(|b| Lvol::try_from(b).expect("Can't create Lvol from device"))
        .unwrap();
        snap_lvol
            .destroy()
            .await
            .expect("Failed to destroy Snapshot");
    }
}

fn check_snapshot_descriptor(
    params: &SnapshotParams,
    descr: &VolumeSnapshotDescriptor,
) {
    let snap_params = descr.snapshot_params();

    assert_eq!(
        params.name().unwrap(),
        snap_params
            .name()
            .expect("Snapshot descriptor has no snapshot name"),
        "Snapshot name doesn't match"
    );

    assert_eq!(
        params.parent_id().unwrap(),
        snap_params
            .parent_id()
            .expect("Snapshot descriptor has no parent ID"),
        "Snapshot parent ID doesn't match"
    );

    assert_eq!(
        params.entity_id().unwrap(),
        snap_params
            .entity_id()
            .expect("Snapshot descriptor has no entity ID"),
        "Snapshot entity ID doesn't match"
    );

    assert_eq!(
        params.snapshot_uuid().unwrap(),
        snap_params
            .snapshot_uuid()
            .expect("Snapshot descriptor has no snapshot UUID"),
        "Snapshot UUID doesn't match"
    );

    assert_eq!(
        params.txn_id().unwrap(),
        snap_params
            .txn_id()
            .expect("Snapshot descriptor has no txn ID"),
        "Snapshot txn ID doesn't match"
    );
    assert_eq!(
        params.create_time().unwrap(),
        snap_params
            .create_time()
            .expect("Snapshot descriptor has no snapshot createtime"),
        "Snapshot CreateTime doesn't match"
    );
}

#[tokio::test]
async fn test_lvol_bdev_snapshot() {
    let ms = get_ms();

    ms.spawn(async move {
        // Create a pool and lvol.
        let pool =
            create_test_pool("pool1", "malloc:///disk0?size_mb=64".to_string())
                .await;
        let lvol = pool
            .create_lvol(
                "lvol1",
                32 * 1024 * 1024,
                Some(&Uuid::new_v4().to_string()),
                false,
            )
            .await
            .expect("Failed to create test lvol");

        // Create a snapshot via lvol object.
        let entity_id = String::from("e1");
        let parent_id = String::from("p1");
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("snap11");
        let snap_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name.clone()),
            Some(snap_uuid.clone()),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        // Check blob attributes for snapshot.
        check_snapshot(snapshot_params).await;

        // Check the device UUID mathches requested snapshot UUID.
        let lvol = find_snapshot_device(&snap_name)
            .await
            .expect("Can't find target snapshot device");
        assert_eq!(snap_uuid, lvol.uuid(), "Snapshot UUID doesn't match");
        let snapshot_list = Lvol::list_all_snapshots();
        clean_snapshots(snapshot_list).await;
    })
    .await;
}

#[tokio::test]
async fn test_lvol_handle_snapshot() {
    let ms = get_ms();

    ms.spawn(async move {
        // Create a pool and lvol.
        let pool =
            create_test_pool("pool2", "malloc:///disk1?size_mb=64".to_string())
                .await;

        pool.create_lvol(
            "lvol2",
            32 * 1024 * 1024,
            Some(&Uuid::new_v4().to_string()),
            false,
        )
        .await
        .expect("Failed to create test lvol");

        // Create a snapshot using device handle directly.
        let descr =
            device_open("lvol2", false).expect("Failed to open volume device");
        let handle = descr
            .into_handle()
            .expect("Failed to get I/O handle for volume device");

        let entity_id = String::from("e1");
        let parent_id = String::from("p1");
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("snap21");
        let snap_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snap_uuid),
            Some(Utc::now().to_string()),
        );

        handle
            .create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create snapshot");

        check_snapshot(snapshot_params).await;
        let snapshot_list = Lvol::list_all_snapshots();
        clean_snapshots(snapshot_list).await;
    })
    .await;
}

#[tokio::test]
async fn test_lvol_list_snapshot() {
    let ms = get_ms();

    ms.spawn(async move {
        // Create a pool and lvol.
        let pool =
            create_test_pool("pool3", "malloc:///disk3?size_mb=64".to_string())
                .await;
        let lvol = pool
            .create_lvol(
                "lvol3",
                32 * 1024 * 1024,
                Some(&Uuid::new_v4().to_string()),
                false,
            )
            .await
            .expect("Failed to create test lvol");

        // Create a snapshot-1 via lvol object.
        let entity_id = String::from("e13");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("snap13");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        // Create a snapshot-1 via lvol object.
        let entity_id = String::from("e14");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("snap14");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        let snapshot_list = lvol.list_snapshot_by_source_uuid();
        info!("Total number of snapshots: {}", snapshot_list.len());
        assert_eq!(2, snapshot_list.len(), "Snapshot Count not matched!!");
        clean_snapshots(snapshot_list).await;
    })
    .await;
}

#[tokio::test]
async fn test_list_all_snapshots() {
    let ms = get_ms();

    ms.spawn(async move {
        // Create a pool and lvol.
        let pool = create_test_pool(
            "pool4",
            "malloc:///disk4?size_mb=128".to_string(),
        )
        .await;
        let lvol = pool
            .create_lvol(
                "lvol4",
                32 * 1024 * 1024,
                Some(&Uuid::new_v4().to_string()),
                false,
            )
            .await
            .expect("Failed to create test lvol");

        // Create a snapshot-1 via lvol object.
        let entity_id = String::from("lvol4_e1");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("lvol4_snap1");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        // Create a snapshot-1 via lvol object.
        let entity_id = String::from("lvol4_e2");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("lvol4_snap2");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        // create another lvol and snapshots
        let lvol = pool
            .create_lvol(
                "lvol5",
                32 * 1024 * 1024,
                Some(&Uuid::new_v4().to_string()),
                false,
            )
            .await
            .expect("Failed to create test lvol");

        // Create a snapshot-1 via lvol object.
        let entity_id = String::from("lvol5_e1");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("lvol5_snap1");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        // Create a snapshot-1 via lvol object.
        let entity_id = String::from("lvol5_e2");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("lvol5_snap2");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        let snapshot_list = Lvol::list_all_snapshots();
        info!("Total number of snapshots: {}", snapshot_list.len());
        assert_eq!(4, snapshot_list.len(), "Snapshot Count not matched!!");
        clean_snapshots(snapshot_list).await;
    })
    .await;
}

#[tokio::test]
async fn test_list_pool_snapshots() {
    let ms = get_ms();

    ms.spawn(async move {
        // Create a pool and lvol.
        let pool =
            create_test_pool("pool6", "malloc:///disk6?size_mb=32".to_string())
                .await;

        let lvol = pool
            .create_lvol(
                "volume6",
                16 * 1024 * 1024,
                Some(&Uuid::new_v4().to_string()),
                false,
            )
            .await
            .expect("Failed to create test lvol");

        // Create the first snapshot.
        let entity_id = String::from("lvol6_e1");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("lvol6_snap1");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params1 = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params1.clone())
            .await
            .expect("Failed to create a snapshot");

        // Create the second snapshot.
        let entity_id2 = String::from("lvol6_e2");
        let parent_id2 = lvol.uuid();
        let txn_id2 = Uuid::new_v4().to_string();
        let snap_name2 = String::from("lvol6_snap2");
        let snapshot_uuid2 = Uuid::new_v4().to_string();

        let snapshot_params2 = SnapshotParams::new(
            Some(entity_id2.clone()),
            Some(parent_id2.clone()),
            Some(txn_id2.clone()),
            Some(snap_name2.clone()),
            Some(snapshot_uuid2.clone()),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params2.clone())
            .await
            .expect("Failed to create a snapshot");

        // Check that snapshots are properly reported via pool snapshot
        // iterator.
        let snapshots = pool
            .snapshots()
            .expect("Can't get snapshot iterator for lvol")
            .collect::<Vec<_>>();

        assert_eq!(snapshots.len(), 2, "Not all snapshots are listed");

        let n = snapshots[0]
            .snapshot_params()
            .name()
            .expect("Can't get snapshot name");
        let idxs: [usize; 2] = if n == snap_name2 { [1, 0] } else { [0, 1] };

        // Check that snapshots match their initial parameters.
        check_snapshot_descriptor(&snapshot_params1, &snapshots[idxs[0]]);
        check_snapshot_descriptor(&snapshot_params2, &snapshots[idxs[1]]);
        clean_snapshots(snapshots).await;
        pool.export().await.expect("Failed to export the pool");
    })
    .await;
}

#[tokio::test]
async fn test_list_all_snapshots_with_replica_destroy() {
    let ms = get_ms();

    ms.spawn(async move {
        // Create a pool and lvol.
        let pool = create_test_pool(
            "pool7",
            "malloc:///disk7?size_mb=128".to_string(),
        )
        .await;
        let lvol = pool
            .create_lvol(
                "lvol7",
                32 * 1024 * 1024,
                Some(&Uuid::new_v4().to_string()),
                false,
            )
            .await
            .expect("Failed to create test lvol");

        // Create a snapshot-1 via lvol object.
        let entity_id = String::from("lvol7_e1");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("lvol7_snap1");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        lvol.destroy().await.expect("Failed to destroy replica");

        let snapshot_list = Lvol::list_all_snapshots();
        info!("Total number of snapshots: {}", snapshot_list.len());
        assert_eq!(1, snapshot_list.len(), "Snapshot Count not matched!!");
        clean_snapshots(snapshot_list).await;
    })
    .await;
}
#[tokio::test]
async fn test_snapshot_referenced_size() {
    let ms = get_ms();
    const LVOL_NAME: &str = "lvol8";
    const LVOL_SIZE: u64 = 24 * 1024 * 1024;

    ms.spawn(async move {
        // Create a pool and lvol.
        let pool = create_test_pool(
            "pool8",
            "malloc:///disk8?size_mb=64".to_string(),
        )
        .await;

        let cluster_size = pool.blob_cluster_size();

        let lvol = pool
            .create_lvol(
                LVOL_NAME,
                LVOL_SIZE,
                Some(&Uuid::new_v4().to_string()),
                false,
            )
            .await
            .expect("Failed to create test lvol");

        // Thick-provisioned volume, all blob clusters must be pre-allocated.
        assert_eq!(
            lvol.usage().allocated_bytes,
            LVOL_SIZE,
            "Wiped superbock is not properly accounted in volume allocated bytes"
        );

        /* Scenario 1: create a snapshot for a volume without any data written:
         * snapshot size must be equal to the initial volume size and current
         * size of the volume must be zero.
         * Note: initially volume is thick-provisioned, so snapshot shall own
         * all volume's data.
         */
        let snap1_name = "lvol8_snapshot1".to_string();
        let mut snapshot_params = SnapshotParams::new(
            Some("e1".to_string()),
            Some("p1".to_string()),
            Some(Uuid::new_v4().to_string()),
            Some(snap1_name.clone()),
            Some(Uuid::new_v4().to_string()),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create the first snapshot for test volume");

        // Make sure snapshot fully owns initial volume data.
        let snapshot_list = pool.snapshots().expect("Failed to enumerate poool snapshots").collect::<Vec<_>>();
        assert_eq!(snapshot_list.len(), 1, "No first snapshot found");
        assert_eq!(
            snapshot_list[0].snapshot_size,
            LVOL_SIZE,
            "Snapshot size doesn't properly reflect wiped superblock"
        );

        let snap_lvol = find_snapshot_device(&snap1_name)
            .await
            .expect("Can't lookup snapshot lvol");
        assert_eq!(
            snap_lvol.usage().allocated_bytes,
            LVOL_SIZE,
            "Snapshot size doesn't properly reflect wiped superblock"
        );

        // Make sure volume has no allocated space after snapshot is taken.
        assert_eq!(
            lvol.usage().allocated_bytes,
            0,
            "Volume still has some space allocated after taking a snapshot"
        );

        /* Scenario 2: write some data to volume at 2nd cluster, take the second snapshot
         * and make sure snapshot size reflects the amount of data written (aligned by
         * the size of the blobstore cluster).
         * Note: volume is now a thin-provisioned volume, so the volume stores only incremental
         * differences from its underlying snapshot.
         */
        bdev_io::write_some(LVOL_NAME, 2 * cluster_size, 16, 0xaau8)
            .await
            .expect("Failed to write data to volume");

        bdev_io::write_some(LVOL_NAME, 3 * cluster_size, 16, 0xbbu8)
            .await
            .expect("Failed to write data to volume");

        // Make sure volume has exactly one allocated cluster even if a smaller amount of bytes was written.
        assert_eq!(
            lvol.usage().allocated_bytes,
            2 * cluster_size,
            "Volume still has some space allocated after taking a snapshot"
        );

        let snap2_name = "lvol8_snapshot2".to_string();
        snapshot_params.set_name(snap2_name.clone());
        snapshot_params.set_snapshot_uuid(Uuid::new_v4().to_string());

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create the second snapshot for test volume");

        let snapshot_list = pool.snapshots().expect("Failed to enumerate poool snapshots").collect::<Vec<_>>();
        assert_eq!(snapshot_list.len(), 2, "Not all snapshots found");
        let snap_lvol = snapshot_list.iter().find(|s| {
            s.snapshot_params().name().expect("Snapshot has no name") == snap2_name
        })
        .expect("No second snapshot found");

        // Before a new data is written to the volume, volume's space accounts snapshot space too.
        assert_eq!(
            lvol.usage().allocated_bytes,
            2 * cluster_size,
            "Volume still has some space allocated after taking a snapshot"
        );

        // Make sure snapshot owns newly written volume data.
        assert_eq!(
            snap_lvol.snapshot_size,
            2 * cluster_size,
            "Snapshot size doesn't properly reflect new volume data"
        );

        let snap_lvol = find_snapshot_device(&snap2_name)
            .await
            .expect("Can't lookup snapshot lvol");
        assert_eq!(
            snap_lvol.usage().allocated_bytes,
            2 * cluster_size,
            "Snapshot size doesn't properly reflect wiped superblock"
        );

        // Write some data to the volume and make sure volume accounts only
        // new incremental storage difference (1 cluster).
        bdev_io::write_some(LVOL_NAME, 0, 16, 0xccu8)
            .await
            .expect("Failed to write data to volume");

        assert_eq!(
            lvol.usage().allocated_bytes,
            cluster_size,
            "Volume still has some space allocated after taking a snapshot"
        );

        // Make sure snapshots allocated space hasn't changed.
        let snap_lvol = find_snapshot_device(&snap2_name)
            .await
            .expect("Can't lookup snapshot lvol");
        assert_eq!(
            snap_lvol.usage().allocated_bytes,
            2 * cluster_size,
            "Snapshot size doesn't properly reflect wiped superblock"
        );

    })
    .await;
}
#[tokio::test]
async fn test_snapshot_clone() {
    let ms = get_ms();

    ms.spawn(async move {
        // Create a pool and lvol.
        let pool = create_test_pool(
            "pool9",
            "malloc:///disk5?size_mb=128".to_string(),
        )
        .await;
        let lvol = pool
            .create_lvol(
                "lvol9",
                32 * 1024 * 1024,
                Some(&Uuid::new_v4().to_string()),
                false,
            )
            .await
            .expect("Failed to create test lvol");

        // Create a snapshot-1 via lvol object.
        let entity_id = String::from("lvol9_e1");
        let parent_id = lvol.uuid();
        let txn_id = Uuid::new_v4().to_string();
        let snap_name = String::from("lvol9_snap1");
        let snapshot_uuid = Uuid::new_v4().to_string();

        let snapshot_params = SnapshotParams::new(
            Some(entity_id),
            Some(parent_id),
            Some(txn_id),
            Some(snap_name),
            Some(snapshot_uuid),
            Some(Utc::now().to_string()),
        );

        lvol.create_snapshot(snapshot_params.clone())
            .await
            .expect("Failed to create a snapshot");

        let snapshot_list = Lvol::list_all_snapshots();
        assert_eq!(1, snapshot_list.len(), "Snapshot Count not matched!!");
        let snapshot_lvol = UntypedBdev::lookup_by_uuid_str(
            snapshot_list
                .get(0)
                .unwrap()
                .snapshot_params()
                .snapshot_uuid()
                .unwrap_or_default()
                .as_str(),
        )
        .map(|b| Lvol::try_from(b).expect("Can't create Lvol from device"))
        .unwrap();
        let clone_name = String::from("lvol9_snap1_clone_1");
        let clone_uuid = Uuid::new_v4().to_string();
        let source_uuid = snapshot_lvol.uuid();

        let clone_param = CloneParams::new(
            Some(clone_name),
            Some(clone_uuid),
            Some(source_uuid),
            Some(Utc::now().to_string()),
        );
        let clone = snapshot_lvol
            .create_clone(clone_param.clone())
            .await
            .expect("Failed to create a clone");
        info!("Clone creation success with uuid {:?}", clone.uuid());
        check_clone(clone, clone_param).await;
    })
    .await;
}
use std::num::NonZeroUsize;

use gource_core::{
    Action, Catalog, Event, EventKey, EventTarget, Generation, History, PathId, ReplayConfig, Rgb8,
    SourceSeq,
};
use gource_sim::{ExecutionMode, ReplaySession, SceneSnapshot};

const BULK_FILES: usize = 128;
const FUTURE_TIMESTAMP: i64 = 1_000_000;
const REPOSITORY_SECONDS_PER_TICK: i128 = 720;
const EQUIVALENCE_THREADS: [usize; 4] = [1, 2, 4, 8];
const INTERLEAVED_PARENTS: usize = 2;
const INTERLEAVED_SIBLINGS_PER_KIND: usize = 32;
const BALANCED_PARENTS: usize = 4;
const BALANCED_SIBLINGS_PER_KIND: usize = 32;
const SINGLE_PARENT_FILES: usize = 128;

struct ChurnScenario {
    history: History,
    recreated_path: PathId,
    feature_old_path: PathId,
    feature_new_path: PathId,
    future_path: PathId,
}

struct FadeScenario {
    history: History,
    deleted_path: PathId,
    idle_path: PathId,
}

fn push_event(
    events: &mut Vec<Event>,
    timestamp: i64,
    contributor: gource_core::ContributorId,
    target: EventTarget,
    action: Action,
    color: Option<Rgb8>,
) {
    let sequence = events.len() as u64;
    events.push(Event::new(
        EventKey::new(timestamp, SourceSeq::new(sequence).expect("event sequence")),
        Generation::ZERO,
        contributor,
        target,
        action,
        color,
    ));
}

fn churn_scenario() -> ChurnScenario {
    let mut catalog = Catalog::new();
    let contributors = [
        catalog.intern_contributor("alice").expect("alice"),
        catalog.intern_contributor("bob").expect("bob"),
        catalog.intern_contributor("carol").expect("carol"),
    ];

    let mut bulk_paths = Vec::with_capacity(BULK_FILES);
    for index in 0..BULK_FILES {
        let path = format!("bulk/group{:02}/file{:03}.rs", index % 4, index);
        bulk_paths.push(catalog.intern_path_str(&path).expect("bulk path"));
    }
    let main_path = catalog.intern_path_str("src/main.rs").expect("main path");
    let core_path = catalog
        .intern_path_str("src/lib/core.rs")
        .expect("core path");
    let recreated_path = catalog
        .intern_path_str("src/recreated.rs")
        .expect("recreated path");
    let feature_old_path = catalog
        .intern_path_str("src/feature/old.rs")
        .expect("old feature path");
    let feature_keep_path = catalog
        .intern_path_str("src/feature/keep.rs")
        .expect("kept feature path");
    let feature_new_path = catalog
        .intern_path_str("src/feature/new.rs")
        .expect("new feature path");
    let feature_deep_new_path = catalog
        .intern_path_str("src/feature/deep/new.rs")
        .expect("deep feature path");
    let feature_dir = catalog
        .intern_path_str("src/feature/")
        .expect("feature directory");

    let mut events = Vec::with_capacity(BULK_FILES + 16);
    for (index, path) in bulk_paths.iter().copied().enumerate() {
        push_event(
            &mut events,
            0,
            contributors[index % contributors.len()],
            EventTarget::File(path),
            Action::Add,
            (index == 0).then_some(Rgb8::new(12, 34, 56)),
        );
    }
    for (path, contributor, color) in [
        (main_path, contributors[0], Some(Rgb8::new(201, 20, 31))),
        (core_path, contributors[1], None),
        (recreated_path, contributors[2], None),
        (feature_old_path, contributors[0], None),
        (feature_keep_path, contributors[1], None),
    ] {
        push_event(
            &mut events,
            0,
            contributor,
            EventTarget::File(path),
            Action::Add,
            color,
        );
    }

    // Equal-time events exercise stable source-sequence ordering within one
    // canonical tick instead of relying only on event timestamp ordering.
    push_event(
        &mut events,
        720,
        contributors[1],
        EventTarget::File(main_path),
        Action::Modify,
        Some(Rgb8::new(88, 99, 111)),
    );
    push_event(
        &mut events,
        720,
        contributors[2],
        EventTarget::File(core_path),
        Action::Modify,
        None,
    );

    // Delete/re-add allocates a new file incarnation while the old one fades.
    push_event(
        &mut events,
        1_440,
        contributors[0],
        EventTarget::File(recreated_path),
        Action::Delete,
        Some(Rgb8::new(240, 80, 80)),
    );
    push_event(
        &mut events,
        2_160,
        contributors[2],
        EventTarget::File(recreated_path),
        Action::Add,
        None,
    );

    // Removing an entire compressed subtree forces hierarchy pruning; later
    // additions recreate nested directories and exercise churn in both maps.
    push_event(
        &mut events,
        2_880,
        contributors[1],
        EventTarget::Directory(feature_dir),
        Action::Delete,
        None,
    );
    push_event(
        &mut events,
        3_600,
        contributors[0],
        EventTarget::File(feature_new_path),
        Action::Add,
        None,
    );
    push_event(
        &mut events,
        3_600,
        contributors[2],
        EventTarget::File(feature_deep_new_path),
        Action::Add,
        None,
    );

    // This event is intentionally far enough ahead that auto-skip must jump
    // after the action lifetime and idle gate have both elapsed.
    push_event(
        &mut events,
        FUTURE_TIMESTAMP,
        contributors[1],
        EventTarget::File(bulk_paths[0]),
        Action::Modify,
        Some(Rgb8::new(17, 180, 220)),
    );

    ChurnScenario {
        history: History::new(catalog, events).expect("canonical churn history"),
        recreated_path,
        feature_old_path,
        feature_new_path,
        future_path: bulk_paths[0],
    }
}

fn fade_scenario() -> FadeScenario {
    let mut catalog = Catalog::new();
    let alice = catalog.intern_contributor("alice").expect("alice");
    let bob = catalog.intern_contributor("bob").expect("bob");
    let deleted_path = catalog
        .intern_path_str("fade/recreated.rs")
        .expect("deleted path");
    let idle_path = catalog.intern_path_str("fade/idle.rs").expect("idle path");
    let companion_path = catalog
        .intern_path_str("fade/companion.rs")
        .expect("companion path");

    let mut events = Vec::new();
    for (path, contributor) in [
        (deleted_path, alice),
        (idle_path, bob),
        (companion_path, alice),
    ] {
        push_event(
            &mut events,
            0,
            contributor,
            EventTarget::File(path),
            Action::Add,
            None,
        );
    }
    push_event(
        &mut events,
        720,
        alice,
        EventTarget::File(deleted_path),
        Action::Delete,
        None,
    );
    push_event(
        &mut events,
        1_440,
        bob,
        EventTarget::File(deleted_path),
        Action::Add,
        None,
    );

    FadeScenario {
        history: History::new(catalog, events).expect("canonical fade history"),
        deleted_path,
        idle_path,
    }
}

fn mixed_parent_history(parent_count: usize, siblings_per_kind: usize) -> History {
    let mut catalog = Catalog::new();
    let contributors = [
        catalog.intern_contributor("alice").expect("alice"),
        catalog.intern_contributor("bob").expect("bob"),
        catalog.intern_contributor("carol").expect("carol"),
    ];

    let mut nested_paths = Vec::with_capacity(parent_count);
    let mut direct_paths = Vec::with_capacity(parent_count);
    for parent in 0..parent_count {
        let mut nested = Vec::with_capacity(siblings_per_kind);
        let mut direct = Vec::with_capacity(siblings_per_kind);
        for sibling in 0..siblings_per_kind {
            nested.push(
                catalog
                    .intern_path_str(&format!(
                        "group{parent:02}/child{sibling:02}/leaf{sibling:02}.rs"
                    ))
                    .expect("nested sibling path"),
            );
            direct.push(
                catalog
                    .intern_path_str(&format!("group{parent:02}/direct{sibling:02}.rs"))
                    .expect("direct sibling path"),
            );
        }
        nested_paths.push(nested);
        direct_paths.push(direct);
    }

    // Alternate parent and node kinds while creating the hierarchy.  This
    // intentionally interleaves directory and file ID allocation across
    // parents instead of producing one contiguous subtree per parent.
    let mut events = Vec::with_capacity(parent_count * siblings_per_kind * 2);
    for sibling in 0..siblings_per_kind {
        for parent in 0..parent_count {
            push_event(
                &mut events,
                0,
                contributors[(parent + sibling) % contributors.len()],
                EventTarget::File(nested_paths[parent][sibling]),
                Action::Add,
                None,
            );
            push_event(
                &mut events,
                0,
                contributors[(parent + sibling + 1) % contributors.len()],
                EventTarget::File(direct_paths[parent][sibling]),
                Action::Add,
                None,
            );
        }
    }

    History::new(catalog, events).expect("canonical mixed-parent history")
}

fn interleaved_mixed_scenario() -> History {
    mixed_parent_history(INTERLEAVED_PARENTS, INTERLEAVED_SIBLINGS_PER_KIND)
}

fn balanced_multi_parent_scenario() -> History {
    mixed_parent_history(BALANCED_PARENTS, BALANCED_SIBLINGS_PER_KIND)
}

fn single_parent_fallback_scenario() -> History {
    let mut catalog = Catalog::new();
    let contributors = [
        catalog.intern_contributor("alice").expect("alice"),
        catalog.intern_contributor("bob").expect("bob"),
    ];
    let mut events = Vec::with_capacity(SINGLE_PARENT_FILES);
    for index in 0..SINGLE_PARENT_FILES {
        let path = catalog
            .intern_path_str(&format!("single-parent-file{index:03}.rs"))
            .expect("single-parent file path");
        push_event(
            &mut events,
            0,
            contributors[index % contributors.len()],
            EventTarget::File(path),
            Action::Add,
            None,
        );
    }

    History::new(catalog, events).expect("canonical single-parent history")
}

fn grouped_layout_config() -> ReplayConfig {
    ReplayConfig {
        realtime: false,
        seconds_per_day: 1.0,
        auto_skip_seconds: 0.0,
        file_idle_seconds: None,
        ..ReplayConfig::default()
    }
}

fn accelerated_config() -> ReplayConfig {
    ReplayConfig {
        // One repository day per wall second makes integer event timestamps
        // land on nearby canonical ticks, keeping this regression bounded.
        realtime: false,
        seconds_per_day: 1.0,
        auto_skip_seconds: 0.25,
        file_idle_seconds: None,
        ..ReplayConfig::default()
    }
}

fn fade_config() -> ReplayConfig {
    ReplayConfig {
        realtime: false,
        seconds_per_day: 1.0,
        auto_skip_seconds: 0.0,
        // 0.02 s rounds up to three 120-Hz ticks, so the fourth tick starts
        // idle-file fading and exposes both full and partial opacity states.
        file_idle_seconds: Some(0.02),
        ..ReplayConfig::default()
    }
}

fn parallel_mode(threads: usize) -> ExecutionMode {
    ExecutionMode::Parallel {
        threads: NonZeroUsize::new(threads).expect("non-zero thread count"),
    }
}

fn assert_snapshots_equal(
    serial: &ReplaySession<History>,
    parallel: &ReplaySession<History>,
    context: &str,
) {
    assert_eq!(
        serial.snapshot().as_ref(),
        parallel.snapshot().as_ref(),
        "serial and parallel snapshots diverged ({context})"
    );
}

fn assert_tick_checkpoint_seek_equivalence(
    history: &History,
    config: ReplayConfig,
    ticks: u64,
    checkpoint_tick: u64,
    seek_tick: u64,
    context: &str,
) {
    assert!(checkpoint_tick > 0 && checkpoint_tick <= ticks);
    assert!(seek_tick < checkpoint_tick);

    for threads in EQUIVALENCE_THREADS {
        let mut serial = ReplaySession::new_with_execution(
            history.clone(),
            config.clone(),
            ExecutionMode::Serial,
        )
        .expect("serial grouped-layout replay construction");
        let mut parallel = ReplaySession::new_with_execution(
            history.clone(),
            config.clone(),
            parallel_mode(threads),
        )
        .expect("parallel grouped-layout replay construction");

        assert_snapshots_equal(&serial, &parallel, &format!("{context} origin"));

        let mut serial_checkpoint = None;
        let mut parallel_checkpoint = None;
        let mut serial_checkpoint_snapshot = None;
        let mut parallel_checkpoint_snapshot = None;
        for tick in 1..=ticks {
            serial.advance_ticks(1).expect("serial grouped-layout tick");
            parallel
                .advance_ticks(1)
                .expect("parallel grouped-layout tick");
            assert_snapshots_equal(&serial, &parallel, &format!("{context} tick {tick}"));

            if tick == checkpoint_tick {
                serial_checkpoint = Some(serial.checkpoint());
                parallel_checkpoint = Some(parallel.checkpoint());
                serial_checkpoint_snapshot = Some(serial.snapshot());
                parallel_checkpoint_snapshot = Some(parallel.snapshot());
            }
        }

        let serial_checkpoint = serial_checkpoint.expect("serial grouped-layout checkpoint");
        let parallel_checkpoint = parallel_checkpoint.expect("parallel grouped-layout checkpoint");
        let serial_checkpoint_snapshot =
            serial_checkpoint_snapshot.expect("serial grouped-layout checkpoint snapshot");
        let parallel_checkpoint_snapshot =
            parallel_checkpoint_snapshot.expect("parallel grouped-layout checkpoint snapshot");

        serial
            .advance_ticks(13)
            .expect("serial grouped-layout post-checkpoint ticks");
        parallel
            .advance_ticks(13)
            .expect("parallel grouped-layout post-checkpoint ticks");
        assert_snapshots_equal(&serial, &parallel, &format!("{context} post-checkpoint"));

        serial
            .restore_checkpoint(&serial_checkpoint)
            .expect("serial grouped-layout checkpoint restore");
        parallel
            .restore_checkpoint(&parallel_checkpoint)
            .expect("parallel grouped-layout checkpoint restore");
        assert_snapshots_equal(&serial, &parallel, &format!("{context} checkpoint restore"));
        assert_eq!(
            serial.snapshot().as_ref(),
            serial_checkpoint_snapshot.as_ref(),
            "serial grouped-layout checkpoint changed ({context})"
        );
        assert_eq!(
            parallel.snapshot().as_ref(),
            parallel_checkpoint_snapshot.as_ref(),
            "parallel grouped-layout checkpoint changed ({context})"
        );

        serial
            .seek_tick(seek_tick)
            .expect("serial grouped-layout backward seek");
        parallel
            .seek_tick(seek_tick)
            .expect("parallel grouped-layout backward seek");
        assert_snapshots_equal(&serial, &parallel, &format!("{context} backward seek"));

        let serial_seek_checkpoint = serial.checkpoint();
        let parallel_seek_checkpoint = parallel.checkpoint();
        let seek_snapshot = serial.snapshot();
        serial
            .advance_ticks(17)
            .expect("serial grouped-layout seek replay");
        parallel
            .advance_ticks(17)
            .expect("parallel grouped-layout seek replay");
        assert_snapshots_equal(&serial, &parallel, &format!("{context} seek replay"));

        serial
            .restore_checkpoint(&serial_seek_checkpoint)
            .expect("serial grouped-layout seek checkpoint restore");
        parallel
            .restore_checkpoint(&parallel_seek_checkpoint)
            .expect("parallel grouped-layout seek checkpoint restore");
        assert_snapshots_equal(
            &serial,
            &parallel,
            &format!("{context} seek checkpoint restore"),
        );
        assert_eq!(
            serial.snapshot().as_ref(),
            seek_snapshot.as_ref(),
            "serial grouped-layout seek checkpoint changed ({context})"
        );
    }
}

fn repository_floor(snapshot: &SceneSnapshot) -> i128 {
    snapshot
        .repository_time
        .numerator
        .div_euclid(snapshot.repository_time.denominator)
}

fn active_count(snapshot: &SceneSnapshot, path: PathId) -> usize {
    snapshot
        .files
        .iter()
        .filter(|file| file.path_id == path && file.active)
        .count()
}

#[test]
fn grouped_layout_matches_serial_for_interleaved_typed_ids_and_mixed_siblings() {
    let history = interleaved_mixed_scenario();
    assert_tick_checkpoint_seek_equivalence(
        &history,
        grouped_layout_config(),
        48,
        24,
        7,
        "interleaved typed IDs and mixed siblings",
    );
}

#[test]
fn grouped_layout_matches_serial_for_one_parent_fallback_scene() {
    let history = single_parent_fallback_scenario();
    assert_tick_checkpoint_seek_equivalence(
        &history,
        grouped_layout_config(),
        48,
        24,
        7,
        "one-parent fallback",
    );
}

#[test]
fn grouped_layout_matches_serial_for_balanced_multi_parent_groups() {
    let history = balanced_multi_parent_scenario();
    assert_tick_checkpoint_seek_equivalence(
        &history,
        grouped_layout_config(),
        48,
        24,
        7,
        "balanced multi-parent groups",
    );
}

#[test]
fn parallel_matches_serial_for_large_scene_churn_idle_skip_and_restore() {
    let scenario = churn_scenario();

    for threads in EQUIVALENCE_THREADS {
        let config = accelerated_config();
        let mut serial = ReplaySession::new_with_execution(
            scenario.history.clone(),
            config.clone(),
            ExecutionMode::Serial,
        )
        .expect("serial replay construction");
        let mut parallel = ReplaySession::new_with_execution(
            scenario.history.clone(),
            config,
            parallel_mode(threads),
        )
        .expect("parallel replay construction");

        assert_snapshots_equal(&serial, &parallel, "origin");
        let initial = serial.snapshot();
        assert_eq!(initial.tick, 0);
        assert!(
            initial.actions.len() >= BULK_FILES,
            "same-timestamp origin burst was not represented"
        );

        let mut serial_checkpoint = None;
        let mut parallel_checkpoint = None;
        let mut serial_checkpoint_snapshot = None;
        let mut parallel_checkpoint_snapshot = None;
        let mut previous_repository_time = repository_floor(initial.as_ref());
        let mut saw_idle_skip = false;

        for tick in 1..=240 {
            serial.advance_ticks(1).expect("serial tick");
            parallel.advance_ticks(1).expect("parallel tick");
            assert_snapshots_equal(&serial, &parallel, &format!("tick {tick}"));

            let snapshot = serial.snapshot();
            let repository_time = repository_floor(snapshot.as_ref());
            if repository_time - previous_repository_time > REPOSITORY_SECONDS_PER_TICK {
                saw_idle_skip = true;
                assert!(
                    repository_time >= i128::from(FUTURE_TIMESTAMP),
                    "idle skip must land on the future event timestamp"
                );
                assert!(
                    snapshot
                        .actions
                        .iter()
                        .any(|action| action.path_id == scenario.future_path
                            && action.action == Action::Modify),
                    "future event was not applied after idle skip"
                );
            }
            previous_repository_time = repository_time;

            match tick {
                2 => {
                    assert_eq!(active_count(snapshot.as_ref(), scenario.recreated_path), 0);
                    assert!(snapshot.files.iter().any(|file| {
                        file.path_id == scenario.recreated_path
                            && !file.active
                            && file.opacity > 0.99
                    }));
                }
                3 => {
                    let incarnations: Vec<_> = snapshot
                        .files
                        .iter()
                        .filter(|file| file.path_id == scenario.recreated_path)
                        .collect();
                    assert_eq!(active_count(snapshot.as_ref(), scenario.recreated_path), 1);
                    assert!(
                        incarnations.len() >= 2,
                        "old incarnation should still be fading"
                    );
                    let active = incarnations
                        .iter()
                        .find(|file| file.active)
                        .expect("recreated active file");
                    let fading = incarnations
                        .iter()
                        .find(|file| !file.active)
                        .expect("old fading file");
                    assert_ne!(active.file_id, fading.file_id);
                }
                4 => {
                    assert_eq!(
                        active_count(snapshot.as_ref(), scenario.feature_old_path),
                        0
                    );
                }
                5 => {
                    assert_eq!(
                        active_count(snapshot.as_ref(), scenario.feature_new_path),
                        1
                    );
                }
                8 => {
                    serial_checkpoint = Some(serial.checkpoint());
                    parallel_checkpoint = Some(parallel.checkpoint());
                    serial_checkpoint_snapshot = Some(serial.snapshot());
                    parallel_checkpoint_snapshot = Some(parallel.snapshot());
                }
                _ => {}
            }
        }

        assert!(saw_idle_skip, "history did not exercise the auto-skip path");
        let serial_checkpoint = serial_checkpoint.expect("serial checkpoint");
        let parallel_checkpoint = parallel_checkpoint.expect("parallel checkpoint");
        let serial_checkpoint_snapshot =
            serial_checkpoint_snapshot.expect("serial checkpoint snapshot");
        let parallel_checkpoint_snapshot =
            parallel_checkpoint_snapshot.expect("parallel checkpoint snapshot");

        // Continue beyond the checkpoint, restore it, then seek backwards to
        // force both checkpoint state and fresh-origin replay through the same
        // mode comparison.
        serial
            .advance_ticks(19)
            .expect("serial post-checkpoint ticks");
        parallel
            .advance_ticks(19)
            .expect("parallel post-checkpoint ticks");
        assert_snapshots_equal(&serial, &parallel, "post-checkpoint advance");

        serial
            .restore_checkpoint(&serial_checkpoint)
            .expect("serial checkpoint restore");
        parallel
            .restore_checkpoint(&parallel_checkpoint)
            .expect("parallel checkpoint restore");
        assert_snapshots_equal(&serial, &parallel, "checkpoint restore");
        assert_eq!(
            serial.snapshot().as_ref(),
            serial_checkpoint_snapshot.as_ref()
        );
        assert_eq!(
            parallel.snapshot().as_ref(),
            parallel_checkpoint_snapshot.as_ref()
        );

        serial.seek_tick(3).expect("serial backward seek");
        parallel.seek_tick(3).expect("parallel backward seek");
        assert_snapshots_equal(&serial, &parallel, "backward seek");

        let serial_rewind_checkpoint = serial.checkpoint();
        let parallel_rewind_checkpoint = parallel.checkpoint();
        let rewind_snapshot = serial.snapshot();
        serial.advance_ticks(37).expect("serial rewind replay");
        parallel.advance_ticks(37).expect("parallel rewind replay");
        assert_snapshots_equal(&serial, &parallel, "replay after backward seek");
        serial
            .restore_checkpoint(&serial_rewind_checkpoint)
            .expect("serial rewind checkpoint restore");
        parallel
            .restore_checkpoint(&parallel_rewind_checkpoint)
            .expect("parallel rewind checkpoint restore");
        assert_snapshots_equal(&serial, &parallel, "rewind checkpoint restore");
        assert_eq!(serial.snapshot().as_ref(), rewind_snapshot.as_ref());
    }
}

#[test]
fn parallel_matches_serial_through_delete_and_idle_file_fades() {
    let scenario = fade_scenario();

    for threads in EQUIVALENCE_THREADS {
        let config = fade_config();
        let mut serial = ReplaySession::new_with_execution(
            scenario.history.clone(),
            config.clone(),
            ExecutionMode::Serial,
        )
        .expect("serial fade replay construction");
        let mut parallel = ReplaySession::new_with_execution(
            scenario.history.clone(),
            config,
            parallel_mode(threads),
        )
        .expect("parallel fade replay construction");
        assert_snapshots_equal(&serial, &parallel, "fade origin");

        let mut serial_checkpoint = None;
        let mut parallel_checkpoint = None;
        let mut serial_checkpoint_snapshot = None;
        let mut parallel_checkpoint_snapshot = None;

        for tick in 1..=100 {
            serial.advance_ticks(1).expect("serial fade tick");
            parallel.advance_ticks(1).expect("parallel fade tick");
            assert_snapshots_equal(&serial, &parallel, &format!("fade tick {tick}"));
            if tick == 40 {
                serial_checkpoint = Some(serial.checkpoint());
                parallel_checkpoint = Some(parallel.checkpoint());
                serial_checkpoint_snapshot = Some(serial.snapshot());
                parallel_checkpoint_snapshot = Some(parallel.snapshot());
            }

            let snapshot = serial.snapshot();
            match tick {
                1 => {
                    let deleted = snapshot
                        .files
                        .iter()
                        .find(|file| file.path_id == scenario.deleted_path)
                        .expect("deleted file fade start");
                    assert!(!deleted.active);
                    assert!(deleted.opacity > 0.99);
                }
                2 => {
                    let deleted: Vec<_> = snapshot
                        .files
                        .iter()
                        .filter(|file| file.path_id == scenario.deleted_path)
                        .collect();
                    assert_eq!(active_count(snapshot.as_ref(), scenario.deleted_path), 1);
                    assert!(deleted.len() >= 2, "re-add should retain old fade");
                    assert!(
                        deleted
                            .iter()
                            .any(|file| !file.active && file.opacity < 1.0)
                    );
                    assert!(deleted.iter().any(|file| file.active));
                }
                4 => {
                    let idle = snapshot
                        .files
                        .iter()
                        .find(|file| file.path_id == scenario.idle_path)
                        .expect("idle file fade start");
                    assert!(!idle.active);
                    assert!(idle.opacity > 0.99);
                }
                5 => {
                    let idle = snapshot
                        .files
                        .iter()
                        .find(|file| file.path_id == scenario.idle_path)
                        .expect("idle file partial fade");
                    assert!(!idle.active);
                    assert!(idle.opacity < 1.0 && idle.opacity > 0.0);
                }
                _ => {}
            }
        }
        let serial_checkpoint = serial_checkpoint.expect("serial fade checkpoint");
        let parallel_checkpoint = parallel_checkpoint.expect("parallel fade checkpoint");
        let serial_checkpoint_snapshot =
            serial_checkpoint_snapshot.expect("serial fade checkpoint snapshot");
        let parallel_checkpoint_snapshot =
            parallel_checkpoint_snapshot.expect("parallel fade checkpoint snapshot");
        serial
            .restore_checkpoint(&serial_checkpoint)
            .expect("serial fade checkpoint restore");
        parallel
            .restore_checkpoint(&parallel_checkpoint)
            .expect("parallel fade checkpoint restore");
        assert_snapshots_equal(&serial, &parallel, "fade checkpoint restore");
        assert_eq!(
            serial.snapshot().as_ref(),
            serial_checkpoint_snapshot.as_ref()
        );
        assert_eq!(
            parallel.snapshot().as_ref(),
            parallel_checkpoint_snapshot.as_ref()
        );

        serial.seek_tick(10).expect("serial fade backward seek");
        parallel.seek_tick(10).expect("parallel fade backward seek");
        assert_snapshots_equal(&serial, &parallel, "fade backward seek");
        let serial_seek_checkpoint = serial.checkpoint();
        let parallel_seek_checkpoint = parallel.checkpoint();
        let seek_snapshot = serial.snapshot();
        serial.advance_ticks(17).expect("serial fade seek replay");
        parallel
            .advance_ticks(17)
            .expect("parallel fade seek replay");
        assert_snapshots_equal(&serial, &parallel, "fade seek replay");
        serial
            .restore_checkpoint(&serial_seek_checkpoint)
            .expect("serial fade seek checkpoint restore");
        parallel
            .restore_checkpoint(&parallel_seek_checkpoint)
            .expect("parallel fade seek checkpoint restore");
        assert_snapshots_equal(&serial, &parallel, "fade seek checkpoint restore");
        assert_eq!(serial.snapshot().as_ref(), seek_snapshot.as_ref());
    }
}

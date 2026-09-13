use std::sync::mpsc;
use std::time::Duration;

use gource_core::{
    Action, Catalog, Event, EventKey, EventTarget, Generation, History, ReplayConfig, SourceSeq,
};
use gource_sim::{PumpResult, ReplaySession, WorkBudget};

fn burst_history() -> History {
    let mut catalog = Catalog::new();
    let path = catalog.intern_path_str("src/main.rs").unwrap();
    let contributor = catalog.intern_contributor("alice").unwrap();
    let events = vec![
        Event::new(
            EventKey::new(0, SourceSeq::new(0).unwrap()),
            Generation::ZERO,
            contributor,
            EventTarget::File(path),
            Action::Add,
            None,
        ),
        Event::new(
            EventKey::new(1, SourceSeq::new(1).unwrap()),
            Generation::ZERO,
            contributor,
            EventTarget::File(path),
            Action::Modify,
            None,
        ),
        Event::new(
            EventKey::new(1, SourceSeq::new(2).unwrap()),
            Generation::ZERO,
            contributor,
            EventTarget::File(path),
            Action::Modify,
            None,
        ),
        Event::new(
            EventKey::new(1, SourceSeq::new(3).unwrap()),
            Generation::ZERO,
            contributor,
            EventTarget::File(path),
            Action::Modify,
            None,
        ),
    ];
    History::new(catalog, events).unwrap()
}

fn deterministic_config() -> ReplayConfig {
    ReplayConfig {
        realtime: true,
        seconds_per_day: 86_400.0,
        auto_skip_seconds: 0.0,
        ..ReplayConfig::default()
    }
}

fn pump_promptly(
    mut session: ReplaySession<History>,
    budget: WorkBudget,
) -> (ReplaySession<History>, PumpResult) {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = session.pump(budget);
        let _ = sender.send((session, result));
    });

    let (session, result) = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("bounded pump did not return promptly");
    (
        session,
        result.expect("bounded pump returned a replay error"),
    )
}

#[test]
fn pump_budget_makes_one_tick_of_idle_progress_and_keeps_a_due_burst() {
    let history = burst_history();
    let config = deterministic_config();
    let mut expected = ReplaySession::new(history.clone(), config.clone()).unwrap();
    expected.advance_ticks(123).unwrap();

    let mut session = ReplaySession::new(history, config).unwrap();
    assert_eq!(session.snapshot().tick, 0);

    let budget = WorkBudget::new(1, 3);
    let (session_after_idle, idle) = pump_promptly(session, budget);
    session = session_after_idle;
    assert_eq!((idle.ticks, idle.events, idle.snapshot_tick), (1, 0, 1));
    assert!(!idle.complete);
    assert!(!idle.cancelled);
    assert_eq!(session.snapshot().tick, 1);

    for expected_tick in 2..=119 {
        let result = session.pump(budget).unwrap();
        assert_eq!(result.ticks, 1, "idle pump stalled at tick {expected_tick}");
        assert_eq!(result.events, 0);
        assert_eq!(result.snapshot_tick, expected_tick);
        assert!(!result.cancelled);
    }

    let (session_after_burst, burst) = pump_promptly(session, budget);
    session = session_after_burst;
    assert_eq!(
        (burst.ticks, burst.events, burst.snapshot_tick),
        (1, 3, 120)
    );
    assert!(!burst.complete);
    assert!(!burst.cancelled);

    let burst_snapshot = session.snapshot();
    assert_eq!(burst_snapshot.tick, 120);
    assert_eq!(burst_snapshot.actions.len(), 4);
    assert_eq!(
        burst_snapshot
            .actions
            .iter()
            .filter(|action| action.action == Action::Add)
            .count(),
        1
    );
    assert_eq!(
        burst_snapshot
            .actions
            .iter()
            .filter(|action| action.action == Action::Modify)
            .count(),
        3
    );

    for expected_tick in 121..=123 {
        let result = session.pump(budget).unwrap();
        assert_eq!(result.ticks, 1);
        assert_eq!(result.events, 0);
        assert_eq!(result.snapshot_tick, expected_tick);
        assert!(!result.cancelled);
    }

    assert_eq!(session.snapshot().tick, 123);
    assert_eq!(*session.snapshot(), *expected.snapshot());
}

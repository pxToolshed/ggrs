use ggrs::{
    Config, ExternalInputProvenance, ExternalInputReplacement, ExternalInputReplacementError,
    ExternalInputStateError, ExternalSession, GgrsError, GgrsRequest, InputStatus,
    PredictRepeatLast, SessionBuilder,
};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
struct TestInput {
    value: u8,
}

const A: TestInput = TestInput { value: 1 };
const B: TestInput = TestInput { value: 2 };

struct TestConfig;

impl Config for TestConfig {
    type Input = TestInput;
    type InputPredictor = PredictRepeatLast;
    type State = u32;
    type Address = ();
}

fn single_player_session() -> ExternalSession<TestConfig> {
    SessionBuilder::<TestConfig>::new()
        .with_num_players(1)
        .unwrap()
        .start_external_session()
}

fn fulfill_requests(
    session: &mut ExternalSession<TestConfig>,
    inputs: &[Option<TestInput>],
) -> Vec<(TestInput, InputStatus)> {
    let requests = session.advance_frame(inputs).unwrap();
    let mut advance_inputs = None;
    for request in requests {
        match request {
            GgrsRequest::SaveGameState { cell, frame } => {
                cell.save(frame, Some(frame as u32), None);
            }
            GgrsRequest::AdvanceFrame { inputs } => advance_inputs = Some(inputs),
            GgrsRequest::LoadGameState { .. } => panic!("unexpected load request"),
        }
    }
    advance_inputs.unwrap()
}

#[test]
fn advance_frame_requests_save_then_advance_and_increments_once() {
    let mut session = SessionBuilder::<TestConfig>::new()
        .with_num_players(1)
        .unwrap()
        .start_external_session();
    let requests = session.advance_frame(&[None]).unwrap();

    assert_eq!(session.current_frame(), 1);
    assert!(matches!(
        requests[0],
        GgrsRequest::SaveGameState { frame: 0, .. }
    ));
    assert!(matches!(requests[1], GgrsRequest::AdvanceFrame { .. }));
}

#[test]
fn advance_frame_uses_exact_player_count_and_handle_order() {
    let mut session = SessionBuilder::<TestConfig>::new()
        .with_num_players(3)
        .unwrap()
        .start_external_session();
    let inputs = [
        Some(TestInput { value: 10 }),
        Some(TestInput { value: 20 }),
        Some(TestInput { value: 30 }),
    ];

    let requests = session.advance_frame(&inputs).unwrap();
    let advance_inputs = requests
        .into_iter()
        .find_map(|request| match request {
            GgrsRequest::AdvanceFrame { inputs } => Some(inputs),
            GgrsRequest::SaveGameState { .. } => None,
            GgrsRequest::LoadGameState { .. } => panic!("unexpected load request"),
        })
        .unwrap();
    assert_eq!(
        advance_inputs,
        vec![
            (TestInput { value: 10 }, InputStatus::Confirmed),
            (TestInput { value: 20 }, InputStatus::Confirmed),
            (TestInput { value: 30 }, InputStatus::Confirmed),
        ]
    );
    assert_eq!(session.current_frame(), 1);
}

#[test]
fn none_is_default_and_predicted() {
    let mut session = single_player_session();
    assert_eq!(
        fulfill_requests(&mut session, &[None])[0],
        (TestInput::default(), InputStatus::Predicted)
    );
}

#[test]
fn none_and_some_default_have_distinct_statuses() {
    let mut session = single_player_session();
    assert_eq!(
        fulfill_requests(&mut session, &[None])[0].0,
        fulfill_requests(&mut session, &[Some(TestInput::default())])[0].0
    );
    let mut session = single_player_session();
    assert_eq!(
        fulfill_requests(&mut session, &[None])[0].1,
        InputStatus::Predicted
    );
    assert_eq!(
        fulfill_requests(&mut session, &[Some(TestInput::default())])[0].1,
        InputStatus::Confirmed
    );
}

#[test]
fn none_does_not_repeat_last_input_with_repeat_predictor() {
    let mut session = single_player_session();
    fulfill_requests(&mut session, &[Some(TestInput { value: 7 })]);
    assert_eq!(
        fulfill_requests(&mut session, &[None])[0],
        (TestInput::default(), InputStatus::Predicted)
    );
}

#[test]
fn mixed_authoritative_and_predicted_sequence_preserves_values_and_statuses() {
    let mut session = single_player_session();
    assert_eq!(
        fulfill_requests(&mut session, &[Some(TestInput { value: 3 })])[0],
        (TestInput { value: 3 }, InputStatus::Confirmed)
    );
    assert_eq!(
        fulfill_requests(&mut session, &[None])[0],
        (TestInput::default(), InputStatus::Predicted)
    );
    assert_eq!(
        fulfill_requests(&mut session, &[Some(TestInput { value: 9 })])[0],
        (TestInput { value: 9 }, InputStatus::Confirmed)
    );
}

#[test]
fn dense_external_saves_are_tagged_with_each_frame() {
    let mut session = single_player_session();
    for frame in 0..3 {
        let requests = session.advance_frame(&[None]).unwrap();
        assert!(matches!(
            requests[0],
            GgrsRequest::SaveGameState { frame: saved, .. } if saved == frame
        ));
        for request in requests {
            if let GgrsRequest::SaveGameState { cell, frame } = request {
                cell.save(frame, Some(frame as u32), None);
            }
        }
    }
}

#[test]
fn zero_and_small_history_retain_mixed_input_progress() {
    for history in [0, 2] {
        let mut session = SessionBuilder::<TestConfig>::new()
            .with_rollback_history_frames(history)
            .with_num_players(1)
            .unwrap()
            .start_external_session();
        for frame in 0..5 {
            let input = (frame % 2 == 0).then_some(TestInput { value: frame });
            let result = fulfill_requests(&mut session, &[input]);
            assert_eq!(result[0].0, input.unwrap_or_default());
        }
        assert_eq!(session.current_frame(), 5);
    }
}

#[test]
fn invalid_input_length_is_atomic_and_next_valid_call_starts_at_frame_zero() {
    let mut session = SessionBuilder::<TestConfig>::new()
        .with_num_players(2)
        .unwrap()
        .start_external_session();
    match session.advance_frame(&[None]) {
        Err(GgrsError::InvalidRequest { info }) => {
            assert_eq!(info, "Expected 2 inputs, got 1.");
        }
        _ => panic!("wrong error for invalid input length"),
    }
    assert_eq!(session.current_frame(), 0);
    let requests = session
        .advance_frame(&[None, Some(TestInput { value: 4 })])
        .unwrap();
    assert!(matches!(
        requests[0],
        GgrsRequest::SaveGameState { frame: 0, .. }
    ));
    assert!(matches!(requests[1], GgrsRequest::AdvanceFrame { .. }));
    assert_eq!(session.current_frame(), 1);
}

#[test]
fn history_127_wraps_snapshot_ring_at_frame_128() {
    let mut session = SessionBuilder::<TestConfig>::new()
        .with_rollback_history_frames(127)
        .with_num_players(1)
        .unwrap()
        .start_external_session();
    let mut frame_zero_cell = None;
    let mut frame_one_cell = None;
    for frame in 0..300 {
        let requests = session
            .advance_frame(&[Some(TestInput { value: frame as u8 })])
            .unwrap();
        let mut advance_inputs = None;
        for request in requests {
            if let GgrsRequest::SaveGameState { cell, frame: saved } = request {
                cell.save(saved, Some(saved as u32), None);
                if saved == 0 {
                    frame_zero_cell = Some(cell.clone());
                }
                if saved == 1 {
                    frame_one_cell = Some(cell.clone());
                }
            } else if let GgrsRequest::AdvanceFrame { inputs } = request {
                advance_inputs = Some(inputs);
            }
        }
        assert_eq!(
            advance_inputs.unwrap(),
            vec![(TestInput { value: frame as u8 }, InputStatus::Confirmed)]
        );
        if frame == 127 {
            assert_eq!(frame_zero_cell.as_ref().unwrap().load(), Some(0));
        }
        if frame == 128 {
            assert_eq!(frame_zero_cell.as_ref().unwrap().load(), Some(128));
        }
        if frame == 256 {
            assert_eq!(frame_zero_cell.as_ref().unwrap().load(), Some(256));
        }
        if frame == 128 {
            assert_eq!(frame_one_cell.as_ref().unwrap().load(), Some(1));
        }
    }
    assert_eq!(session.current_frame(), 300);
}

#[test]
#[should_panic(expected = "rollback history must be at most 127 frames")]
fn history_128_is_rejected_by_builder() {
    let _ = SessionBuilder::<TestConfig>::new().with_rollback_history_frames(128);
}

#[test]
fn external_session_uses_configured_values_without_transport_setup() {
    let session: ExternalSession<TestConfig> = SessionBuilder::new()
        .with_num_players(4)
        .unwrap()
        .with_rollback_history_frames(12)
        .start_external_session();

    assert_eq!(session.num_players(), 4);
    assert_eq!(session.rollback_history_frames(), 12);
    assert_eq!(session.current_frame(), 0);
}

#[test]
fn external_session_allows_zero_history() {
    let session = SessionBuilder::<TestConfig>::new()
        .with_rollback_history_frames(0)
        .start_external_session();

    assert_eq!(session.rollback_history_frames(), 0);
}

#[test]
fn external_history_is_independent_from_prediction_window() {
    let session = SessionBuilder::<TestConfig>::new()
        .with_max_prediction_window(3)
        .with_rollback_history_frames(11)
        .start_external_session();

    assert_eq!(session.rollback_history_frames(), 11);
}

#[test]
fn public_input_lookup_exposes_value_and_provenance() {
    let mut session = single_player_session();
    fulfill_requests(&mut session, &[None]);
    let state = session.input_state(0, 0).unwrap();

    assert_eq!(state.input(), TestInput::default());
    assert_eq!(state.provenance(), ExternalInputProvenance::Predicted);
}

#[test]
fn public_replacement_returns_state_and_exact_retry_is_no_op() {
    let mut session = single_player_session();
    fulfill_requests(&mut session, &[None]);
    let expected = session.input_state(0, 0).unwrap();
    let replacement = TestInput { value: 9 };

    let result = session
        .replace_past_input(0, 0, expected, replacement)
        .unwrap();
    let resulting = match result {
        ExternalInputReplacement::Replaced(state) => state,
        ExternalInputReplacement::RetryNoOp(_) => panic!("first replacement unexpectedly retried"),
    };
    assert_eq!(resulting.input(), replacement);
    assert_eq!(
        resulting.provenance(),
        ExternalInputProvenance::Authoritative
    );

    assert!(matches!(
        session.replace_past_input(0, 0, expected, replacement),
        Ok(ExternalInputReplacement::RetryNoOp(state))
            if state == resulting
    ));
}

#[test]
fn public_replacement_maps_errors_and_does_not_mutate_on_mismatch() {
    let mut session = single_player_session();
    fulfill_requests(&mut session, &[None]);
    let expected = session.input_state(0, 0).unwrap();
    fulfill_requests(&mut session, &[Some(TestInput { value: 8 })]);
    let wrong_expected = session.input_state(0, 1).unwrap();

    assert_eq!(
        session.input_state(1, 0),
        Err(ExternalInputStateError::InvalidHandle)
    );
    assert_eq!(
        session.input_state(0, -1),
        Err(ExternalInputStateError::InvalidFrame)
    );
    assert_eq!(
        session.input_state(0, 2),
        Err(ExternalInputStateError::Unavailable)
    );
    assert_eq!(
        session.replace_past_input(1, 0, expected, TestInput { value: 1 }),
        Err(ExternalInputReplacementError::InvalidHandle)
    );
    assert_eq!(
        session.replace_past_input(0, -1, expected, TestInput { value: 1 }),
        Err(ExternalInputReplacementError::InvalidFrame)
    );

    let replacement = TestInput { value: 3 };
    session
        .replace_past_input(0, 0, expected, replacement)
        .unwrap();
    assert_eq!(
        session.replace_past_input(0, 0, wrong_expected, TestInput { value: 4 }),
        Err(ExternalInputReplacementError::ExpectedStateMismatch)
    );
    assert_eq!(session.input_state(0, 0).unwrap().input(), replacement);
}

#[test]
fn missing_mandatory_save_state_maps_to_snapshot_unavailable() {
    let mut session = single_player_session();
    // This setup represents an unfulfilled mandatory SaveGameState request.
    session.advance_frame(&[None]).unwrap();
    let expected = session.input_state(0, 0).unwrap();

    assert_eq!(
        session.replace_past_input(0, 0, expected, TestInput { value: 2 }),
        Err(ExternalInputReplacementError::SnapshotUnavailable)
    );
}

#[derive(Debug, PartialEq)]
struct TraceEntry {
    request: &'static str,
    frame: i32,
    input: Option<(TestInput, InputStatus)>,
}

struct Driver {
    frame: i32,
    state: u32,
    trace: Vec<TraceEntry>,
}

impl Driver {
    fn new() -> Self {
        Self {
            frame: 0,
            state: 7,
            trace: Vec::new(),
        }
    }

    fn process(&mut self, requests: Vec<GgrsRequest<TestConfig>>) {
        for request in requests {
            match request {
                GgrsRequest::SaveGameState { cell, frame } => {
                    self.trace.push(TraceEntry {
                        request: "SaveGameState",
                        frame,
                        input: None,
                    });
                    cell.save(frame, Some(self.state), None);
                }
                GgrsRequest::LoadGameState { cell, frame } => {
                    self.trace.push(TraceEntry {
                        request: "LoadGameState",
                        frame,
                        input: None,
                    });
                    self.state = cell.load().unwrap();
                    self.frame = frame;
                }
                GgrsRequest::AdvanceFrame { inputs } => {
                    assert_eq!(inputs.len(), 1);
                    self.trace.push(TraceEntry {
                        request: "AdvanceFrame",
                        frame: self.frame,
                        input: Some(inputs[0]),
                    });
                    self.state = self
                        .state
                        .wrapping_mul(31)
                        .wrapping_add(inputs[0].0.value as u32 + 1);
                    self.frame += 1;
                }
            }
        }
    }
}

fn advance_with_driver(
    session: &mut ExternalSession<TestConfig>,
    driver: &mut Driver,
    input: Option<TestInput>,
) {
    driver.process(session.advance_frame(&[input]).unwrap());
}

#[test]
fn replacement_replays_predicted_frame_before_forward_advance() {
    let mut session = single_player_session();
    let mut driver = Driver::new();
    advance_with_driver(&mut session, &mut driver, None);
    let expected = session.input_state(0, 0).unwrap();
    assert_eq!(expected.input(), TestInput::default());
    assert_eq!(expected.provenance(), ExternalInputProvenance::Predicted);
    session.replace_past_input(0, 0, expected, A).unwrap();
    driver.trace.clear();

    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));

    assert_eq!(
        driver.trace,
        vec![
            TraceEntry {
                request: "LoadGameState",
                frame: 0,
                input: None,
            },
            TraceEntry {
                request: "AdvanceFrame",
                frame: 0,
                input: Some((A, InputStatus::Confirmed)),
            },
            TraceEntry {
                request: "SaveGameState",
                frame: 1,
                input: None,
            },
            TraceEntry {
                request: "AdvanceFrame",
                frame: 1,
                input: Some((TestInput::default(), InputStatus::Confirmed)),
            },
        ]
    );
    assert_eq!(session.current_frame(), 2);
    let expected_state = 7u32
        .wrapping_mul(31)
        .wrapping_add(A.value as u32 + 1)
        .wrapping_mul(31)
        .wrapping_add(1);
    assert_eq!(driver.state, expected_state);

    let mut control_session = single_player_session();
    let mut control_driver = Driver::new();
    advance_with_driver(&mut control_session, &mut control_driver, Some(A));
    advance_with_driver(
        &mut control_session,
        &mut control_driver,
        Some(TestInput::default()),
    );
    assert_eq!(driver.state, control_driver.state);
    driver.trace.clear();
    control_driver.trace.clear();
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    advance_with_driver(
        &mut control_session,
        &mut control_driver,
        Some(TestInput::default()),
    );
    assert!(!driver
        .trace
        .iter()
        .any(|entry| entry.request == "LoadGameState"));
    assert_eq!(driver.state, control_driver.state);
}

#[test]
fn replacement_replays_authoritative_frame_with_new_input() {
    let mut session = single_player_session();
    let mut driver = Driver::new();
    advance_with_driver(&mut session, &mut driver, Some(A));
    let expected = session.input_state(0, 0).unwrap();
    assert_eq!(expected.input(), A);
    assert_eq!(
        expected.provenance(),
        ExternalInputProvenance::Authoritative
    );
    session
        .replace_past_input(
            0,
            0,
            expected,
            TestInput {
                value: A.value | B.value,
            },
        )
        .unwrap();
    driver.trace.clear();

    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));

    assert_eq!(
        driver.trace,
        vec![
            TraceEntry {
                request: "LoadGameState",
                frame: 0,
                input: None,
            },
            TraceEntry {
                request: "AdvanceFrame",
                frame: 0,
                input: Some((TestInput { value: 3 }, InputStatus::Confirmed)),
            },
            TraceEntry {
                request: "SaveGameState",
                frame: 1,
                input: None,
            },
            TraceEntry {
                request: "AdvanceFrame",
                frame: 1,
                input: Some((TestInput::default(), InputStatus::Confirmed)),
            },
        ]
    );
    assert_eq!(session.current_frame(), 2);
    let expected_state = 7u32
        .wrapping_mul(31)
        .wrapping_add(4)
        .wrapping_mul(31)
        .wrapping_add(1);
    assert_eq!(driver.state, expected_state);

    let mut control_session = single_player_session();
    let mut control_driver = Driver::new();
    advance_with_driver(
        &mut control_session,
        &mut control_driver,
        Some(TestInput { value: 3 }),
    );
    advance_with_driver(
        &mut control_session,
        &mut control_driver,
        Some(TestInput::default()),
    );
    assert_eq!(driver.state, control_driver.state);
    driver.trace.clear();
    control_driver.trace.clear();
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    advance_with_driver(
        &mut control_session,
        &mut control_driver,
        Some(TestInput::default()),
    );
    assert!(!driver
        .trace
        .iter()
        .any(|entry| entry.request == "LoadGameState"));
    assert_eq!(driver.state, control_driver.state);
}

#[test]
fn replacement_before_prediction_window_is_snapshot_unavailable_and_atomic() {
    let mut session = SessionBuilder::<TestConfig>::new()
        .with_rollback_history_frames(2)
        .with_num_players(1)
        .unwrap()
        .start_external_session();
    let mut driver = Driver::new();
    for _ in 0..3 {
        advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    }
    let expected = session.input_state(0, 0).unwrap();
    assert_eq!(
        session.replace_past_input(0, 0, expected, A),
        Err(ExternalInputReplacementError::SnapshotUnavailable)
    );
    assert_eq!(session.input_state(0, 0).unwrap(), expected);
    driver.trace.clear();
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    assert!(!driver
        .trace
        .iter()
        .any(|entry| entry.request == "LoadGameState"));
}

#[test]
fn zero_history_rejects_every_past_replacement() {
    let mut session = SessionBuilder::<TestConfig>::new()
        .with_rollback_history_frames(0)
        .with_num_players(1)
        .unwrap()
        .start_external_session();
    let mut driver = Driver::new();
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    let expected = session.input_state(0, 0).unwrap();
    let before = expected;

    assert_eq!(
        session.replace_past_input(0, 0, expected, A),
        Err(ExternalInputReplacementError::SnapshotUnavailable)
    );
    assert_eq!(session.input_state(0, 0).unwrap(), before);
    driver.trace.clear();
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    assert!(!driver
        .trace
        .iter()
        .any(|entry| entry.request == "LoadGameState"));
}

#[test]
fn oldest_frame_inside_prediction_window_remains_replayable() {
    let mut session = SessionBuilder::<TestConfig>::new()
        .with_rollback_history_frames(2)
        .with_num_players(1)
        .unwrap()
        .start_external_session();
    let mut driver = Driver::new();
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));
    let expected = session.input_state(0, 1).unwrap();

    assert!(matches!(
        session.replace_past_input(0, 1, expected, A),
        Ok(ExternalInputReplacement::Replaced(_))
    ));
    driver.trace.clear();
    advance_with_driver(&mut session, &mut driver, Some(TestInput::default()));

    assert_eq!(
        driver.trace,
        vec![
            TraceEntry {
                request: "LoadGameState",
                frame: 1,
                input: None,
            },
            TraceEntry {
                request: "AdvanceFrame",
                frame: 1,
                input: Some((A, InputStatus::Confirmed)),
            },
            TraceEntry {
                request: "SaveGameState",
                frame: 2,
                input: None,
            },
            TraceEntry {
                request: "AdvanceFrame",
                frame: 2,
                input: Some((TestInput::default(), InputStatus::Confirmed)),
            },
            TraceEntry {
                request: "SaveGameState",
                frame: 3,
                input: None,
            },
            TraceEntry {
                request: "AdvanceFrame",
                frame: 3,
                input: Some((TestInput::default(), InputStatus::Confirmed)),
            },
        ]
    );
    assert_eq!(session.current_frame(), 4);

    let mut control_session = SessionBuilder::<TestConfig>::new()
        .with_rollback_history_frames(2)
        .with_num_players(1)
        .unwrap()
        .start_external_session();
    let mut control_driver = Driver::new();
    advance_with_driver(
        &mut control_session,
        &mut control_driver,
        Some(TestInput::default()),
    );
    advance_with_driver(&mut control_session, &mut control_driver, Some(A));
    advance_with_driver(
        &mut control_session,
        &mut control_driver,
        Some(TestInput::default()),
    );
    advance_with_driver(
        &mut control_session,
        &mut control_driver,
        Some(TestInput::default()),
    );
    assert_eq!(driver.state, control_driver.state);
}

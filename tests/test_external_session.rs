use ggrs::{
    Config, ExternalSession, GgrsError, GgrsRequest, InputStatus, PredictRepeatLast, SessionBuilder,
};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
struct TestInput {
    value: u8,
}

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

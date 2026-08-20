use ggrs::{Config, ExternalSession, PredictRepeatLast, SessionBuilder};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, PartialEq, Default, Serialize, Deserialize)]
struct TestInput;

struct TestConfig;

impl Config for TestConfig {
    type Input = TestInput;
    type InputPredictor = PredictRepeatLast;
    type State = ();
    type Address = ();
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

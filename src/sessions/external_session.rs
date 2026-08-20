use crate::{
    input_queue::InputProvenance, sync_layer::SyncLayer, Config, Frame, GgrsError, GgrsRequest,
};

/// A transport-free session backed by GGRS synchronization state.
pub struct ExternalSession<T>
where
    T: Config,
{
    sync_layer: SyncLayer<T>,
}

impl<T: Config> ExternalSession<T> {
    pub(crate) fn new(num_players: usize, rollback_history_frames: usize) -> Self {
        Self {
            sync_layer: SyncLayer::new(num_players, rollback_history_frames),
        }
    }

    /// Returns the configured number of players.
    pub fn num_players(&self) -> usize {
        self.sync_layer.num_players()
    }

    /// Returns the number of previous frames available for rollback.
    pub fn rollback_history_frames(&self) -> usize {
        self.sync_layer.rollback_history_frames()
    }

    /// Returns the current session frame.
    pub fn current_frame(&self) -> Frame {
        self.sync_layer.current_frame()
    }

    /// Advances one frame without transport.
    ///
    /// `None` uses the input default but marks it as predicted; `Some` marks the input as
    /// authoritative, including when it contains the default value.
    ///
    /// # Errors
    /// Returns [`GgrsError::InvalidRequest`] if `inputs.len()` does not equal `num_players()`.
    pub fn advance_frame(
        &mut self,
        inputs: &[Option<T::Input>],
    ) -> Result<Vec<GgrsRequest<T>>, GgrsError> {
        if inputs.len() != self.num_players() {
            return Err(GgrsError::InvalidRequest {
                info: format!(
                    "Expected {} inputs, got {}.",
                    self.num_players(),
                    inputs.len()
                ),
            });
        }

        let frame = self.current_frame();
        let history = self.rollback_history_frames() as Frame;
        let trim_through = if frame > history {
            frame - history - 1
        } else {
            -1
        };
        assert!(self
            .sync_layer
            .preflight_external_inputs(frame, trim_through));
        self.sync_layer.trim_external_retention(trim_through);

        for (handle, input) in inputs.iter().enumerate() {
            let (input, provenance) = match input {
                Some(input) => (*input, InputProvenance::Authoritative),
                None => (T::Input::default(), InputProvenance::Predicted),
            };
            self.sync_layer
                .append_external_input(handle as _, frame, input, provenance);
        }

        let requests = vec![
            self.sync_layer.save_current_state(),
            GgrsRequest::AdvanceFrame {
                inputs: self.sync_layer.external_inputs(frame),
            },
        ];
        self.sync_layer.advance_frame();
        Ok(requests)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, GgrsRequest, PredictRepeatLast};
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

    fn save_frame(session: &mut ExternalSession<TestConfig>) {
        if let GgrsRequest::SaveGameState { cell, frame } = session.sync_layer.save_current_state()
        {
            cell.save(frame, Some(()), None);
        }
    }

    #[test]
    fn positive_history_retains_previous_snapshots() {
        let mut session = ExternalSession::new(1, 2);
        save_frame(&mut session);
        session.sync_layer.advance_frame();
        save_frame(&mut session);
        session.sync_layer.advance_frame();
        save_frame(&mut session);
        session.sync_layer.advance_frame();
        save_frame(&mut session);

        assert!(session.sync_layer.saved_state_by_frame(1).is_some());
        assert!(session.sync_layer.saved_state_by_frame(0).is_none());
    }

    #[test]
    fn zero_history_retains_only_current_snapshot() {
        let mut session = ExternalSession::new(1, 0);
        save_frame(&mut session);
        session.sync_layer.advance_frame();
        save_frame(&mut session);

        assert!(session.sync_layer.saved_state_by_frame(0).is_none());
    }
}

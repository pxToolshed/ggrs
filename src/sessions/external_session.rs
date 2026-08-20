use crate::{
    input_queue::{InputProvenance, InputReplacementError, InputReplacementResult, VersionedInput},
    sync_layer::SyncLayer,
    Config, Frame, GgrsError, GgrsRequest, PlayerHandle,
};

/// Identifies whether an external input was predicted or authoritative.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ExternalInputProvenance {
    /// The input was supplied by prediction.
    Predicted,
    /// The input was supplied authoritatively.
    Authoritative,
}

/// An opaque, versioned input value snapshot used for compare-and-swap replacement.
///
/// Equality includes a hidden revision number as well as the input and provenance. Obtain a
/// snapshot from [`ExternalSession::input_state`] and pass that snapshot back only for the same
/// player and frame. This is a value snapshot, not a session-bound capability.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct ExternalInputState<I> {
    input: I,
    provenance: ExternalInputProvenance,
    revision: u64,
}

impl<I: Eq> Eq for ExternalInputState<I> {}

impl<I> ExternalInputState<I> {
    /// Returns the input in this snapshot.
    pub fn input(&self) -> I
    where
        I: Copy,
    {
        self.input
    }

    /// Returns the provenance in this snapshot.
    pub fn provenance(&self) -> ExternalInputProvenance {
        self.provenance
    }
}

/// An error returned when a requested retained input state cannot be read.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ExternalInputStateError {
    /// The player handle is not configured for this session.
    InvalidHandle,
    /// The frame is negative.
    InvalidFrame,
    /// The input is unavailable or no longer retained.
    Unavailable,
}

/// The outcome of replacing a retained past input.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ExternalInputReplacement<I> {
    /// The expected state was replaced; this is the resulting state. Use it for a later legitimate
    /// revision of the same player and frame.
    Replaced(ExternalInputState<I>),
    /// The exact last successful replacement was repeated; no mutation was needed, and this is
    /// the resulting state.
    RetryNoOp(ExternalInputState<I>),
}

/// An error returned when a past input cannot be replaced.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ExternalInputReplacementError {
    /// The player handle is not configured for this session.
    InvalidHandle,
    /// The frame is negative.
    InvalidFrame,
    /// The requested frame is not in the past.
    NotPast,
    /// The requested frame has been finalized.
    Finalized,
    /// The input is no longer retained.
    InputOutOfRetention,
    /// The mandatory saved state for this frame is unavailable.
    SnapshotUnavailable,
    /// The expected input state no longer matches.
    ExpectedStateMismatch,
    /// A different replacement has already won the compare-and-swap.
    Conflict,
    /// The input revision cannot be incremented.
    RevisionExhausted,
}

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

    /// Returns the retained input state for one player and frame.
    ///
    /// The returned opaque snapshot includes a hidden revision used by CAS replacement. An input
    /// that has not been materialized or is no longer retained returns [`Unavailable`](ExternalInputStateError::Unavailable).
    pub fn input_state(
        &self,
        player: PlayerHandle,
        frame: Frame,
    ) -> Result<ExternalInputState<T::Input>, ExternalInputStateError> {
        if player >= self.num_players() {
            return Err(ExternalInputStateError::InvalidHandle);
        }
        if frame < 0 {
            return Err(ExternalInputStateError::InvalidFrame);
        }
        self.sync_layer
            .input_state(player, frame)
            .map(ExternalInputState::from)
            .ok_or(ExternalInputStateError::Unavailable)
    }

    /// Replaces one retained past input if its state still matches `expected`.
    ///
    /// `expected` must be the snapshot obtained from [`Self::input_state`] for this same player
    /// and frame. Repeat the exact last successful operation to receive [`RetryNoOp`](ExternalInputReplacement::RetryNoOp);
    /// use the state returned by the preceding success for a later legitimate revision.
    pub fn replace_past_input(
        &mut self,
        player: PlayerHandle,
        frame: Frame,
        expected: ExternalInputState<T::Input>,
        replacement: T::Input,
    ) -> Result<ExternalInputReplacement<T::Input>, ExternalInputReplacementError> {
        let result =
            self.sync_layer
                .replace_past_input(player, frame, expected.into(), replacement);
        let result = result.map_err(ExternalInputReplacementError::from)?;
        let state = self
            .sync_layer
            .input_state(player, frame)
            .map(ExternalInputState::from)
            .expect("successful replacement must retain its resulting input state");
        Ok(match result {
            InputReplacementResult::Replaced => ExternalInputReplacement::Replaced(state),
            InputReplacementResult::RetryNoOp => ExternalInputReplacement::RetryNoOp(state),
        })
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

impl<I> From<VersionedInput<I>> for ExternalInputState<I> {
    fn from(value: VersionedInput<I>) -> Self {
        Self {
            input: value.input,
            provenance: match value.provenance {
                InputProvenance::Predicted => ExternalInputProvenance::Predicted,
                InputProvenance::Authoritative => ExternalInputProvenance::Authoritative,
            },
            revision: value.revision,
        }
    }
}

impl<I> From<ExternalInputState<I>> for VersionedInput<I> {
    fn from(value: ExternalInputState<I>) -> Self {
        Self {
            input: value.input,
            provenance: match value.provenance {
                ExternalInputProvenance::Predicted => InputProvenance::Predicted,
                ExternalInputProvenance::Authoritative => InputProvenance::Authoritative,
            },
            revision: value.revision,
        }
    }
}

impl From<InputReplacementError> for ExternalInputReplacementError {
    fn from(value: InputReplacementError) -> Self {
        match value {
            InputReplacementError::InvalidHandle => Self::InvalidHandle,
            InputReplacementError::InvalidFrame => Self::InvalidFrame,
            InputReplacementError::NotPast => Self::NotPast,
            InputReplacementError::Finalized => Self::Finalized,
            InputReplacementError::SnapshotOutOfRetention => Self::SnapshotUnavailable,
            InputReplacementError::OutOfRetention => Self::InputOutOfRetention,
            InputReplacementError::ExpectedStateMismatch => Self::ExpectedStateMismatch,
            InputReplacementError::Conflict => Self::Conflict,
            InputReplacementError::RevisionOverflow => Self::RevisionExhausted,
        }
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

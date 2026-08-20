use crate::frame_info::PlayerInput;
use crate::{Config, Frame, InputPredictor, InputStatus, NULL_FRAME};
use std::cmp;

/// The length of the input queue. This describes the number of inputs GGRS can hold at the same time per player.
pub(crate) const INPUT_QUEUE_LENGTH: usize = 128;
pub(crate) const MAX_ROLLBACK_HISTORY_FRAMES: usize = INPUT_QUEUE_LENGTH - 1;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum InputProvenance {
    Predicted,
    Authoritative,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) struct VersionedInput<I> {
    pub input: I,
    pub provenance: InputProvenance,
    pub revision: u64,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub(crate) struct InputTransition<I> {
    pub expected: VersionedInput<I>,
    pub replacement: VersionedInput<I>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum InputReplacementError {
    InvalidHandle,
    InvalidFrame,
    NotPast,
    Finalized,
    SnapshotOutOfRetention,
    OutOfRetention,
    ExpectedStateMismatch,
    Conflict,
    RevisionOverflow,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum InputReplacementResult {
    Replaced,
    RetryNoOp,
}

/// `InputQueue` handles inputs for a single player and saves them in a circular array. Valid Inputs are between `head` and `tail`.
#[derive(Debug, Clone)]
pub(crate) struct InputQueue<T>
where
    T: Config,
{
    /// The head of the queue. The newest `PlayerInput` is saved here
    head: usize,
    /// The tail of the queue. The oldest `PlayerInput` still valid is saved here.
    tail: usize,
    /// The current length of the queue.
    length: usize,
    /// Denotes if we still are in the first frame, an edge case to be considered by some methods.
    first_frame: bool,

    /// The last frame added by the user
    last_added_frame: Frame,
    /// The last game frame submitted by the user (before frame delay is applied). Used to enforce
    /// sequential submission independent of the current frame_delay value.
    last_user_frame: Frame,
    /// The first frame in the queue that is known to be an incorrect prediction
    first_incorrect_frame: Frame,
    /// The last frame that has been requested. We make sure to never delete anything after this, as we would throw away important data.
    last_requested_frame: Frame,

    /// The delay in frames by which inputs are sent back to the user. This can be set during initialization.
    frame_delay: usize,

    /// Our cyclic input queue
    inputs: Vec<PlayerInput<T::Input>>,
    slots: Vec<VersionedInput<T::Input>>,
    last_transitions: Vec<Option<InputTransition<T::Input>>>,
    /// A pre-allocated prediction we are going to use to return predictions from.
    prediction: PlayerInput<T::Input>,
}

impl<T: Config> InputQueue<T> {
    fn prev_pos(head: usize) -> usize {
        if head == 0 {
            INPUT_QUEUE_LENGTH - 1
        } else {
            head - 1
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            head: 0,
            tail: 0,
            length: 0,
            frame_delay: 0,
            first_frame: true,
            last_added_frame: NULL_FRAME,
            last_user_frame: NULL_FRAME,
            first_incorrect_frame: NULL_FRAME,
            last_requested_frame: NULL_FRAME,
            prediction: PlayerInput::blank_input(NULL_FRAME),
            inputs: vec![PlayerInput::blank_input(NULL_FRAME); INPUT_QUEUE_LENGTH],
            slots: vec![
                VersionedInput {
                    input: T::Input::default(),
                    provenance: InputProvenance::Authoritative,
                    revision: 0,
                };
                INPUT_QUEUE_LENGTH
            ],
            last_transitions: vec![None; INPUT_QUEUE_LENGTH],
        }
    }

    pub(crate) fn first_incorrect_frame(&self) -> Frame {
        self.first_incorrect_frame
    }

    fn retained_index(&self, offset: usize) -> bool {
        self.length > 0
            && (offset + INPUT_QUEUE_LENGTH - self.tail) % INPUT_QUEUE_LENGTH < self.length
    }

    /// Changes the frame delay and returns any fill inputs that were implicitly added to bridge the
    /// gap. The caller is responsible for sending these to remote peers so they see consecutive
    /// frame numbers.
    pub(crate) fn set_frame_delay(&mut self, delay: usize) -> Vec<PlayerInput<T::Input>> {
        let old_delay = self.frame_delay;
        self.frame_delay = delay;

        if delay <= old_delay || self.last_added_frame == NULL_FRAME {
            return Vec::new();
        }

        let fill_count = delay - old_delay;
        let fill_start = self.last_added_frame + 1;
        let last_input = self.inputs[Self::prev_pos(self.head)];
        (0..fill_count as i32)
            .map(|i| PlayerInput::new(fill_start + i, last_input.input))
            .collect()
    }

    pub(crate) fn reset_prediction(&mut self) {
        self.prediction.frame = NULL_FRAME;
        self.first_incorrect_frame = NULL_FRAME;
        self.last_requested_frame = NULL_FRAME;
    }

    pub(crate) fn slot_state(&self, frame: Frame) -> Option<VersionedInput<T::Input>> {
        if frame < 0 {
            return None;
        }
        let offset = frame as usize % INPUT_QUEUE_LENGTH;
        let slot = &self.slots[offset];
        (self.retained_index(offset) && self.inputs[offset].frame == frame).then_some(*slot)
    }

    pub(crate) fn replace_past_slot(
        &mut self,
        frame: Frame,
        expected: VersionedInput<T::Input>,
        input: T::Input,
    ) -> Result<InputReplacementResult, InputReplacementError> {
        if frame < 0 {
            return Err(InputReplacementError::InvalidFrame);
        }
        let offset = frame as usize % INPUT_QUEUE_LENGTH;
        if !self.retained_index(offset) || self.inputs[offset].frame != frame {
            return Err(InputReplacementError::OutOfRetention);
        }
        let current = self.slots[offset];
        if let Some(transition) = self.last_transitions[offset] {
            if transition.expected == expected
                && transition.replacement.input == input
                && transition.replacement.provenance == InputProvenance::Authoritative
                && current == transition.replacement
            {
                return Ok(InputReplacementResult::RetryNoOp);
            }
            if transition.expected == expected
                && current == transition.replacement
                && input != transition.replacement.input
            {
                return Err(InputReplacementError::Conflict);
            }
            if transition.replacement.input == input
                && transition.replacement.provenance == InputProvenance::Authoritative
                && current != expected
            {
                return Err(InputReplacementError::Conflict);
            }
        }
        if current != expected {
            return if current.input == input && current.provenance == InputProvenance::Authoritative
            {
                Err(InputReplacementError::Conflict)
            } else {
                Err(InputReplacementError::ExpectedStateMismatch)
            };
        }
        if current.input == input && current.provenance == InputProvenance::Authoritative {
            return Err(InputReplacementError::Conflict);
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(InputReplacementError::RevisionOverflow)?;
        let replacement = VersionedInput {
            input,
            provenance: InputProvenance::Authoritative,
            revision,
        };
        self.inputs[offset].input = input;
        self.slots[offset] = replacement;
        self.last_transitions[offset] = Some(InputTransition {
            expected,
            replacement,
        });
        if current.input != input
            && (self.first_incorrect_frame == NULL_FRAME || frame < self.first_incorrect_frame)
        {
            self.first_incorrect_frame = frame;
        }
        Ok(InputReplacementResult::Replaced)
    }

    pub(crate) fn materialize_predicted(
        &mut self,
        frame: Frame,
        input: T::Input,
    ) -> Result<(), InputReplacementError> {
        if frame < 0 || (!self.first_frame && frame != self.last_added_frame + 1) {
            return Err(InputReplacementError::InvalidFrame);
        }
        if self.length == INPUT_QUEUE_LENGTH {
            return Err(InputReplacementError::OutOfRetention);
        }
        self.append_sequential(frame, input, InputProvenance::Predicted);
        Ok(())
    }

    pub(crate) fn can_append_after_trim(&self, frame: Frame, trim_through: Frame) -> bool {
        if frame < 0 || (!self.first_frame && frame != self.last_added_frame + 1) {
            return false;
        }
        let retained_after_trim = (0..INPUT_QUEUE_LENGTH)
            .filter(|&offset| {
                self.retained_index(offset) && self.inputs[offset].frame > trim_through
            })
            .count();
        retained_after_trim < INPUT_QUEUE_LENGTH
    }

    pub(crate) fn current_input(&self, frame: Frame) -> Option<(T::Input, InputStatus)> {
        let slot = self.slot_state(frame)?;
        let status = match slot.provenance {
            InputProvenance::Predicted => InputStatus::Predicted,
            InputProvenance::Authoritative => InputStatus::Confirmed,
        };
        Some((slot.input, status))
    }

    pub(crate) fn append_sequential(
        &mut self,
        frame: Frame,
        input: T::Input,
        provenance: InputProvenance,
    ) {
        debug_assert!(self.first_frame || frame == self.last_added_frame + 1);
        assert!(self.length < INPUT_QUEUE_LENGTH);
        let offset = self.head;
        self.inputs[offset] = PlayerInput::new(frame, input);
        self.slots[offset] = VersionedInput {
            input,
            provenance,
            revision: 0,
        };
        self.last_transitions[offset] = None;
        self.head = (self.head + 1) % INPUT_QUEUE_LENGTH;
        self.length += 1;
        self.first_frame = false;
        self.last_added_frame = frame;
    }

    pub(crate) fn trim_external_through(&mut self, frame: Frame) {
        if frame < 0 || self.length == 0 {
            return;
        }
        let old_tail = self.tail;
        let old_length = self.length;
        let mut removed = 0;
        for offset in 0..INPUT_QUEUE_LENGTH {
            let retained = (offset + INPUT_QUEUE_LENGTH - old_tail) % INPUT_QUEUE_LENGTH;
            if retained < old_length && self.inputs[offset].frame <= frame {
                self.inputs[offset].frame = NULL_FRAME;
                self.slots[offset] = VersionedInput {
                    input: T::Input::default(),
                    provenance: InputProvenance::Authoritative,
                    revision: 0,
                };
                self.last_transitions[offset] = None;
                removed += 1;
            }
        }
        if removed == 0 {
            return;
        }
        while self.length > 0 {
            let offset = self.tail;
            if self.inputs[offset].frame != NULL_FRAME {
                break;
            }
            self.tail = (self.tail + 1) % INPUT_QUEUE_LENGTH;
            self.length -= 1;
        }
        if self.length == 0 {
            self.tail = self.head;
        }
    }

    /// Returns a `PlayerInput`, but only if the input for the requested frame is confirmed.
    /// In contrast to `input()`, this will not return a prediction if there is no confirmed input for the frame, but panic instead.
    pub(crate) fn confirmed_input(&self, requested_frame: Frame) -> PlayerInput<T::Input> {
        let offset = requested_frame as usize % INPUT_QUEUE_LENGTH;

        if self.inputs[offset].frame == requested_frame {
            return self.inputs[offset];
        }

        // the requested confirmed input should not be before a prediction. We should not have asked for a known incorrect frame.
        panic!("SyncLayer::confirmed_input(): There is no confirmed input for the requested frame");
    }

    /// Discards confirmed frames up to given `frame` from the queue. All confirmed frames are guaranteed to be synchronized between players, so there is no need to save the inputs anymore.
    pub(crate) fn discard_confirmed_frames(&mut self, mut frame: Frame) {
        if frame < 0 {
            return;
        }
        let old_tail = self.tail;
        let old_length = self.length;
        // we only drop frames until the last frame that was requested, otherwise we might delete data still needed
        if self.last_requested_frame != NULL_FRAME {
            frame = cmp::min(frame, self.last_requested_frame);
        }

        // move the tail to "delete inputs", wrap around if necessary
        if frame >= self.last_added_frame {
            // delete all but most recent
            self.tail = Self::prev_pos(self.head);
            self.length = 1;
        } else if frame <= self.inputs[self.tail].frame {
            // we don't need to delete anything
        } else {
            let offset = (frame - (self.inputs[self.tail].frame)) as usize;
            self.tail = (self.tail + offset) % INPUT_QUEUE_LENGTH;
            self.length -= offset;
        }
        for index in 0..INPUT_QUEUE_LENGTH {
            let was_retained = old_length > 0
                && (index + INPUT_QUEUE_LENGTH - old_tail) % INPUT_QUEUE_LENGTH < old_length;
            if was_retained && !self.retained_index(index) {
                self.inputs[index].frame = NULL_FRAME;
                self.slots[index] = VersionedInput {
                    input: T::Input::default(),
                    provenance: InputProvenance::Authoritative,
                    revision: 0,
                };
                self.last_transitions[index] = None;
            }
        }
    }

    /// Returns the game input of a single player for a given frame, if that input does not exist, we return a prediction instead.
    pub(crate) fn input(&mut self, requested_frame: Frame) -> (T::Input, InputStatus) {
        // No one should ever try to grab any input when we have a prediction error.
        // Doing so means that we're just going further down the wrong path. Assert this to verify that it's true.
        assert!(self.first_incorrect_frame == NULL_FRAME);

        // Remember the last requested frame number for later. We'll need this in add_input() to drop out of prediction mode.
        self.last_requested_frame = requested_frame;

        // assert that we request a frame that still exists
        assert!(requested_frame >= self.inputs[self.tail].frame);

        // We currently don't have a prediction frame
        if self.prediction.frame < 0 {
            //  If the frame requested is in our range, fetch it out of the queue and return it.
            let mut offset: usize = (requested_frame - self.inputs[self.tail].frame) as usize;

            if offset < self.length {
                offset = (offset + self.tail) % INPUT_QUEUE_LENGTH;
                assert!(self.inputs[offset].frame == requested_frame);
                return (self.inputs[offset].input, InputStatus::Confirmed);
            }

            // The requested frame isn't in the queue. This means we need to return a prediction frame.
            // Fetch the previous input if we have one, so we can use it to predict the next frame.
            let previous_player_input =
                if requested_frame == 0 || self.last_added_frame == NULL_FRAME {
                    None
                } else {
                    // basing new prediction frame from previously added frame
                    Some(self.inputs[Self::prev_pos(self.head)])
                };

            // Ask the user to predict the input based on the previous input (if any); if we don't
            // get a prediction from the user, default to the default input.
            let input_prediction = previous_player_input
                .map(|pi| T::InputPredictor::predict(pi.input))
                .unwrap_or_default();

            // Set the frame number of the predicted input to what it was based on
            self.prediction = {
                let frame_num = if let Some(previous_player_input) = previous_player_input {
                    previous_player_input.frame
                } else {
                    self.prediction.frame
                };
                PlayerInput::new(frame_num, input_prediction)
            };
            // update the prediction's frame
            self.prediction.frame += 1;
        }

        // We must be predicting, so we return the prediction frame contents. We are adjusting the prediction to have the requested frame.
        assert!(self.prediction.frame != NULL_FRAME);
        let prediction_to_return = self.prediction; // PlayerInput has copy semantics
        (prediction_to_return.input, InputStatus::Predicted)
    }

    /// Adds an input frame to the queue. Will consider the set frame delay.
    pub(crate) fn add_input(&mut self, input: PlayerInput<T::Input>) -> Frame {
        // Verify that inputs are passed in sequentially by the user. We compare against the raw
        // user frame (before delay is applied) so that changing frame_delay mid-session does not
        // break the sequential check.
        if self.last_user_frame != NULL_FRAME && input.frame != self.last_user_frame + 1 {
            // drop the input if not given sequentially
            return NULL_FRAME;
        }
        self.last_user_frame = input.frame;

        // Move the queue head to the correct point in preparation to input the frame into the queue.
        let new_frame = self.advance_queue_head(input.frame);
        // if the frame is valid, then add the input
        if new_frame != NULL_FRAME {
            self.add_input_by_frame(input, new_frame);
        }
        new_frame
    }

    /// Adds an input frame to the queue at the given frame number. If there are predicted inputs, we will check those and mark them as incorrect, if necessary.
    /// Returns the frame number
    fn add_input_by_frame(&mut self, input: PlayerInput<T::Input>, frame_number: Frame) {
        let previous_position = Self::prev_pos(self.head);

        assert!(self.last_added_frame == NULL_FRAME || frame_number == self.last_added_frame + 1);
        assert!(frame_number == 0 || self.inputs[previous_position].frame == frame_number - 1);

        // Add the frame to the back of the queue
        self.append_sequential(frame_number, input.input, InputProvenance::Authoritative);

        // We have been predicting. See if the inputs we've gotten match what we've been predicting. If so, don't worry about it.
        if self.prediction.frame != NULL_FRAME {
            assert!(frame_number == self.prediction.frame);

            // Remember the first input which was incorrect so we can report it
            if self.first_incorrect_frame == NULL_FRAME && !self.prediction.input_matches(&input) {
                self.first_incorrect_frame = frame_number;
            }

            // If this input is the same frame as the last one requested and we still haven't found any mispredicted inputs, we can exit prediction mode.
            // Otherwise, advance the prediction frame count up.
            if self.prediction.frame == self.last_requested_frame
                && self.first_incorrect_frame == NULL_FRAME
            {
                self.prediction.frame = NULL_FRAME;
            } else {
                self.prediction.frame += 1;
            }
        }
    }

    /// Advances the queue head to the next frame and either drops inputs or fills the queue if the input delay has changed since the last frame.
    fn advance_queue_head(&mut self, mut input_frame: Frame) -> Frame {
        let previous_position = Self::prev_pos(self.head);

        let mut expected_frame = if self.first_frame {
            0
        } else {
            self.inputs[previous_position].frame + 1
        };

        input_frame += self.frame_delay as i32;
        //  This can occur when the frame delay has dropped since the last time we shoved a frame into the system. In this case, there's no room on the queue. Toss it.
        if expected_frame > input_frame {
            return NULL_FRAME;
        }

        // This can occur when the frame delay has been increased since the last time we shoved a frame into the system.
        // We need to replicate the last frame in the queue several times in order to fill the space left.
        while expected_frame < input_frame {
            let input_to_replicate = self.inputs[previous_position];
            self.add_input_by_frame(input_to_replicate, expected_frame);
            expected_frame += 1;
        }

        assert!(
            input_frame == 0 || input_frame == self.inputs[Self::prev_pos(self.head)].frame + 1
        );
        input_frame
    }
}

// #########
// # TESTS #
// #########

#[cfg(test)]
mod input_queue_tests {

    use std::net::SocketAddr;
    use std::panic::AssertUnwindSafe;

    use serde::{Deserialize, Serialize};

    use crate::PredictRepeatLast;

    use super::*;

    #[repr(C)]
    #[derive(Debug, Copy, Clone, PartialEq, Default, Serialize, Deserialize)]
    struct TestInput {
        inp: u8,
    }

    struct TestConfig;

    impl Config for TestConfig {
        type Input = TestInput;
        type InputPredictor = PredictRepeatLast;
        type State = Vec<u8>;
        type Address = SocketAddr;
    }

    #[test]
    fn test_add_input_wrong_frame() {
        let mut queue = InputQueue::<TestConfig>::new();
        let input = PlayerInput::new(0, TestInput { inp: 0 });
        assert_eq!(queue.add_input(input), 0); // fine
        let input_wrong_frame = PlayerInput::new(3, TestInput { inp: 0 });
        assert_eq!(queue.add_input(input_wrong_frame), NULL_FRAME); // input dropped
    }

    #[test]
    fn test_add_input_twice() {
        let mut queue = InputQueue::<TestConfig>::new();
        let input = PlayerInput::new(0, TestInput { inp: 0 });
        assert_eq!(queue.add_input(input), 0); // fine
        assert_eq!(queue.add_input(input), NULL_FRAME); // input dropped
    }

    #[test]
    fn test_add_input_sequentially() {
        let mut queue = InputQueue::<TestConfig>::new();
        for i in 0..10 {
            let input = PlayerInput::new(i, TestInput { inp: 0 });
            queue.add_input(input);
            assert_eq!(queue.last_added_frame, i);
            assert_eq!(queue.length, (i + 1) as usize);
        }
    }

    #[test]
    fn test_input_sequentially() {
        let mut queue = InputQueue::<TestConfig>::new();
        for i in 0..10 {
            let input = PlayerInput::new(i, TestInput { inp: i as u8 });
            queue.add_input(input);
            assert_eq!(queue.last_added_frame, i);
            assert_eq!(queue.length, (i + 1) as usize);
            let (input_in_queue, _status) = queue.input(i);
            assert_eq!(input_in_queue.inp, i as u8);
        }
    }

    #[test]
    fn test_delayed_inputs() {
        let mut queue = InputQueue::<TestConfig>::new();
        let delay: i32 = 2;
        queue.set_frame_delay(delay as usize);
        for i in 0..10 {
            let input = PlayerInput::new(i, TestInput { inp: i as u8 });
            queue.add_input(input);
            assert_eq!(queue.last_added_frame, i + delay);
            assert_eq!(queue.length, (i + delay + 1) as usize);
            let (input_in_queue, _status) = queue.input(i);
            let correct_input = std::cmp::max(0, i - delay) as u8;
            assert_eq!(input_in_queue.inp, correct_input);
        }
    }

    #[test]
    fn test_prediction_returned_for_missing_frame() {
        let mut queue = InputQueue::<TestConfig>::new();
        let input = PlayerInput::new(0, TestInput { inp: 42 });
        queue.add_input(input);
        // frame 1 has not been added yet — should get a prediction
        let (_inp, status) = queue.input(1);
        assert_eq!(status, InputStatus::Predicted);
    }

    #[test]
    fn test_prediction_repeats_last_input() {
        let mut queue = InputQueue::<TestConfig>::new();
        let input = PlayerInput::new(0, TestInput { inp: 77 });
        queue.add_input(input);
        // prediction should repeat the last real input
        let (predicted, _status) = queue.input(1);
        assert_eq!(predicted.inp, 77);
    }

    #[test]
    fn shared_append_preserves_sequential_submission_and_prediction_mismatch() {
        let mut queue = InputQueue::<TestConfig>::new();
        assert_eq!(
            queue.add_input(PlayerInput::new(0, TestInput { inp: 5 })),
            0
        );
        assert_eq!(queue.input(1).0.inp, 5);
        assert_eq!(
            queue.add_input(PlayerInput::new(2, TestInput { inp: 99 })),
            NULL_FRAME
        );
        assert_eq!(
            queue.add_input(PlayerInput::new(1, TestInput { inp: 99 })),
            1
        );
        assert_eq!(queue.first_incorrect_frame(), 1);
    }

    #[test]
    fn full_queue_requires_trim_before_shared_append() {
        let mut queue = InputQueue::<TestConfig>::new();
        for frame in 0..INPUT_QUEUE_LENGTH as Frame {
            assert_eq!(
                queue.add_input(PlayerInput::new(frame, TestInput { inp: 1 })),
                frame
            );
        }
        assert_eq!(queue.length, INPUT_QUEUE_LENGTH);
        assert!(!queue.can_append_after_trim(INPUT_QUEUE_LENGTH as Frame, -1));
        assert!(queue.can_append_after_trim(INPUT_QUEUE_LENGTH as Frame, 0));

        let before = (
            queue.head,
            queue.tail,
            queue.length,
            queue.last_added_frame,
            queue.inputs.clone(),
            queue.slots.clone(),
        );
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            queue.append_sequential(
                INPUT_QUEUE_LENGTH as Frame,
                TestInput { inp: 9 },
                InputProvenance::Authoritative,
            );
        }));
        assert!(result.is_err());
        assert_eq!(
            (queue.head, queue.tail, queue.length, queue.last_added_frame,),
            (before.0, before.1, before.2, before.3)
        );
        assert_eq!(&queue.inputs, &before.4);
        assert_eq!(&queue.slots, &before.5);

        queue.trim_external_through(0);
        assert!(queue.can_append_after_trim(INPUT_QUEUE_LENGTH as Frame, 0));
        queue.append_sequential(
            INPUT_QUEUE_LENGTH as Frame,
            TestInput { inp: 9 },
            InputProvenance::Authoritative,
        );
        assert_eq!(
            queue.current_input(INPUT_QUEUE_LENGTH as Frame),
            Some((TestInput { inp: 9 }, InputStatus::Confirmed))
        );
    }

    #[test]
    fn test_confirmed_input_after_prediction_no_mismatch() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue.add_input(PlayerInput::new(0, TestInput { inp: 5 }));
        // trigger prediction for frame 1
        queue.input(1);
        // now add the real input for frame 1 matching the prediction
        queue.add_input(PlayerInput::new(1, TestInput { inp: 5 }));
        assert_eq!(queue.first_incorrect_frame(), NULL_FRAME);
    }

    #[test]
    fn test_first_incorrect_frame_tracked_on_mismatch() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue.add_input(PlayerInput::new(0, TestInput { inp: 5 }));
        // trigger prediction for frame 1 (predicts inp=5)
        queue.input(1);
        // add real input for frame 1 that differs from prediction
        queue.add_input(PlayerInput::new(1, TestInput { inp: 99 }));
        assert_eq!(queue.first_incorrect_frame(), 1);
    }

    #[test]
    fn test_reset_prediction_clears_state() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue.add_input(PlayerInput::new(0, TestInput { inp: 5 }));
        queue.input(1);
        queue.add_input(PlayerInput::new(1, TestInput { inp: 99 }));
        assert_eq!(queue.first_incorrect_frame(), 1);

        queue.reset_prediction();

        assert_eq!(queue.first_incorrect_frame(), NULL_FRAME);
        assert_eq!(queue.last_requested_frame, NULL_FRAME);
    }

    #[test]
    fn test_confirmed_input_returns_correct_value() {
        let mut queue = InputQueue::<TestConfig>::new();
        for i in 0..6 {
            queue.add_input(PlayerInput::new(i, TestInput { inp: i as u8 * 10 }));
        }
        let confirmed = queue.confirmed_input(3);
        assert_eq!(confirmed.frame, 3);
        assert_eq!(confirmed.input.inp, 30);
    }

    #[test]
    fn test_discard_confirmed_frames_reduces_length() {
        let mut queue = InputQueue::<TestConfig>::new();
        for i in 0..10 {
            queue.add_input(PlayerInput::new(i, TestInput { inp: i as u8 }));
        }
        let len_before = queue.length;
        queue.discard_confirmed_frames(5);
        assert!(queue.length < len_before);
    }

    #[test]
    fn test_increase_delay_mid_session_does_not_drop_next_input() {
        let mut queue = InputQueue::<TestConfig>::new();
        // Add a few frames with no delay
        for i in 0..5_i32 {
            let result = queue.add_input(PlayerInput::new(i, TestInput { inp: i as u8 }));
            assert_ne!(result, NULL_FRAME, "frame {i} should be accepted");
        }

        // Increase delay from 0 to 2
        queue.set_frame_delay(2);

        // The next sequential game frame (5) must still be accepted
        let result = queue.add_input(PlayerInput::new(5, TestInput { inp: 5 }));
        assert_ne!(
            result, NULL_FRAME,
            "first input after delay increase should not be dropped"
        );
    }

    #[test]
    fn test_increase_delay_fills_with_last_input() {
        let mut queue = InputQueue::<TestConfig>::new();
        // Add frames 0-4 with no delay; the last submitted input has inp=4
        for i in 0..5_i32 {
            queue.add_input(PlayerInput::new(i, TestInput { inp: i as u8 }));
        }

        // Increase delay from 0 to 2; the fills should be stamped with the last known input
        let fills = queue.set_frame_delay(2);

        assert_eq!(fills.len(), 2);
        // Both fill frames carry the last input value (inp=4)
        assert!(fills.iter().all(|f| f.input.inp == 4));
        // Frames are consecutive starting right after last_added_frame (4)
        assert_eq!(fills[0].frame, 5);
        assert_eq!(fills[1].frame, 6);
    }

    #[test]
    fn test_decrease_delay_mid_session_continues_sequentially() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue.set_frame_delay(3);
        for i in 0..5_i32 {
            let result = queue.add_input(PlayerInput::new(i, TestInput { inp: i as u8 }));
            assert_ne!(result, NULL_FRAME, "frame {i} should be accepted");
        }

        // Decrease delay from 3 to 1
        queue.set_frame_delay(1);

        // Some inputs will be dropped (the ones that would land before last_added_frame),
        // but eventually the queue accepts sequential inputs again
        let mut accepted = 0;
        for i in 5..10_i32 {
            let result = queue.add_input(PlayerInput::new(i, TestInput { inp: i as u8 }));
            if result != NULL_FRAME {
                accepted += 1;
            }
        }
        assert!(
            accepted > 0,
            "at least some inputs after delay decrease should be accepted"
        );
    }

    #[test]
    fn test_queue_wraps_around_without_panic() {
        let mut queue = InputQueue::<TestConfig>::new();
        // INPUT_QUEUE_LENGTH is 128. Add frames in batches, discarding confirmed frames
        // between batches to keep the queue from filling up. This exercises the circular
        // index wraparound path.
        for i in 0..200_i32 {
            let result = queue.add_input(PlayerInput::new(i, TestInput { inp: i as u8 }));
            assert_ne!(result, NULL_FRAME, "frame {i} should have been accepted");
            // discard every 64 frames so the queue never exceeds INPUT_QUEUE_LENGTH
            if i > 0 && i % 64 == 0 {
                queue.discard_confirmed_frames(i - 1);
            }
        }
    }

    #[test]
    fn exact_retry_uses_recorded_transition() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue
            .materialize_predicted(0, TestInput::default())
            .unwrap();
        let predicted = queue.slot_state(0).unwrap();
        assert_ne!(
            predicted,
            VersionedInput {
                input: TestInput::default(),
                provenance: InputProvenance::Authoritative,
                revision: 0,
            }
        );

        let authoritative = queue
            .replace_past_slot(0, predicted, TestInput::default())
            .unwrap();
        assert_eq!(authoritative, InputReplacementResult::Replaced);
        let replacement = queue.slot_state(0).unwrap();
        assert_eq!(replacement.provenance, InputProvenance::Authoritative);
        assert_eq!(replacement.revision, 1);
        assert_eq!(queue.first_incorrect_frame(), NULL_FRAME);
        let transition = queue.last_transitions[0];
        assert_eq!(
            queue.replace_past_slot(0, predicted, TestInput::default()),
            Ok(InputReplacementResult::RetryNoOp)
        );
        assert_eq!(queue.slot_state(0), Some(replacement));
        assert_eq!(queue.last_transitions[0], transition);
        assert_eq!(queue.first_incorrect_frame(), NULL_FRAME);
    }

    #[test]
    fn same_expected_with_different_replacement_is_conflict() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue
            .materialize_predicted(0, TestInput { inp: 1 })
            .unwrap();
        let old = queue.slot_state(0).unwrap();
        queue
            .replace_past_slot(0, old, TestInput { inp: 2 })
            .unwrap();
        let before_slot = queue.slot_state(0);
        let before_revision = before_slot.unwrap().revision;
        let before_transition = queue.last_transitions[0];
        let before_mismatch = queue.first_incorrect_frame();
        assert_eq!(
            queue.replace_past_slot(0, old, TestInput { inp: 3 }),
            Err(InputReplacementError::Conflict)
        );
        assert_eq!(queue.slot_state(0), before_slot);
        assert_eq!(queue.slot_state(0).unwrap().revision, before_revision);
        assert_eq!(queue.last_transitions[0], before_transition);
        assert_eq!(queue.first_incorrect_frame(), before_mismatch);
    }

    #[test]
    fn current_state_is_not_a_historical_retry() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue
            .materialize_predicted(0, TestInput { inp: 1 })
            .unwrap();
        let old = queue.slot_state(0).unwrap();
        queue
            .replace_past_slot(0, old, TestInput { inp: 2 })
            .unwrap();
        let current = queue.slot_state(0).unwrap();
        assert_eq!(
            queue.replace_past_slot(0, current, TestInput { inp: 2 }),
            Err(InputReplacementError::Conflict)
        );
        assert_eq!(queue.slot_state(0), Some(current));
    }

    #[test]
    fn same_target_with_different_expected_is_conflict() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue
            .materialize_predicted(0, TestInput { inp: 1 })
            .unwrap();
        let old = queue.slot_state(0).unwrap();
        queue
            .replace_past_slot(0, old, TestInput { inp: 2 })
            .unwrap();
        let wrong_expected = VersionedInput {
            input: TestInput { inp: 1 },
            provenance: InputProvenance::Authoritative,
            revision: old.revision,
        };
        assert_eq!(
            queue.replace_past_slot(0, wrong_expected, TestInput { inp: 2 }),
            Err(InputReplacementError::Conflict)
        );
    }

    #[test]
    fn authoritative_revision_marks_earliest_mismatch() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue
            .materialize_predicted(0, TestInput { inp: 0 })
            .unwrap();
        let old0 = queue.slot_state(0).unwrap();
        queue
            .replace_past_slot(0, old0, TestInput { inp: 1 })
            .unwrap();
        queue
            .materialize_predicted(1, TestInput { inp: 1 })
            .unwrap();
        let old1 = queue.slot_state(1).unwrap();
        queue
            .replace_past_slot(1, old1, TestInput { inp: 1 })
            .unwrap();
        assert_eq!(queue.first_incorrect_frame(), 0);
        let current1 = queue.slot_state(1).unwrap();
        queue
            .replace_past_slot(1, current1, TestInput { inp: 2 })
            .unwrap();
        assert_eq!(queue.first_incorrect_frame(), 0);

        let mut fresh = InputQueue::<TestConfig>::new();
        fresh
            .materialize_predicted(0, TestInput { inp: 1 })
            .unwrap();
        let old = fresh.slot_state(0).unwrap();
        fresh
            .replace_past_slot(0, old, TestInput { inp: 1 })
            .unwrap();
        fresh.reset_prediction();
        let current = fresh.slot_state(0).unwrap();
        fresh
            .replace_past_slot(0, current, TestInput { inp: 2 })
            .unwrap();
        assert_eq!(fresh.first_incorrect_frame(), 0);
    }

    #[test]
    fn trimming_full_queue_retains_newest_slot_and_drops_old_metadata() {
        let mut queue = InputQueue::<TestConfig>::new();
        for frame in 0..INPUT_QUEUE_LENGTH as Frame {
            queue
                .materialize_predicted(frame, TestInput { inp: frame as u8 })
                .unwrap();
        }
        queue.discard_confirmed_frames((INPUT_QUEUE_LENGTH - 1) as Frame);
        assert!(queue
            .slot_state((INPUT_QUEUE_LENGTH - 1) as Frame)
            .is_some());
        assert!(queue.slot_state(0).is_none());
        assert_eq!(queue.length, 1);
        assert_eq!(queue.tail, InputQueue::<TestConfig>::prev_pos(queue.head));
    }

    #[test]
    fn external_trim_removes_logical_slots_and_transitions() {
        let mut queue = InputQueue::<TestConfig>::new();
        for frame in 0..3 {
            queue
                .materialize_predicted(frame, TestInput { inp: frame as u8 })
                .unwrap();
        }
        let old = queue.slot_state(0).unwrap();
        queue
            .replace_past_slot(0, old, TestInput { inp: 9 })
            .unwrap();
        queue.trim_external_through(1);
        assert!(queue.slot_state(0).is_none());
        assert!(queue.slot_state(1).is_none());
        assert!(queue.slot_state(2).is_some());
        assert_eq!(queue.length, 1);
        assert_eq!(queue.last_transitions[0], None);
        assert_eq!(queue.last_transitions[1], None);
        assert_eq!(
            queue.replace_past_slot(0, old, TestInput { inp: 4 }),
            Err(InputReplacementError::OutOfRetention)
        );
    }

    #[test]
    fn full_queue_retained_slot_replaces_in_place() {
        let mut queue = InputQueue::<TestConfig>::new();
        for frame in 0..INPUT_QUEUE_LENGTH as Frame {
            queue
                .materialize_predicted(frame, TestInput { inp: frame as u8 })
                .unwrap();
        }
        let old = queue.slot_state(127).unwrap();
        let before = (queue.head, queue.tail, queue.length);
        assert_eq!(
            queue.replace_past_slot(127, old, TestInput { inp: 9 }),
            Ok(InputReplacementResult::Replaced)
        );
        assert_eq!((queue.head, queue.tail, queue.length), before);
    }

    #[test]
    fn invalid_frames_are_atomic() {
        let mut queue = InputQueue::<TestConfig>::new();
        let before_inputs = queue.inputs.clone();
        let before_slots = queue.slots.clone();
        let before_transitions = queue.last_transitions.clone();
        let before = (
            queue.head,
            queue.tail,
            queue.length,
            queue.first_incorrect_frame,
        );
        assert_eq!(queue.slot_state(NULL_FRAME), None);
        assert_eq!(queue.slot_state(-2), None);
        assert_eq!(
            queue.materialize_predicted(NULL_FRAME, TestInput::default()),
            Err(InputReplacementError::InvalidFrame)
        );
        assert_eq!(
            queue.materialize_predicted(-2, TestInput::default()),
            Err(InputReplacementError::InvalidFrame)
        );
        let expected = VersionedInput {
            input: TestInput::default(),
            provenance: InputProvenance::Predicted,
            revision: 0,
        };
        assert_eq!(
            queue.replace_past_slot(NULL_FRAME, expected, TestInput::default()),
            Err(InputReplacementError::InvalidFrame)
        );
        assert_eq!(
            queue.replace_past_slot(-2, expected, TestInput::default()),
            Err(InputReplacementError::InvalidFrame)
        );
        assert_eq!(
            (
                queue.head,
                queue.tail,
                queue.length,
                queue.first_incorrect_frame
            ),
            before
        );
        assert_eq!(queue.inputs, before_inputs);
        assert_eq!(queue.slots, before_slots);
        assert_eq!(queue.last_transitions, before_transitions);
    }

    #[test]
    fn revision_overflow_is_atomic() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue
            .materialize_predicted(0, TestInput { inp: 1 })
            .unwrap();
        queue.slots[0].revision = u64::MAX;
        let expected = queue.slot_state(0).unwrap();
        let before_inputs = queue.inputs.clone();
        let before_slots = queue.slots.clone();
        let before_transitions = queue.last_transitions.clone();
        let before = (
            queue.head,
            queue.tail,
            queue.length,
            queue.first_incorrect_frame,
        );
        assert_eq!(
            queue.replace_past_slot(0, expected, TestInput { inp: 2 }),
            Err(InputReplacementError::RevisionOverflow)
        );
        assert_eq!(queue.inputs, before_inputs);
        assert_eq!(queue.slots, before_slots);
        assert_eq!(queue.last_transitions, before_transitions);
        assert_eq!(
            (
                queue.head,
                queue.tail,
                queue.length,
                queue.first_incorrect_frame
            ),
            before
        );
    }

    #[test]
    fn predicted_and_authoritative_default_are_distinct() {
        let mut queue = InputQueue::<TestConfig>::new();
        queue
            .materialize_predicted(0, TestInput::default())
            .unwrap();
        let predicted = queue.slot_state(0).unwrap();
        queue
            .replace_past_slot(0, predicted, TestInput::default())
            .unwrap();
        let authoritative = queue.slot_state(0).unwrap();
        assert_eq!(predicted.input, authoritative.input);
        assert_ne!(predicted.provenance, authoritative.provenance);
        assert_eq!(predicted.revision + 1, authoritative.revision);
        assert_eq!(queue.first_incorrect_frame(), NULL_FRAME);
    }
}

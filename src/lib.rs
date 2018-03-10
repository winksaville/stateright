//! A library for specifying state machines and model checking invariants.
//!
//! ## Example
//!
//! ```
//! use stateright::*;
//! use std::collections::VecDeque;
//!
//! struct BinaryClock { start: u8 }
//!
//! impl StateMachine for BinaryClock {
//!     type State = u8;
//!
//!     fn init(&self, results: &mut StepVec<Self::State>) {
//!         results.push(("start", self.start));
//!     }
//!
//!     fn next(&self, state: &Self::State, results: &mut StepVec<Self::State>) {
//!         results.push(("flip bit", (1 - *state)));
//!     }
//! }
//!
//! impl Model for BinaryClock {
//!     fn invariant(&self, state: &Self::State) -> bool {
//!         0 <= *state && *state <= 1
//!     }
//! }
//!
//! let mut checker = BinaryClock { start: 1 }.checker(true);
//! assert_eq!(
//!     checker.check(100),
//!     CheckResult::Pass);
//! assert_eq!(
//!     checker.path_to(&0),
//!     Some(vec![("start", 1), ("flip bit", 0)]));
//! ```
//!
//! ## More Examples
//!
//! - [Two Phase Commit](https://github.com/stateright/stateright/blob/9a5b413b06768db92c77f7ddfd8d65e2dbb544a7/src/examples/two_phase_commit.rs)
//!
//! ## Performance
//!
//! To benchmark model checking speed, run:
//!
//! ```sh
//! cargo run --release --example bench 2pc
//! ```
//!
//! ## License
//!
//! Copyright 2018 Jonathan Nadal and made available under the MIT License.

use std::cmp::max;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;

pub mod examples;

/// Represents an action-state pair.
pub type Step<State> = (&'static str, State);

/// Represents the range of action-state pairs that a state machine can follow during a step.
pub type StepVec<State> = Vec<Step<State>>;

/// Defines how a state begins and evolves, possibly nondeterministically.
pub trait StateMachine: Sized {
    /// The type of state upon which this machine operates.
    type State: Clone + Eq + Hash;

    /// Collects the initial possible action-state pairs.
    fn init(&self, results: &mut StepVec<Self::State>);

    /// Collects the subsequent possible action-state pairs based on a previous state.
    fn next(&self, state: &Self::State, results: &mut StepVec<Self::State>);
}

/// Elaborates on a state machine by providing a state invariant.
pub trait Model: StateMachine {
    /// A claim that should always be true.
    fn invariant(&self, state: &Self::State) -> bool;

    /// Initializes a fresh checker for a particular model.
    fn checker(&self, keep_paths: bool) -> Checker<Self> {
        const STARTING_CAPACITY: usize = 30_000_000;

        let mut results = StepVec::new();
        self.init(&mut results);
        let mut pending: VecDeque<Step<Self::State>> = VecDeque::new();
        for r in results { pending.push_back(r); }

        let mut source: HashMap<Self::State, Step<Option<Self::State>>> = HashMap::new();
        if keep_paths {
            source = HashMap::with_capacity(STARTING_CAPACITY);
            for &(ref init_action, ref init_state) in pending.iter() {
                if !source.contains_key(&init_state) {
                    source.insert(init_state.clone(), (init_action, None));
                }
            }
        }

        Checker {
            keep_paths,
            model: self,

            pending,
            source,
            visited: HashSet::with_capacity(STARTING_CAPACITY),
        }
    }
}

/// Model checking can be time consuming, so the library checks up to a fixed number of states then
/// returns. This approach allows the library to avoid tying up a thread indefinitely while still
/// maintaining adequate performance. This type represents the result of one of those checking
/// passes.
#[derive(Debug, Eq, PartialEq)]
pub enum CheckResult<State> {
    /// Indicates that the checker still has pending states.
    Incomplete,
    /// Indicates that checking completed, and the invariant was not violated.
    Pass,
    /// Indicates that checking completed, and the invariant did not hold.
    Fail { state: State }
}

/// Visits every state reachable by a state machine, and verifies that an invariant holds.
pub struct Checker<'model, M: 'model + Model> {
    // immutable cfg
    keep_paths: bool,
    model: &'model M,

    // mutable checking state
    pending: VecDeque<Step<M::State>>,
    source: HashMap<M::State, Step<Option<M::State>>>,
    pub visited: HashSet<M::State>,
}

impl<'model, M: Model> Checker<'model, M> {
    /// Visits up to a specified number of states checking the model's invariant. May return
    /// earlier when all states have been visited or a state is found in which the invariant fails
    /// to hold.
    pub fn check(&mut self, max_count: usize) -> CheckResult<M::State> {
        let mut remaining = max_count;

        while let Some((_action, state)) = self.pending.pop_front() {
            // skip if already visited
            if self.visited.contains(&state) { continue; }

            // exit if invariant fails to hold
            if !self.model.invariant(&state) {
                self.visited.insert(state.clone());
                return CheckResult::Fail { state };
            }

            // otherwise collect the next steps/states
            let mut results = StepVec::new();
            self.model.next(&state, &mut results);
            if self.keep_paths {
                for (next_action, next_state) in results.clone() {
                    self.source.entry(next_state).or_insert((next_action, Some(state.clone())));
                }
            }
            for r in results { self.pending.push_back(r); }
            self.visited.insert(state);

            // but pause if we've reached the limit so that the caller can display progress
            remaining -= 1;
            if remaining == 0 { return CheckResult::Incomplete }
        }

        CheckResult::Pass
    }

    /// Identifies the action-state "behavior" path by which a visited state was reached.
    pub fn path_to(&self, state: &M::State) -> Option<Vec<Step<M::State>>> {
        let mut output = Vec::new();
        let mut next_state = state;
        while let Some(source) = self.source.get(next_state) {
            match *source {
                (next_action, None) => {
                    output.push((next_action, next_state.clone()));
                    output.reverse();
                    return Some(output);
                },
                (next_action, Some(ref prev_state)) => {
                    output.push((next_action, next_state.clone()));
                    next_state = &prev_state;
                },
            }
        }
        None // missing source indicates path not retained... or bug.
    }

    /// Blocks the thread until model checking is complete. Periodically emits a status while
    /// checking, tailoring the block size to the checking speed. Emits a report when complete.
    pub fn check_and_report(&mut self) {
        use std::time::Instant;
        let method_start = Instant::now();
        let mut block_size = 32_768;
        loop {
            let block_start = Instant::now();
            match self.check(block_size) {
                CheckResult::Fail { state } => {
                    println!("{} unique states visited after {} sec. Invariant violated{}.",
                             self.visited.len(),
                             method_start.elapsed().as_secs(),
                             self.path_to(&state)
                                 .map(|path| format!(" by path of length {}", path.len()))
                                 .unwrap_or(String::from("")));
                    return;
                },
                CheckResult::Pass => {
                    println!("{} unique states visited after {} sec. Passed.",
                             self.visited.len(),
                             method_start.elapsed().as_secs());
                    return;
                },
                CheckResult::Incomplete => {}
            }

            let block_elapsed = block_start.elapsed().as_secs();
            if block_elapsed > 0 {
                println!("{} unique states visited after {} sec. Continuing.",
                         self.visited.len(),
                         method_start.elapsed().as_secs());
            }

            if block_elapsed < 3 { block_size *= 2; }
            else if block_elapsed > 10 { block_size = max(1, block_size / 2); }
        }
    }
}

#[cfg(test)]
mod test {
    use ::*;
    use std::num::Wrapping;

    /// Given `a`, `b`, and `c`, finds `x` and `y` such that `a*x + b*y = c` where all values are
    /// in `Wrapping<u8>`.
    struct LinearEquation { a: u8, b: u8, c: u8 }
    impl StateMachine for LinearEquation {
        type State = (Wrapping<u8>, Wrapping<u8>);

        fn init(&self, results: &mut StepVec<Self::State>) {
            results.push(("guess", (Wrapping(0), Wrapping(0))));
        }

        fn next(&self, state: &Self::State, results: &mut StepVec<Self::State>) {
            match *state {
                (x, y) => {
                    results.push(("increase x", (x + Wrapping(1), y)));
                    results.push(("increase y", (x, y + Wrapping(1))));
                }
            }
        }
    }
    impl Model for LinearEquation {
        fn invariant(&self, state: &Self::State) -> bool {
            match *state {
                (x, y) => {
                    Wrapping(self.a)*x + Wrapping(self.b)*y != Wrapping(self.c)
                }
            }
        }
    }

    #[test]
    fn model_check_records_states() {
        use std::iter::FromIterator;
        let mut checker = LinearEquation { a: 2, b: 10, c: 14 }.checker(false);
        checker.check(100);
        assert_eq!(checker.visited, HashSet::from_iter(vec![
            (Wrapping(0), Wrapping(0)),
            (Wrapping(1), Wrapping(0)), (Wrapping(0), Wrapping(1)),
            (Wrapping(2), Wrapping(0)), (Wrapping(1), Wrapping(1)), (Wrapping(0), Wrapping(2)),
            (Wrapping(3), Wrapping(0)), (Wrapping(2), Wrapping(1))]));
    }

    #[test]
    fn model_check_can_pass() {
        let mut checker = LinearEquation { a: 2, b: 4, c: 7 }.checker(false);
        assert_eq!(checker.check(100), CheckResult::Incomplete);
        assert_eq!(checker.visited.len(), 100);
        assert_eq!(checker.check(100_000), CheckResult::Pass);
        assert_eq!(checker.visited.len(), 256 * 256);
    }

    #[test]
    fn model_check_can_fail() {
        let mut checker = LinearEquation { a: 2, b: 7, c: 111 }.checker(false);
        assert_eq!(checker.check(100), CheckResult::Incomplete);
        assert_eq!(checker.visited.len(), 100);
        assert_eq!(
            checker.check(100_000),
            CheckResult::Fail { state: (Wrapping(3), Wrapping(15)) });
        assert_eq!(checker.visited.len(), 187);
    }

    #[test]
    fn model_check_can_indicate_path() {
        let mut checker = LinearEquation { a: 2, b: 10, c: 14 }.checker(true);
        match checker.check(100_000) {
            CheckResult::Fail { state } => {
                assert_eq!(
                    checker.path_to(&state),
                    Some(vec![
                        ("guess",      (Wrapping(0), Wrapping(0))),
                        ("increase x", (Wrapping(1), Wrapping(0))),
                        ("increase x", (Wrapping(2), Wrapping(0))),
                        ("increase y", (Wrapping(2), Wrapping(1))),
                    ]));
            },
            _ => panic!("expected solution")
        }
    }
}


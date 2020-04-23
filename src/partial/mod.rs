//! Support for approximate results. This provides convenient API and also implementation for
//! approximate calculation.
use thiserror::Error;

mod approximate_action_listener;
pub(self) mod approximate_evaluator;
pub(self) mod bounded_double;
mod count_evaluator;
mod partial_result;

pub(crate) use approximate_action_listener::ApproximateActionListener;
pub(crate) use approximate_evaluator::ApproximateEvaluator;
pub(crate) use count_evaluator::CountEvaluator;
pub(crate) use partial_result::PartialResult;

#[derive(Debug, Error)]
pub enum PartialJobError {
    #[error("set_final_value called twice on a PartialResult")]
    SetFinalValTwice,

    #[error("unreachable")]
    None,
}
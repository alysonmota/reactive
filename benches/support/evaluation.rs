//! Host-only experiment selection. The runtime itself never reads environment
//! variables in normal builds and keeps FullFrames as its default.
pub fn runtime() -> typed_runtime_prototype::Runtime {
    use typed_runtime_prototype::{EvaluationStrategy, Runtime};
    let strategy = match std::env::var("TYPED_EVALUATION")
        .as_deref()
        .unwrap_or("full")
    {
        "full" => EvaluationStrategy::FullFrames,
        "topological" => EvaluationStrategy::DemandDriven,
        other => panic!("unknown TYPED_EVALUATION: {other}"),
    };
    Runtime::with_evaluation_strategy(strategy)
}

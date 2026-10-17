//! Host-only experiment selection. The runtime itself never reads environment
//! variables in normal builds and keeps FullFrames as its default.
pub fn runtime() -> typed_runtime_prototype::Runtime {
    use typed_runtime_prototype::{EvaluationStrategy, Runtime};
    let strategy = match std::env::var("TYPED_EVALUATION")
        .as_deref()
        .unwrap_or("full")
    {
        "full" => EvaluationStrategy::FullFrames,
        "light" => EvaluationStrategy::LightFrames,
        "topological" => EvaluationStrategy::DemandDriven,
        "shared" => EvaluationStrategy::SharedPlan,
        "shared-dirty" => EvaluationStrategy::SharedPlanDirty,
        "shared-opt" => EvaluationStrategy::SharedPlanOptimized,
        "shared-dirty-opt" => EvaluationStrategy::SharedPlanDirtyOptimized,
        "global" => EvaluationStrategy::GlobalPlan,
        "global-dirty" => EvaluationStrategy::GlobalPlanDirty,
        "local" => EvaluationStrategy::LocalPlan,
        "local-dirty" => EvaluationStrategy::LocalPlanDirty,
        "ordered" => EvaluationStrategy::OrderedDemand,
        other => panic!("unknown TYPED_EVALUATION: {other}"),
    };
    Runtime::with_evaluation_strategy(strategy)
}

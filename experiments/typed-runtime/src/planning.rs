/// The checked frame control remains the default; fusion is experimental.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EvaluationStrategy {
    #[default]
    FullFrames,
    /// Advance the callback-free walk and resume/pop in one graph kernel.
    DemandDriven,
}

//! Every distinct kind of profiling data collection is an [`Instrument`] implementation that
//! lives in a submodule.
//!
//! Each instrument is uniquely identified by its [`Instrument::name`].

use miden_core::operations::Operation;

pub mod op_histogram;

pub use op_histogram::OpHistogram;

/// The functionality required for an instrument to be plugged in to `Profiler`.
pub trait Instrument {
    /// The name used to uniquely identify this instrumentation.
    fn name(&self) -> &'static str;
    /// To be called each vm cycle an `Operation` is executed.
    fn on_operation_execution_cycle(&mut self, op: Operation);
    fn write_report_to(&self, writer: &mut dyn std::io::Write) -> std::io::Result<()>;
}

//! A self-contained snapshot of a recorded execution, sufficient to replay it in the debugger
//! without the original host.
//!
//! A live execution (e.g. a transaction driven through the DAP executor) resolves two kinds of
//! host interaction that a bare debugger host cannot reproduce on its own: the advice mutations
//! returned by event handlers, and the MAST forests resolved for `call`/`dyncall` targets (account
//! code, note scripts, etc.). A [ReplaySnapshot] captures both, alongside the program and its
//! inputs, so the same execution can be re-run later by feeding the recorded event log into an
//! event-replay debugger host (see [`Executor::into_debug_with_replay`](crate::exec::Executor) and
//! `State::new_for_transaction`).

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use miden_core::{
    mast::MastForest,
    program::StackInputs,
    serde::{ByteReader, ByteWriter, Deserializable, DeserializationError, Serializable},
};
use miden_mast_package::{Package, debug_info::PackageDebugInfo};
use miden_processor::{
    ExecutionOptions, LoadedMastForest,
    advice::{AdviceInputs, AdviceMutation},
};

use super::advice::{read_event_log, write_event_log};

/// A shared, cloneable log of the MAST forests a host resolved during execution.
///
/// The debugger's event-replay host serves recorded advice mutations for `on_event`, but it still
/// has to resolve the code for `call`/`dyncall` targets itself. Recording the forests the live
/// host returned — deduplicated, since the same forest is resolved for many nodes — lets the
/// replay host load exactly that set and reach the same targets.
#[derive(Clone, Default)]
pub struct MastForestRecorder {
    forests: Arc<Mutex<Vec<LoadedMastForest>>>,
}

impl MastForestRecorder {
    /// Create a new, empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a copy of the recorded forests.
    pub fn snapshot(&self) -> Vec<LoadedMastForest> {
        self.forests.lock().expect("mast forest log poisoned").clone()
    }

    /// Record a forest resolved by the host, ignoring forests already recorded this run.
    pub(crate) fn record(&self, forest: LoadedMastForest) {
        let mut guard = self.forests.lock().expect("mast forest log poisoned");
        if !guard
            .iter()
            .any(|existing| Arc::ptr_eq(existing.mast_forest(), forest.mast_forest()))
        {
            guard.push(forest);
        }
    }

    /// Discard everything recorded so far, e.g. when execution restarts from the beginning.
    pub(crate) fn clear(&self) {
        self.forests.lock().expect("mast forest log poisoned").clear();
    }
}

/// Successful replay snapshot write metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplaySnapshotWrite {
    pub path: PathBuf,
    pub event_count: usize,
    pub forest_count: usize,
}

/// Error metadata for a failed replay snapshot write.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("failed to write replay snapshot to {}: {}", path.display(), message)]
pub struct ReplaySnapshotWriteError {
    pub path: PathBuf,
    pub message: String,
}

/// Shared status handle for a configured replay snapshot write.
#[derive(Clone, Debug, Default)]
pub struct ReplaySnapshotRecorder {
    status: Arc<Mutex<Option<Result<ReplaySnapshotWrite, ReplaySnapshotWriteError>>>>,
}

impl ReplaySnapshotRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the last snapshot write status, leaving the handle empty.
    pub fn take(&self) -> Option<Result<ReplaySnapshotWrite, ReplaySnapshotWriteError>> {
        self.status.lock().expect("replay snapshot status poisoned").take()
    }

    pub(crate) fn record_success(&self, write: ReplaySnapshotWrite) {
        *self.status.lock().expect("replay snapshot status poisoned") = Some(Ok(write));
    }

    pub(crate) fn record_error(&self, path: PathBuf, err: impl ToString) {
        *self.status.lock().expect("replay snapshot status poisoned") =
            Some(Err(ReplaySnapshotWriteError {
                path,
                message: err.to_string(),
            }));
    }
}

/// Magic bytes identifying a replay snapshot file, followed by a format version. Bumping the
/// version invalidates older snapshots, whose serialized shape may differ.
const SNAPSHOT_MAGIC: [u8; 6] = *b"MDNSNP";
const SNAPSHOT_VERSION: u8 = 2;

/// Everything needed to replay a recorded execution in the debugger.
pub struct ReplaySnapshot {
    /// The program that was executed (for a transaction, the transaction kernel).
    pub package: Arc<Package>,
    /// The operand stack inputs the program started with.
    pub stack_inputs: StackInputs,
    /// The advice inputs the program started with.
    pub advice_inputs: AdviceInputs,
    /// The VM execution options used by the recorded run.
    pub options: ExecutionOptions,
    /// The MAST forests resolved by the host during execution (account code, note scripts, ...),
    /// which the replay host must be able to resolve for the same `call`/`dyncall` targets.
    pub mast_forests: Vec<LoadedMastForest>,
    /// The advice mutations produced by event handlers, one entry per `on_event` invocation, in
    /// execution order — the event replay queue.
    pub event_log: Vec<Vec<AdviceMutation>>,
}

impl ReplaySnapshot {
    /// Serialize the snapshot to `path`.
    pub fn write_to_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        std::fs::write(path, self.to_bytes())
    }

    /// Read and deserialize a snapshot from `path`.
    pub fn read_from_file(path: impl AsRef<Path>) -> Result<Self, ReplaySnapshotError> {
        let bytes = std::fs::read(path).map_err(ReplaySnapshotError::Io)?;
        Self::read_from_bytes(&bytes).map_err(ReplaySnapshotError::Deserialization)
    }

    /// Serialize the snapshot to a byte vector.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.write_into(&mut bytes);
        bytes
    }

    /// Deserialize a snapshot from bytes.
    pub fn read_from_bytes(bytes: &[u8]) -> Result<Self, DeserializationError> {
        let mut reader = miden_core::serde::SliceReader::new(bytes);
        Self::read_from(&mut reader)
    }
}

impl Serializable for ReplaySnapshot {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write_bytes(&SNAPSHOT_MAGIC);
        target.write_u8(SNAPSHOT_VERSION);
        self.package.write_into(target);
        self.stack_inputs.write_into(target);
        self.advice_inputs.write_into(target);
        write_execution_options(&self.options, target);
        target.write_usize(self.mast_forests.len());
        for forest in &self.mast_forests {
            forest.mast_forest().as_ref().write_into(target);
            match forest.package_debug_info().ok().flatten() {
                Some(debug_info) => {
                    target.write_bool(true);
                    debug_info.as_ref().write_into(target);
                }
                None => {
                    target.write_bool(false);
                }
            }
        }
        write_event_log(&self.event_log, target);
    }
}

impl Deserializable for ReplaySnapshot {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let magic: [u8; 6] = source.read_array()?;
        if magic != SNAPSHOT_MAGIC {
            return Err(DeserializationError::InvalidValue(
                "not a Miden debugger replay snapshot (bad magic)".to_string(),
            ));
        }
        let version = source.read_u8()?;
        if version != SNAPSHOT_VERSION {
            return Err(DeserializationError::InvalidValue(format!(
                "unsupported replay snapshot version {version} (expected {SNAPSHOT_VERSION})"
            )));
        }
        let package = Arc::new(Package::read_from_unchecked(source)?);
        let stack_inputs = StackInputs::read_from(source)?;
        let advice_inputs = AdviceInputs::read_from(source)?;
        let options = read_execution_options(source)?;
        let forest_count = source.read_usize()?;
        let mut mast_forests = Vec::with_capacity(forest_count);
        for _ in 0..forest_count {
            let mast_forest = Arc::new(MastForest::read_from(source)?);
            mast_forests.push(if source.read_bool()? {
                let debug_info = Some(PackageDebugInfo::read_from(source)?);
                LoadedMastForest::with_package_debug_info(mast_forest, Ok(debug_info))
            } else {
                LoadedMastForest::new(mast_forest)
            });
        }
        let event_log = read_event_log(source)?;
        Ok(Self {
            package,
            stack_inputs,
            advice_inputs,
            options,
            mast_forests,
            event_log,
        })
    }
}

fn write_execution_options<W: ByteWriter>(options: &ExecutionOptions, target: &mut W) {
    target.write_u32(options.max_cycles());
    target.write_u32(options.expected_cycles());
    target.write_usize(options.core_trace_fragment_size());
    target.write_usize(options.max_adv_map_value_size());
    target.write_usize(options.max_adv_map_elements());
    target.write_usize(options.max_hash_len_bytes());
    target.write_usize(options.max_deferred_elements());
    target.write_bool(options.overlapped_trace_build());
    target.write_usize(options.max_num_continuations());
    target.write_usize(options.max_merkle_store_nodes());
    target.write_usize(options.max_stack_depth());
    target.write_usize(options.max_memory_elements());
}

fn read_execution_options<R: ByteReader>(
    source: &mut R,
) -> Result<ExecutionOptions, DeserializationError> {
    let max_cycles = source.read_u32()?;
    let expected_cycles = source.read_u32()?;
    let core_trace_fragment_size = source.read_usize()?;
    let max_adv_map_value_size = source.read_usize()?;
    let max_adv_map_elements = source.read_usize()?;
    let max_hash_len_bytes = source.read_usize()?;
    let max_deferred_elements = source.read_usize()?;
    let overlapped_trace_build = source.read_bool()?;
    let max_num_continuations = source.read_usize()?;
    let max_merkle_store_nodes = source.read_usize()?;
    let max_stack_depth = source.read_usize()?;
    let max_memory_elements = source.read_usize()?;

    ExecutionOptions::new(Some(max_cycles), expected_cycles, core_trace_fragment_size)
        .map_err(|err| {
            DeserializationError::InvalidValue(format!("invalid execution options: {err}"))
        })
        .and_then(|options| {
            options
                .with_max_adv_map_value_size(max_adv_map_value_size)
                .with_max_adv_map_elements(max_adv_map_elements)
                .with_max_hash_len_bytes(max_hash_len_bytes)
                .with_max_deferred_elements(max_deferred_elements)
                .with_overlapped_trace_build(overlapped_trace_build)
                .with_max_num_continuations(max_num_continuations)
                .with_max_merkle_store_nodes(max_merkle_store_nodes)
                .with_max_memory_elements(max_memory_elements)
                .with_max_stack_depth(max_stack_depth)
                .map_err(|err| {
                    DeserializationError::InvalidValue(format!("invalid execution options: {err}"))
                })
        })
}

/// Error reading a [ReplaySnapshot] from a file.
#[derive(Debug, thiserror::Error)]
pub enum ReplaySnapshotError {
    #[error("failed to read replay snapshot file: {0}")]
    Io(std::io::Error),
    #[error("failed to deserialize replay snapshot: {0}")]
    Deserialization(DeserializationError),
}

#[cfg(test)]
mod tests {
    use miden_assembly::{Assembler, DefaultSourceManager};
    use miden_core::{Felt, Word, crypto::merkle::InnerNodeInfo};

    use super::*;

    fn word(values: [u32; 4]) -> Word {
        Word::from(values.map(Felt::from))
    }

    /// A snapshot round-trips through bytes: program, inputs, forests, and the event log — across
    /// the AdviceMutation variant shapes that carry simple payloads — survive intact.
    #[test]
    fn replay_snapshot_round_trips() {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let program = Assembler::new(source_manager)
            .assemble_program("program", "begin push.1 push.2 add drop end")
            .map(Arc::<Package>::from)
            .expect("failed to assemble test program");
        let forest = LoadedMastForest::with_package_debug_info(
            program.mast_forest().clone(),
            program.debug_info(),
        );

        let event_log = vec![
            vec![AdviceMutation::extend_advice_stack(
                [Felt::from(7u32), Felt::from(8u32)].into_iter().collect(),
            )],
            vec![],
            vec![AdviceMutation::extend_merkle_store([InnerNodeInfo {
                value: word([1, 2, 3, 4]),
                left: word([5, 6, 7, 8]),
                right: word([9, 10, 11, 12]),
            }])],
        ];

        let snapshot = ReplaySnapshot {
            package: program.clone(),
            stack_inputs: StackInputs::new(&[Felt::from(42u32), Felt::from(43u32)]).unwrap(),
            advice_inputs: AdviceInputs::default()
                .with_advice_stack([Felt::from(99u32)].into_iter().collect()),
            options: ExecutionOptions::new(Some(100_000), 32, 1024)
                .unwrap()
                .with_max_adv_map_value_size(64)
                .with_max_adv_map_elements(256)
                .with_max_hash_len_bytes(512)
                .with_max_deferred_elements(768)
                .with_overlapped_trace_build(false)
                .with_max_num_continuations(128)
                .with_max_merkle_store_nodes(384)
                .with_max_memory_elements(1024)
                .with_max_stack_depth(128)
                .unwrap(),
            mast_forests: vec![forest],
            event_log,
        };

        let restored = ReplaySnapshot::read_from_bytes(&snapshot.to_bytes())
            .expect("snapshot failed to deserialize");

        assert_eq!(restored.package.digest(), snapshot.package.digest());
        assert_eq!(restored.stack_inputs, snapshot.stack_inputs);
        assert_eq!(restored.advice_inputs, snapshot.advice_inputs);
        assert_eq!(restored.options, snapshot.options);
        assert_eq!(restored.mast_forests.len(), 1);
        assert_eq!(restored.event_log.len(), 3);
        assert_eq!(restored.event_log[1].len(), 0, "empty event batch must survive");
        match restored.event_log[0].as_slice() {
            [AdviceMutation::ExtendStack { stack }] => {
                assert_eq!(
                    stack.iter().copied().collect::<Vec<_>>(),
                    [Felt::from(7u32), Felt::from(8u32)],
                );
            }
            _ => panic!("unexpected first event batch"),
        }
        match restored.event_log[2].as_slice() {
            [AdviceMutation::ExtendMerkleStore { infos }] => {
                assert_eq!(infos.len(), 1);
                assert_eq!(infos[0].value, word([1, 2, 3, 4]));
                assert_eq!(infos[0].right, word([9, 10, 11, 12]));
            }
            _ => panic!("unexpected merkle-store event batch"),
        }
    }

    /// A file that does not start with the snapshot magic is rejected.
    #[test]
    fn replay_snapshot_rejects_bad_magic() {
        let err = ReplaySnapshot::read_from_bytes(b"not a snapshot at all really");
        assert!(err.is_err(), "expected deserialization to fail on bad magic");
    }

    /// Version 1 snapshots may contain legacy precompile mutations and opcode semantics, so they
    /// must not be interpreted as version 2 snapshots.
    #[test]
    fn replay_snapshot_rejects_previous_version() {
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        bytes.push(SNAPSHOT_VERSION - 1);

        let Err(err) = ReplaySnapshot::read_from_bytes(&bytes) else {
            panic!("expected deserialization to reject a version 1 snapshot");
        };
        assert!(err.to_string().contains("unsupported replay snapshot version 1"));
    }
}

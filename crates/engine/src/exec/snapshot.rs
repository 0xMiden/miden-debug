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
    path::Path,
    sync::{Arc, Mutex},
};

use miden_core::{
    mast::MastForest,
    program::{Program, StackInputs},
    serde::{ByteReader, ByteWriter, Deserializable, DeserializationError, Serializable},
};
use miden_processor::advice::{AdviceInputs, AdviceMutation};

use super::advice::{read_event_log, write_event_log};

/// A shared, cloneable log of the MAST forests a host resolved during execution.
///
/// The debugger's event-replay host serves recorded advice mutations for `on_event`, but it still
/// has to resolve the code for `call`/`dyncall` targets itself. Recording the forests the live
/// host returned — deduplicated, since the same forest is resolved for many nodes — lets the
/// replay host load exactly that set and reach the same targets.
#[derive(Clone, Default)]
pub struct MastForestRecorder {
    forests: Arc<Mutex<Vec<Arc<MastForest>>>>,
}

impl MastForestRecorder {
    /// Create a new, empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a copy of the recorded forests.
    pub fn snapshot(&self) -> Vec<Arc<MastForest>> {
        self.forests.lock().expect("mast forest log poisoned").clone()
    }

    /// Record a forest resolved by the host, ignoring forests already recorded this run.
    pub(crate) fn record(&self, forest: Arc<MastForest>) {
        let mut guard = self.forests.lock().expect("mast forest log poisoned");
        if !guard.iter().any(|existing| Arc::ptr_eq(existing, &forest)) {
            guard.push(forest);
        }
    }

    /// Discard everything recorded so far, e.g. when execution restarts from the beginning.
    pub(crate) fn clear(&self) {
        self.forests.lock().expect("mast forest log poisoned").clear();
    }
}

/// Magic bytes identifying a replay snapshot file, followed by a format version. Bumping the
/// version invalidates older snapshots, whose serialized shape may differ.
const SNAPSHOT_MAGIC: [u8; 6] = *b"MDNSNP";
const SNAPSHOT_VERSION: u8 = 1;

/// Everything needed to replay a recorded execution in the debugger.
pub struct ReplaySnapshot {
    /// The program that was executed (for a transaction, the transaction kernel).
    pub program: Program,
    /// The operand stack inputs the program started with.
    pub stack_inputs: StackInputs,
    /// The advice inputs the program started with.
    pub advice_inputs: AdviceInputs,
    /// The MAST forests resolved by the host during execution (account code, note scripts, ...),
    /// which the replay host must be able to resolve for the same `call`/`dyncall` targets.
    pub mast_forests: Vec<Arc<MastForest>>,
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
        self.program.write_into(target);
        self.stack_inputs.write_into(target);
        self.advice_inputs.write_into(target);
        target.write_usize(self.mast_forests.len());
        for forest in &self.mast_forests {
            forest.as_ref().write_into(target);
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
        let program = Program::read_from(source)?;
        let stack_inputs = StackInputs::read_from(source)?;
        let advice_inputs = AdviceInputs::read_from(source)?;
        let forest_count = source.read_usize()?;
        let mut mast_forests = Vec::with_capacity(forest_count);
        for _ in 0..forest_count {
            mast_forests.push(Arc::new(MastForest::read_from(source)?));
        }
        let event_log = read_event_log(source)?;
        Ok(Self {
            program,
            stack_inputs,
            advice_inputs,
            mast_forests,
            event_log,
        })
    }
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
            .assemble_program("begin push.1 push.2 add drop end")
            .expect("failed to assemble test program");
        let forest = program.mast_forest().clone();

        let event_log = vec![
            vec![AdviceMutation::extend_stack([Felt::from(7u32), Felt::from(8u32)])],
            vec![],
            vec![AdviceMutation::extend_merkle_store([InnerNodeInfo {
                value: word([1, 2, 3, 4]),
                left: word([5, 6, 7, 8]),
                right: word([9, 10, 11, 12]),
            }])],
        ];

        let snapshot = ReplaySnapshot {
            program: program.clone(),
            stack_inputs: StackInputs::new(&[Felt::from(42u32), Felt::from(43u32)]).unwrap(),
            advice_inputs: AdviceInputs::default().with_stack([Felt::from(99u32)]),
            mast_forests: vec![forest],
            event_log,
        };

        let restored = ReplaySnapshot::read_from_bytes(&snapshot.to_bytes())
            .expect("snapshot failed to deserialize");

        assert_eq!(restored.program.hash(), snapshot.program.hash());
        assert_eq!(restored.stack_inputs, snapshot.stack_inputs);
        assert_eq!(restored.advice_inputs, snapshot.advice_inputs);
        assert_eq!(restored.mast_forests.len(), 1);
        assert_eq!(restored.event_log.len(), 3);
        assert_eq!(restored.event_log[1].len(), 0, "empty event batch must survive");
        match restored.event_log[0].as_slice() {
            [AdviceMutation::ExtendStack { values }] => {
                assert_eq!(values.as_slice(), &[Felt::from(7u32), Felt::from(8u32)]);
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
}

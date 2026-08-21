use std::{process::Command, sync::Arc};

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_debug::{
    Executor, ReplaySnapshot,
    flamegraph::FlamegraphProfile,
    processor::{ExecutionOptions, StackInputs, advice::AdviceInputs},
};

#[test]
fn flamegraph_profile_can_be_collected_from_debug_executor() {
    let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
    let source = r#"
begin
    push.1
    push.2
    add
    push.10
    mul
    swap
    drop
end
"#;

    let program = Assembler::new(source_manager.clone())
        .assemble_program("program", source)
        .unwrap();
    let mut debug_executor = Executor::new(vec![]).into_debug(program.into(), source_manager);

    let profile =
        FlamegraphProfile::collect(&mut debug_executor).expect("program execution failed");

    assert!(debug_executor.stopped);
    assert!(profile.total_cycles() > 0);
    assert!(!profile.samples().is_empty());
    assert_eq!(profile.samples().values().sum::<usize>(), profile.total_cycles());
}

#[test]
fn flamegraph_profile_can_be_collected_from_replay_snapshot() {
    let profile =
        FlamegraphProfile::collect_replay(replay_snapshot()).expect("replay execution failed");

    assert!(profile.total_cycles() > 0);
    assert_eq!(profile.samples().values().sum::<usize>(), profile.total_cycles());
}

#[test]
fn flamegraph_cli_accepts_replay_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let snapshot_path = temp.path().join("program.mdsnap");
    let flamegraph_path = temp.path().join("program.svg");
    replay_snapshot().write_to_file(&snapshot_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_miden-debug"))
        .args([
            "flamegraph",
            "--replay",
            snapshot_path.to_str().unwrap(),
            "--output",
            flamegraph_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(flamegraph_path.is_file());
}

fn replay_snapshot() -> ReplaySnapshot {
    let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
    let package = Assembler::new(source_manager)
        .assemble_program("program", "begin push.1 push.2 add swap drop end")
        .map(Arc::from)
        .unwrap();
    ReplaySnapshot {
        package,
        stack_inputs: StackInputs::default(),
        advice_inputs: AdviceInputs::default(),
        options: ExecutionOptions::default(),
        mast_forests: Vec::new(),
        event_log: Vec::new(),
    }
}

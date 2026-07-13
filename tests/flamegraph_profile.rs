use std::sync::Arc;

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_debug::{Executor, flamegraph::FlamegraphProfile};

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

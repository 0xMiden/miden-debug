use miden_assembly_syntax::ast::types::{CallConv, FunctionType, Type};
use miden_core::Felt;

use super::*;

#[test]
fn typed_arguments_use_the_entrypoint_abi() {
    let config = DebuggerConfig {
        args: vec!["4294967303".into(), "true".into()],
        ..Default::default()
    };
    let procedure = TypedProcedure::new(
        "entrypoint",
        FunctionType::new(CallConv::ComponentModel, [Type::U64, Type::I1], []),
    )
    .unwrap();

    let inputs = execution_inputs(&config, Some(&procedure)).unwrap();

    assert_eq!(
        &inputs.inputs.as_ref()[..3],
        [Felt::from_u32(7), Felt::from_u32(1), Felt::from_u32(1)]
    );
}

#[test]
fn untyped_arguments_keep_sequential_push_order() {
    let config = DebuggerConfig {
        args: vec!["3".into(), "4".into()],
        ..Default::default()
    };

    let inputs = execution_inputs(&config, None).unwrap();

    assert_eq!(&inputs.inputs.as_ref()[..2], [Felt::from_u32(4), Felt::from_u32(3)]);
}

pub(crate) fn test_package_input() -> crate::InputFile {
    use miden_assembly::{Assembler, DefaultSourceManager};
    use miden_core::serde::Serializable;

    let package = Assembler::new(Arc::new(DefaultSourceManager::default()))
        .assemble_program("test", "begin push.3 push.4 add add end")
        .expect("test package should assemble");
    crate::InputFile::new("stdin://", Some(package.to_bytes().into_boxed_slice()))
}

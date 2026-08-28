//! Compile a .masm file into a .masp package that miden-debug can load.
//!
//! Usage:
//!   cargo run --example compile-masm -- examples/simple.masm -o examples/simple.masp
//!
//! If `-o` is omitted, this produces `examples/simple.masp`, which you can then debug:
//!   cargo run -- examples/simple.masp

use std::{env, path::PathBuf, sync::Arc};

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_assembly_syntax::Parse;
use miden_mast_package::Package;

const USAGE: &str =
    "Usage: compile-masm <file.masm> [-o <file.masp>] [--inject-frame-base-test-vars]";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("{USAGE}");
        std::process::exit(1);
    }

    let input_path = PathBuf::from(&args[1]);
    let mut output_path = None;
    let mut inject_frame_base_test_vars = false;
    let mut options = args[2..].iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "-o" | "--output" => {
                output_path = Some(PathBuf::from(options.next().ok_or("missing output path")?));
            }
            "--inject-frame-base-test-vars" => inject_frame_base_test_vars = true,
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(1);
            }
        }
    }
    let output_path = output_path.unwrap_or_else(|| input_path.with_extension(Package::EXTENSION));
    let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());

    // Read and assemble the MASM source as a program
    let assembler = Assembler::new(source_manager.clone());
    let package = if inject_frame_base_test_vars {
        let mut module = input_path.as_path().parse(false, source_manager)?;
        inject_frame_base_test_vars_into(&mut module)?;
        assembler.assemble_program("program", module)?
    } else {
        assembler.assemble_program("program", input_path.as_path())?
    };

    // Write the .masp file
    package.write_to_file(&output_path)?;

    println!("Compiled {} -> {}", input_path.display(), output_path.display());
    println!("\nTo debug:");
    println!("  cargo run -- {}", output_path.display());

    Ok(())
}

fn inject_frame_base_test_vars_into(
    module: &mut miden_assembly::ast::Module,
) -> Result<(), Box<dyn std::error::Error>> {
    use miden_assembly_syntax::{
        ast::{DebugFrameBase, DebugVarInfo, DebugVarLocation, Instruction, Op},
        debuginfo::{SourceSpan, Span},
    };

    let procedure = module
        .procedures_mut()
        .find(|procedure| !procedure.is_entrypoint() && procedure.num_locals() == 1)
        .ok_or("frame-base test fixture requires a non-entrypoint procedure with one local")?;

    for (name, base) in [
        ("local_frame", DebugFrameBase::Local(-4)),
        ("memory_frame", DebugFrameBase::Memory(100)),
    ] {
        let location = DebugVarLocation::ResolvedFrameBase {
            base,
            byte_offset: 4,
        };
        let debug_var = DebugVarInfo::new(name, location);
        procedure
            .body_mut()
            .push(Op::Inst(Span::new(SourceSpan::default(), Instruction::DebugVar(debug_var))));
    }
    procedure
        .body_mut()
        .push(Op::Inst(Span::new(SourceSpan::default(), Instruction::LocLoad(0u16.into()))));
    procedure
        .body_mut()
        .push(Op::Inst(Span::new(SourceSpan::default(), Instruction::Drop)));

    Ok(())
}

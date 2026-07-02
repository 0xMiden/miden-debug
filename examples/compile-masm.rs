//! Compile a .masm file into a .masp package that miden-debug can load.
//!
//! Usage:
//!   cargo run --example compile-masm -- examples/simple.masm -o examples/simple.masp
//!
//! If `-o` is omitted, this produces `examples/simple.masp`, which you can then debug:
//!   cargo run -- examples/simple.masp

use std::{collections::BTreeMap, env, path::PathBuf, sync::Arc};

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_assembly_syntax::{
    Library, Path as MasmPath, ast,
    library::{LibraryExport, ProcedureExport as LibraryProcedureExport},
};
use miden_core::serde::Serializable;
use miden_mast_package::{Package, TargetType, Version};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: compile-masm <file.masm> [-o <file.masp>]");
        std::process::exit(1);
    }

    let input_path = PathBuf::from(&args[1]);
    let output_path = match args.as_slice() {
        [_, _] => input_path.with_extension("masp"),
        [_, _, flag, output] if flag == "-o" || flag == "--output" => PathBuf::from(output),
        _ => {
            eprintln!("Usage: compile-masm <file.masm> [-o <file.masp>]");
            std::process::exit(1);
        }
    };
    let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());

    let program = Assembler::new(source_manager).assemble_program(input_path.as_path())?;
    let exec_path: Arc<MasmPath> =
        MasmPath::exec_path().join(ast::ProcedureName::MAIN_PROC_NAME).into();
    let library = Arc::new(Library::new(
        program.mast_forest().clone(),
        BTreeMap::from_iter([(
            exec_path.clone(),
            LibraryExport::Procedure(LibraryProcedureExport::new(program.entrypoint(), exec_path)),
        )]),
    )?);
    let package = Package::from_library(
        input_path.file_stem().and_then(|s| s.to_str()).unwrap_or("program").into(),
        Version::new(0, 0, 0),
        TargetType::Executable,
        library,
        [],
    );

    std::fs::write(&output_path, package.to_bytes())?;

    println!("Compiled {} -> {}", input_path.display(), output_path.display());
    println!("\nTo debug:");
    println!("  cargo run -- {}", output_path.display());

    Ok(())
}

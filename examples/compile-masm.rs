//! Compile a .masm file into a .masp package that miden-debug can load.
//!
//! Usage:
//!   cargo run --example compile-masm -- examples/simple.masm -o examples/simple.masp
//!
//! If `-o` is omitted, this produces `examples/simple.masp`, which you can then debug:
//!   cargo run -- examples/simple.masp

use std::{env, path::PathBuf, sync::Arc};

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_mast_package::Package;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: compile-masm <file.masm> [-o <file.masp>]");
        std::process::exit(1);
    }

    let input_path = PathBuf::from(&args[1]);
    let output_path = match args.as_slice() {
        [_, _] => input_path.with_extension(Package::EXTENSION),
        [_, _, flag, output] if flag == "-o" || flag == "--output" => PathBuf::from(output),
        _ => {
            eprintln!("Usage: compile-masm <file.masm> [-o <file.masp>]");
            std::process::exit(1);
        }
    };
    let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());

    // Read and assemble the MASM source as a program
    let assembler = Assembler::new(source_manager.clone());
    let package = assembler.assemble_program("program", input_path.as_path())?;

    // Write the .masp file
    package.write_to_file(&output_path)?;

    println!("Compiled {} -> {}", input_path.display(), output_path.display());
    println!("\nTo debug:");
    println!("  cargo run -- {}", output_path.display());

    Ok(())
}

//! Compile a .masm file into a .masp package that miden-debug can load.
//!
//! Usage:
//!   cargo run --example compile-masm -- examples/simple.masm
//!
//! This produces `examples/simple.masp` which you can then debug:
//!   cargo run -- examples/simple.masp

use std::{env, path::PathBuf, sync::Arc};

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_core::serde::Serializable;
use miden_mast_package::{Package, PackageManifest, TargetType, Version};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: compile-masm <file.masm>");
        std::process::exit(1);
    }

    let input_path = PathBuf::from(&args[1]);
    let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());

    // Read and assemble the MASM source as a library
    let source = std::fs::read_to_string(&input_path)?;
    let assembler = Assembler::new(source_manager.clone());
    let library = assembler.assemble_library([source.as_str()])?;

    // Build a package from the library
    let package = Package {
        name: input_path.file_stem().and_then(|s| s.to_str()).unwrap_or("program").into(),
        version: Version::new(0, 0, 0),
        description: None,
        kind: TargetType::Executable,
        mast: library,
        manifest: PackageManifest::new(core::iter::empty())?,
        sections: vec![],
    };

    // Write the .masp file
    let output_path = input_path.with_extension("masp");
    let bytes = package.to_bytes();
    std::fs::write(&output_path, bytes)?;

    println!("Compiled {} -> {}", input_path.display(), output_path.display());
    println!("\nTo debug:");
    println!("  cargo run -- {}", output_path.display());

    Ok(())
}

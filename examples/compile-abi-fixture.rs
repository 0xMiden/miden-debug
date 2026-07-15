//! Compile a MASM fixture and attach ABI type/debug-variable metadata for lit tests.

use std::{env, io, path::PathBuf, sync::Arc};

use miden_assembly::{Assembler, DefaultSourceManager, SourceManager};
use miden_assembly_syntax::ast::{
    DebugVarLocation,
    types::{ArrayType, EnumType, StructType, Type, Variant},
};
use miden_core::serde::Serializable;
use miden_mast_package::{
    Package, Section, SectionId,
    debug_info::{
        DebugSourceNodeId, DebugSourceVar, DebugStringIdx, DebugTypeIdx, PackageDebugInfoBuilder,
    },
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (input_path, output_path) = parse_args()?;
    let source_manager: Arc<dyn SourceManager> = Arc::new(DefaultSourceManager::default());
    let mut package =
        Assembler::new(source_manager).assemble_program("abi-types", input_path.as_path())?;

    add_abi_debug_info(&mut package)?;
    package.write_to_file(&output_path)?;

    Ok(())
}

fn parse_args() -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    match args.as_slice() {
        [_, input, flag, output] if flag == "-o" || flag == "--output" => {
            Ok((PathBuf::from(input), PathBuf::from(output)))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: compile-abi-fixture <file.masm> -o <file.masp>",
        )
        .into()),
    }
}

fn add_abi_debug_info(package: &mut Package) -> Result<(), Box<dyn std::error::Error>> {
    let debug_info = package
        .debug_info()
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("assembled fixture has no debug information"))?;
    let mut builder = PackageDebugInfoBuilder::from(Box::new(debug_info));

    let (marker_node, marker_op) = find_nop_marker(&builder)
        .ok_or_else(|| io::Error::other("ABI fixture is missing its nop declaration marker"))?;

    let variables = abi_metadata(&mut builder, marker_op)?;
    builder[marker_node].debug_vars.extend(variables);
    builder[marker_node].debug_vars.sort_by_key(|row| row.op_idx);

    let debug_info = builder.build();
    package.sections.retain(|section| section.id != SectionId::DEBUG_INFO);
    package
        .sections
        .push(Section::new(SectionId::DEBUG_INFO, debug_info.to_bytes()));
    package.debug_info().map_err(io::Error::other)?;

    Ok(())
}

/// Finds the source node and operation index of the `nop` declaration marker.
fn find_nop_marker(builder: &PackageDebugInfoBuilder) -> Option<(DebugSourceNodeId, u32)> {
    let info = builder.debug_info();
    for index in 0..info.nodes().len() {
        let node_id = DebugSourceNodeId::from(index as u32);
        let node = info.source_node(node_id)?;
        for row in &node.asm_ops {
            if info.get_string(row.op_name_idx).is_some_and(|op| op.as_ref() == "nop") {
                return Some((node_id, row.op_idx));
            }
        }
    }
    None
}

fn abi_metadata(
    builder: &mut PackageDebugInfoBuilder,
    op_idx: u32,
) -> Result<Vec<DebugSourceVar>, Box<dyn std::error::Error>> {
    let account_ty = Type::Struct(Arc::new(StructType::named(
        Arc::from("miden:base/core-types@1.0.0/account-id"),
        [(Arc::from("prefix"), Type::Felt), (Arc::from("suffix"), Type::Felt)],
    )));
    let array_ty = Type::Array(Arc::new(ArrayType::new(Type::U32, 3)));
    let option_ty = Type::Enum(Arc::new(
        EnumType::new(
            Arc::from("OptionU32"),
            Type::U32,
            [
                Variant::c_like(Arc::from("None"), Some(0)),
                Variant::new(Arc::from("Some"), Type::U32, Some(1)),
            ],
        )
        .map_err(|err| io::Error::other(err.to_string()))?,
    ));
    let point_ty = Type::Struct(Arc::new(StructType::new([
        (Arc::from("x"), Type::Felt),
        (Arc::from("y"), Type::Felt),
    ])));

    let account = register_type(builder, &account_ty)?;
    let array = register_type(builder, &array_ty)?;
    let bool_type = register_type(builder, &Type::I1)?;
    let option = register_type(builder, &option_ty)?;
    let point = register_type(builder, &point_ty)?;
    let u256_type = register_type(builder, &Type::U256)?;

    let variables = vec![
        typed_memory_var(builder, op_idx, "account", 130, account),
        typed_memory_var(builder, op_idx, "array", 110, array),
        typed_memory_var(builder, op_idx, "enabled", 150, bool_type),
        typed_memory_var(builder, op_idx, "maybe", 120, option),
        typed_memory_var(builder, op_idx, "point", 100, point),
        typed_memory_var(builder, op_idx, "wide", 140, u256_type),
    ];

    Ok(variables)
}

fn register_type(
    builder: &mut PackageDebugInfoBuilder,
    ty: &Type,
) -> Result<DebugTypeIdx, Box<dyn std::error::Error>> {
    builder
        .register_debug_type(None, None, ty)
        .map_err(|err| io::Error::other(err.to_string()).into())
}

fn typed_memory_var(
    builder: &mut PackageDebugInfoBuilder,
    op_idx: u32,
    name: &str,
    address: u32,
    type_id: DebugTypeIdx,
) -> DebugSourceVar {
    let name_idx: DebugStringIdx = builder.add_string(name);
    DebugSourceVar {
        op_idx,
        name_idx,
        type_id: Some(type_id),
        arg_idx: None,
        location_idx: None,
        value_location: DebugVarLocation::Memory(address),
    }
}

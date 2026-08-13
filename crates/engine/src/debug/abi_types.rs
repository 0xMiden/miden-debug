use std::{format, string::String, vec::Vec};

use miden_assembly_syntax::ast::types::{EnumType, Type};
use miden_core::Felt;

/// Number of memory bytes represented by a single felt in the canonical ABI layout.
const BYTES_PER_FELT: usize = 4;

/// Resolves and decodes the package debug type represented by `ty`.
///
/// `ty` is reconstructed from the package debug type table by
/// `SourceNode::debug_infos_for_operation`; this function only handles value rendering. Type names
/// are rendered by `Type`'s standard `Display` implementation at the call site.
pub fn format_value(
    ty: &Type,
    resolve_felts: impl FnOnce(usize) -> Option<Vec<Felt>>,
) -> Option<String> {
    if matches!(
        ty,
        Type::Unknown | Type::Never | Type::Ptr(_) | Type::List(_) | Type::Function(_)
    ) {
        return None;
    }

    let felts = resolve_felts(ty.size_in_felts())?;
    let (value, rest) = decode_value(&felts, ty)?;
    rest.is_empty().then_some(value)
}

fn decode_value<'a>(felts: &'a [Felt], ty: &Type) -> Option<(String, &'a [Felt])> {
    match ty {
        Type::Struct(struct_ty) => {
            let full_name = struct_ty.name().unwrap_or_default();
            let short_name = wit_short_name(&full_name).to_string();
            if is_account_id_type(&full_name)
                && let Some((rendered, rest)) = decode_account_id(felts)
            {
                return Some((rendered, rest));
            }

            if let [field] = struct_ty.fields() {
                let (inner, rest) = decode_value(felts, &field.ty)?;
                return Some((wrap_struct(&short_name, &inner), rest));
            }

            let mut cursor = felts;
            let mut rendered = Vec::with_capacity(struct_ty.fields().len());
            for field in struct_ty.fields() {
                let name = field_name(field);
                let (value, rest) = decode_value(cursor, &field.ty)?;
                rendered.push(format!("{name}={value}"));
                cursor = rest;
            }
            Some((wrap_struct(&short_name, &rendered.join(", ")), cursor))
        }
        Type::Array(array) => {
            let mut cursor = felts;
            let mut rendered = Vec::with_capacity(array.len());
            for _ in 0..array.len() {
                let (value, rest) = decode_value(cursor, array.element_type())?;
                rendered.push(value);
                cursor = rest;
            }
            Some((format!("[{}]", rendered.join(", ")), cursor))
        }
        Type::Enum(enum_ty) => decode_enum(felts, enum_ty),
        Type::Unknown | Type::Never | Type::Ptr(_) | Type::List(_) | Type::Function(_) => None,
        primitive => decode_primitive(felts, primitive),
    }
}

fn decode_enum<'a>(felts: &'a [Felt], enum_ty: &EnumType) -> Option<(String, &'a [Felt])> {
    let count = enum_ty.size_in_bytes().div_ceil(BYTES_PER_FELT);
    let (value, rest) = felts.split_at_checked(count)?;
    let discriminant = value.first()?.as_canonical_u64() as u128;
    let enum_name = wit_short_name(enum_ty.name());

    let mut next_discriminant = 0u128;
    let variant = enum_ty.variant_offsets().find(|(_, variant)| {
        let current = variant.discriminant_value.unwrap_or(next_discriminant);
        next_discriminant = current + 1;
        current == discriminant
    });

    let Some((payload_offset, variant)) = variant else {
        return Some((wrap_enum_variant(enum_name, &discriminant.to_string()), rest));
    };
    let Some(payload_type) = &variant.value else {
        return Some((wrap_enum_variant(enum_name, &variant.name), rest));
    };
    let (_, payload) = value.split_at_checked(payload_offset as usize / BYTES_PER_FELT)?;
    let (payload, _) = decode_value(payload, payload_type)?;
    Some((wrap_enum_variant(enum_name, &format!("{}({payload})", variant.name)), rest))
}

fn wrap_enum_variant(enum_name: &str, variant: &str) -> String {
    if enum_name.is_empty() {
        variant.into()
    } else {
        format!("{enum_name}::{variant}")
    }
}

fn decode_primitive<'a>(felts: &'a [Felt], primitive: &Type) -> Option<(String, &'a [Felt])> {
    match primitive {
        Type::U256 => {
            if felts.len() < 8 {
                return None;
            }
            let (chunk, rest) = felts.split_at(8);
            let limbs = chunk
                .iter()
                .map(|felt| felt.as_canonical_u64().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Some((format!("u256([{limbs}])"), rest))
        }
        Type::Felt => {
            let (head, rest) = felts.split_first()?;
            Some((format!("{head}"), rest))
        }
        Type::I1 => {
            let (head, rest) = felts.split_first()?;
            let value = head.as_canonical_u64();
            Some(((value != 0).to_string(), rest))
        }
        Type::I8
        | Type::I16
        | Type::I32
        | Type::I64
        | Type::U8
        | Type::U16
        | Type::U32
        | Type::U64 => {
            let (head, rest) = felts.split_first()?;
            Some((head.as_canonical_u64().to_string(), rest))
        }
        Type::I128 | Type::U128 | Type::F64 => {
            let (head, rest) = felts.split_first()?;
            Some((format!("{} (as {primitive})", head.as_canonical_u64()), rest))
        }
        _ => None,
    }
}

fn decode_account_id(felts: &[Felt]) -> Option<(String, &[Felt])> {
    if felts.len() < 2 {
        return None;
    }

    let (chunk, rest) = felts.split_at(2);
    let prefix = chunk[0].as_canonical_u64();
    let suffix = chunk[1].as_canonical_u64();
    let mut hex = format!("0x{prefix:016x}{suffix:016x}");
    hex.truncate(32);
    Some((format!("account-id({hex})"), rest))
}

fn wrap_struct(short_name: &str, body: &str) -> String {
    if short_name.is_empty() {
        format!("{{{body}}}")
    } else {
        format!("{short_name}({body})")
    }
}

fn field_name(field: &miden_assembly_syntax::ast::types::StructField) -> String {
    field
        .name
        .as_ref()
        .map(|name| name.as_ref().to_string())
        .unwrap_or_else(|| format!("f{}", field.index))
}

/// Extracts the trailing component of a WIT-style name such as
/// `miden:base/core-types@1.0.0/account-id`, treating compiler-generated
/// anonymous markers as unnamed.
fn wit_short_name(name: &str) -> &str {
    name.rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && *name != "<anon>" && *name != "<anonymous>")
        .unwrap_or("")
}

fn is_account_id_type(name: &str) -> bool {
    wit_short_name(name) == "account-id"
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_assembly_syntax::ast::types::{ArrayType, EnumType, StructType, Variant};

    use super::*;

    fn felt(value: u64) -> Felt {
        Felt::try_from(value).expect("value exceeds field modulus")
    }

    fn render(ty: &Type, felts: &[Felt]) -> Option<String> {
        format_value(ty, |count| (felts.len() >= count).then(|| felts[..count].to_vec()))
    }

    #[test]
    fn formats_account_id_struct() {
        let account_id = Type::Struct(Arc::new(StructType::named(
            Arc::from("miden:base/core-types@1.0.0/account-id"),
            [(Arc::from("prefix"), Type::Felt), (Arc::from("suffix"), Type::Felt)],
        )));

        let felts = [felt(0xa591_009a_3022_e800), felt(0x788f_9ed1_77dc_db00)];

        assert_eq!(
            render(&account_id, &felts).as_deref(),
            Some("account-id(0xa591009a3022e800788f9ed177dcdb)")
        );
    }

    #[test]
    fn formats_anonymous_struct_shape() {
        let point = Type::Struct(Arc::new(StructType::new([
            (Arc::from("x"), Type::Felt),
            (Arc::from("y"), Type::Felt),
        ])));

        assert_eq!(render(&point, &[felt(3), felt(4)]).as_deref(), Some("{x=3, y=4}"));
    }

    #[test]
    fn formats_payload_enum() {
        let option = Type::Enum(Arc::new(
            EnumType::new(
                Arc::from("OptionU32"),
                Type::U8,
                [
                    Variant::c_like(Arc::from("None"), Some(0)),
                    Variant::new(Arc::from("Some"), Type::U32, Some(1)),
                ],
            )
            .expect("valid enum type"),
        ));

        assert_eq!(render(&option, &[felt(1), felt(42)]).as_deref(), Some("OptionU32::Some(42)"));
    }

    #[test]
    fn formats_fixed_array() {
        let array = Type::Array(Arc::new(ArrayType::new(Type::U32, 3)));

        assert_eq!(render(&array, &[felt(5), felt(6), felt(7)]).as_deref(), Some("[5, 6, 7]"));
    }

    #[test]
    fn formats_u256_limbs() {
        let u256 = Type::U256;
        let felts = (1..=8).map(felt).collect::<Vec<_>>();

        assert_eq!(render(&u256, &felts).as_deref(), Some("u256([1, 2, 3, 4, 5, 6, 7, 8])"));
    }
}

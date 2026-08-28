use std::{boxed::Box, format, string::String, vec::Vec};

use miden_assembly_syntax::ast::types::{
    CallConv, FunctionType, MIDEN_CORE_TYPES, Type, TypedError, TypedProcInfo, WitScalarCodec,
};
use miden_core::Felt;
use miden_mast_package::Package;

/// Typed ABI metadata for a package entrypoint.
///
/// This is a small, cloneable wrapper around [`TypedProcInfo`]. A fresh typed view is constructed
/// for each encode/decode operation so all debugger frontends use the same codecs.
#[derive(Clone, Debug)]
pub struct TypedProcedure {
    name: String,
    signature: FunctionType,
}

impl TypedProcedure {
    pub fn new(name: impl Into<String>, signature: FunctionType) -> Result<Self, TypedError> {
        let procedure = Self {
            name: name.into(),
            signature,
        };
        procedure.info()?;
        Ok(procedure)
    }

    /// Returns typed metadata for the selected executable entrypoint, when it uses the canonical
    /// component-model ABI and carries a manifest signature.
    pub fn for_package_entrypoint(package: &Package) -> Option<Self> {
        let entrypoint = package.entrypoint()?;
        let export = package.manifest.get_export(entrypoint.as_ref())?.as_procedure()?;
        let signature = export.signature.clone()?;
        if signature.abi != CallConv::ComponentModel {
            return None;
        }
        Self::new(export.path.to_string(), signature).ok()
    }

    pub fn encode_args<T: AsRef<str>>(&self, args: &[T]) -> Result<Vec<Felt>, TypedError> {
        self.info()?.encode_args(args)
    }

    pub fn decode_result(&self, stack: &[Felt]) -> Result<Option<String>, TypedError> {
        self.info()?.decode_result(stack)
    }

    pub fn output_felt_count(&self) -> Result<Option<usize>, TypedError> {
        self.info().map(|info| info.output_felt_count())
    }

    pub fn display_signature(&self) -> Result<String, TypedError> {
        self.info().map(|info| info.to_string())
    }

    fn info(&self) -> Result<TypedProcInfo, TypedError> {
        TypedProcInfo::new(self.name.clone(), self.signature.clone())
            .map(|info| info.with_scalar_codec(Box::new(AccountIdCodec)))
    }
}

/// Resolves and decodes the package debug type represented by `ty`.
pub fn format_value(
    ty: &Type,
    resolve_felts: impl FnOnce(usize) -> Option<Vec<Felt>>,
) -> Option<String> {
    let decoder = value_decoder(ty)?;
    let felts = resolve_felts(decoder.output_felt_count().ok()??)?;
    decoder.decode_result(&felts).ok().flatten()
}

pub(crate) fn value_felt_count(ty: &Type) -> Option<usize> {
    value_decoder(ty)?.output_felt_count().ok()?
}

fn value_decoder(ty: &Type) -> Option<TypedProcedure> {
    TypedProcedure::new(
        "debug-variable",
        FunctionType::new(CallConv::ComponentModel, [], [ty.clone()]),
    )
    .ok()
}

struct AccountIdCodec;

impl WitScalarCodec for AccountIdCodec {
    fn wit_name(&self) -> &str {
        "account-id"
    }

    fn wit_interface(&self) -> Option<&str> {
        Some(MIDEN_CORE_TYPES)
    }

    fn encode(&self, token: &str) -> Result<Vec<Felt>, TypedError> {
        let token = token
            .strip_prefix("account-id(")
            .and_then(|token| token.strip_suffix(')'))
            .unwrap_or(token);
        let hex = token.strip_prefix("0x").ok_or_else(|| {
            self.invalid_scalar(token, "expected a 0x-prefixed, 15-byte account ID")
        })?;
        if hex.len() != 30 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(self.invalid_scalar(token, "expected exactly 30 hexadecimal digits"));
        }

        let prefix = u64::from_str_radix(&hex[..16], 16)
            .map_err(|_| self.invalid_scalar(token, "invalid account ID prefix"))?;
        let suffix = u64::from_str_radix(&hex[16..], 16)
            .map_err(|_| self.invalid_scalar(token, "invalid account ID suffix"))?
            << 8;
        let prefix = Felt::try_from(prefix).map_err(|_| {
            self.invalid_scalar(token, "account ID prefix exceeds the field modulus")
        })?;
        let suffix = Felt::try_from(suffix).map_err(|_| {
            self.invalid_scalar(token, "account ID suffix exceeds the field modulus")
        })?;

        Ok(vec![prefix, suffix])
    }

    fn decode(&self, felts: &[Felt]) -> Result<String, TypedError> {
        let [prefix, suffix] = felts else {
            return Err(TypedError::MalformedResult {
                ty: self.wit_name().into(),
                reason: "an account ID occupies exactly two felts",
            });
        };
        let mut hex =
            format!("0x{:016x}{:016x}", prefix.as_canonical_u64(), suffix.as_canonical_u64());
        hex.truncate(32);
        Ok(format!("account-id({hex})"))
    }
}

impl AccountIdCodec {
    fn invalid_scalar(&self, token: &str, reason: &str) -> TypedError {
        TypedError::InvalidScalar {
            wit_name: self.wit_name().into(),
            token: token.into(),
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_assembly_syntax::ast::types::{ArrayType, StructType};

    use super::*;

    fn felt(value: u64) -> Felt {
        Felt::try_from(value).expect("value exceeds field modulus")
    }

    fn render(ty: &Type, felts: &[Felt]) -> Option<String> {
        format_value(ty, |count| (felts.len() >= count).then(|| felts[..count].to_vec()))
    }

    #[test]
    fn formats_account_id_struct() {
        let account_id = Type::from(StructType::named(
            Arc::from("miden:base/core-types@1.0.0/account-id"),
            [(Arc::from("prefix"), Type::Felt), (Arc::from("suffix"), Type::Felt)],
        ));
        let felts = [felt(0xa591_009a_3022_e800), felt(0x788f_9ed1_77dc_db00)];

        assert_eq!(
            render(&account_id, &felts).as_deref(),
            Some("account-id(0xa591009a3022e800788f9ed177dcdb)")
        );

        let procedure = TypedProcedure::new(
            "take-account",
            FunctionType::new(CallConv::ComponentModel, [account_id], []),
        )
        .unwrap();
        assert_eq!(procedure.encode_args(&["0xa591009a3022e800788f9ed177dcdb"]).unwrap(), felts);
    }

    #[test]
    fn encodes_and_decodes_rust_abi_values() {
        let procedure = TypedProcedure::new(
            "roundtrip",
            FunctionType::new(CallConv::ComponentModel, [Type::U64, Type::I1], [Type::U64]),
        )
        .unwrap();

        assert_eq!(
            procedure.encode_args(&["4294967303", "true"]).unwrap(),
            [felt(7), felt(1), felt(1)]
        );
        assert_eq!(
            procedure.decode_result(&[felt(7), felt(1)]).unwrap().as_deref(),
            Some("4294967303u64")
        );
    }

    #[test]
    fn formats_anonymous_struct_shape() {
        let point = Type::from(StructType::new([
            (Arc::from("x"), Type::Felt),
            (Arc::from("y"), Type::Felt),
        ]));

        assert_eq!(render(&point, &[felt(3), felt(4)]).as_deref(), Some("{ x: 3, y: 4 }"));
    }

    #[test]
    fn formats_fixed_array() {
        let array = Type::Array(Arc::new(ArrayType::new(Type::U32, 3)));

        assert_eq!(render(&array, &[felt(5), felt(6), felt(7)]).as_deref(), Some("[5, 6, 7]"));
    }
}

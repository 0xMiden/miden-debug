use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use core::fmt;

use miden_assembly_syntax::{
    Report,
    diagnostics::{Diagnostic, miette},
};
use miden_core::serde::DeserializationError;
use miden_mast_package::{Package, PackageDebugInfoError};

const PACKAGE_MAGIC: &[u8; 5] = b"MASP\0";
const TOOLCHAIN_HELP: &str = "package, MAST, and debug-info formats are tied to the Miden \
                              toolchain that produced them; rerun with that toolchain, for \
                              example `miden +0.16.0 debug <FILE>`";

#[derive(Debug, thiserror::Error, Diagnostic)]
enum PackageLoadError {
    #[error("failed to load Miden package from {input}")]
    DecodePackage {
        input: String,
        #[source]
        cause: DeserializationError,
    },
    #[error("Miden package from {input} uses unsupported package format {version}")]
    #[diagnostic(code(miden_debug::incompatible_package_format))]
    IncompatiblePackageFormat {
        input: String,
        version: PackageFormatVersion,
        #[source]
        cause: DeserializationError,
        #[help]
        help: String,
    },
    #[error("failed to load debug information from Miden package {input}")]
    DecodeDebugInfo {
        input: String,
        #[source]
        cause: PackageDebugInfoError,
    },
    #[error("Miden package from {input} uses an unsupported debug-info format")]
    #[diagnostic(code(miden_debug::incompatible_debug_info_format))]
    IncompatibleDebugInfoFormat {
        input: String,
        #[source]
        cause: PackageDebugInfoError,
        #[help]
        help: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PackageFormatVersion([u8; 3]);

impl fmt::Display for PackageFormatVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.0[0], self.0[1], self.0[2])
    }
}

/// Decode a package and eagerly validate its debug-info section.
///
/// The processor and debugger must use matching package, MAST, and debug-info formats. When the
/// input carries a different format version, this returns an actionable diagnostic that points the
/// user to midenup's versioned toolchain invocation instead of implying that the artifact is
/// corrupt.
#[doc(hidden)]
pub fn read_package_from_bytes(
    bytes: &[u8],
    input: impl fmt::Display,
) -> Result<Arc<Package>, Report> {
    let input = input.to_string();
    let package = Package::read_from_bytes_trusted(bytes).map_err(|cause| {
        if let Some(version) = incompatible_package_format(bytes, &cause) {
            Report::new(PackageLoadError::IncompatiblePackageFormat {
                input: input.clone(),
                version,
                cause,
                help: TOOLCHAIN_HELP.into(),
            })
        } else {
            Report::new(PackageLoadError::DecodePackage {
                input: input.clone(),
                cause,
            })
        }
    })?;

    if let Err(cause) = package.debug_info() {
        let error = if is_incompatible_debug_info(&cause) {
            PackageLoadError::IncompatibleDebugInfoFormat {
                input,
                cause,
                help: TOOLCHAIN_HELP.into(),
            }
        } else {
            PackageLoadError::DecodeDebugInfo { input, cause }
        };
        return Err(Report::new(error));
    }

    Ok(Arc::new(package))
}

fn incompatible_package_format(
    bytes: &[u8],
    error: &DeserializationError,
) -> Option<PackageFormatVersion> {
    let DeserializationError::InvalidValue(message) = error else {
        return None;
    };
    if !message.starts_with("unsupported version.") {
        return None;
    }

    let version = bytes.strip_prefix(PACKAGE_MAGIC)?.get(..3)?.try_into().ok()?;
    Some(PackageFormatVersion(version))
}

fn is_incompatible_debug_info(error: &PackageDebugInfoError) -> bool {
    let PackageDebugInfoError::DecodeSection { source, .. } = error else {
        return false;
    };
    matches!(
        source,
        DeserializationError::InvalidValue(message)
            if message.starts_with("unsupported debug_info version:")
    )
}

#[cfg(test)]
mod tests {
    use miden_assembly::{Assembler, DefaultSourceManager};
    use miden_core::serde::Serializable;
    use miden_mast_package::SectionId;

    use super::*;

    #[test]
    fn current_package_with_debug_info_loads() {
        let package = Assembler::new(Arc::new(DefaultSourceManager::default()))
            .assemble_program("test", "begin push.1 drop end")
            .unwrap();

        let restored = read_package_from_bytes(&package.to_bytes(), "current.masp").unwrap();

        assert_eq!(restored.to_bytes(), package.to_bytes());
        assert!(restored.debug_info().unwrap().is_some());
    }

    #[test]
    fn incompatible_package_format_recommends_matching_toolchain() {
        let bytes = b"MASP\0\x06\x00\x00";

        let report = read_package_from_bytes(bytes, "old.masp").unwrap_err();
        let error = report
            .downcast_ref::<PackageLoadError>()
            .expect("expected a classified package load error");

        let PackageLoadError::IncompatiblePackageFormat { version, help, .. } = error else {
            panic!("expected an incompatible package format error, got {error:?}");
        };
        assert_eq!(*version, PackageFormatVersion([6, 0, 0]));
        assert!(help.contains("miden +0.16.0 debug"));
    }

    #[test]
    fn incompatible_debug_info_recommends_matching_toolchain() {
        let mut package = Assembler::new(Arc::new(DefaultSourceManager::default()))
            .assemble_program("test", "begin push.1 drop end")
            .unwrap();
        let debug_info = package
            .sections
            .iter_mut()
            .find(|section| section.id == SectionId::DEBUG_INFO)
            .expect("assembled package should contain debug info");
        debug_info.data.to_mut()[0] = u8::MAX;

        let report = read_package_from_bytes(&package.to_bytes(), "old-debug.masp").unwrap_err();
        let error = report
            .downcast_ref::<PackageLoadError>()
            .expect("expected a classified package load error");

        let PackageLoadError::IncompatibleDebugInfoFormat { help, .. } = error else {
            panic!("expected an incompatible debug-info format error, got {error:?}");
        };
        assert!(help.contains("miden +0.16.0 debug"));
    }

    #[test]
    fn malformed_package_does_not_get_a_version_hint() {
        let report = read_package_from_bytes(b"not a package", "broken.masp").unwrap_err();
        let error = report
            .downcast_ref::<PackageLoadError>()
            .expect("expected a classified package load error");

        assert!(matches!(error, PackageLoadError::DecodePackage { .. }));
    }
}

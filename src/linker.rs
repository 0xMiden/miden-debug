use std::{
    borrow::Cow,
    fmt,
    path::{Path as FsPath, PathBuf},
    str::FromStr,
    sync::Arc,
};

use miden_assembly::SourceManager;
use miden_assembly_syntax::{
    Library, Path as LibraryPath,
    diagnostics::{IntoDiagnostic, Report},
};
use miden_core::serde::{
    BudgetedReader, ByteReader, Deserializable, DeserializationError, SliceReader,
};
use miden_mast_package::{Package, PackageId, PackageManifest, Section, TargetType};

use crate::config::DebuggerConfig;

/// A library requested by the user to be linked against during compilation
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkLibrary {
    /// The name of the library.
    ///
    /// If requested by name, e.g. `-l std`, the name is used as given.
    ///
    /// If requested by path, e.g. `-l ./target/libs/miden-base.masp`, then the name of the library
    /// will be the basename of the file specified in the path.
    pub name: Cow<'static, str>,
    /// If specified, the path from which this library should be loaded
    pub path: Option<PathBuf>,
    /// The kind of library to load.
    ///
    /// By default this is assumed to be a `.masp` package, but the kind will be detected based on
    /// how it is requested by the user. It may also be specified explicitly by the user.
    pub kind: LibraryKind,
}

/// The types of libraries that can be linked against during compilation
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum LibraryKind {
    /// A Miden package (MASP)
    #[default]
    Masp,
    /// A source-form MASM library, using the standard project layout
    Masm,
}
impl fmt::Display for LibraryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Masm => f.write_str("masm"),
            Self::Masp => f.write_str("masp"),
        }
    }
}
impl FromStr for LibraryKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "masm" => Ok(Self::Masm),
            "masp" => Ok(Self::Masp),
            _ => Err(()),
        }
    }
}

impl LinkLibrary {
    /// Get the name of this library
    pub fn name(&self) -> &str {
        self.name.as_ref()
    }

    pub fn load(
        &self,
        config: &DebuggerConfig,
        source_manager: Arc<dyn SourceManager>,
    ) -> Result<Arc<Library>, Report> {
        if matches!(self.kind, LibraryKind::Masp) {
            return Ok(self.load_package(config)?.mast.clone());
        }

        if let Some(path) = self.path.as_deref() {
            return self.load_from_path(path, source_manager);
        }

        // Search for library among specified search paths
        let path = self.find(config)?;

        self.load_from_path(&path, source_manager)
    }

    pub(crate) fn load_package(
        &self,
        config: &DebuggerConfig,
    ) -> Result<Arc<miden_mast_package::Package>, Report> {
        if self.kind != LibraryKind::Masp {
            return Err(Report::msg(format!(
                "source-form MASM library '{}' cannot be linked while debugging a package; pass a \
                 compiled .masp package instead",
                self.name
            )));
        }

        let path = match self.path.as_deref() {
            Some(path) => Cow::Borrowed(path),
            None => Cow::Owned(self.find(config)?),
        };

        let package = load_package_from_path(&path)?;
        if !package.is_library() {
            return Err(Report::msg(format!(
                "link library '{}' resolved to executable package '{}'; expected a library package",
                self.name, package.name
            )));
        }

        Ok(package)
    }

    fn load_from_path(
        &self,
        path: &FsPath,
        source_manager: Arc<dyn SourceManager>,
    ) -> Result<Arc<Library>, Report> {
        match self.kind {
            LibraryKind::Masm => {
                let ns = LibraryPath::validate(self.name.as_ref()).map_err(|err| {
                    Report::msg(format!("invalid library namespace '{}': {err}", &self.name))
                })?;

                let modules = miden_assembly_syntax::parser::read_modules_from_dir(
                    path,
                    ns,
                    source_manager.clone(),
                    false,
                )?;

                miden_assembly::Assembler::new(source_manager).assemble_library(modules)
            }
            LibraryKind::Masp => {
                let package = load_package_from_path(path)?;
                Ok(package.mast.clone())
            }
        }
    }

    fn find(&self, config: &DebuggerConfig) -> Result<PathBuf, Report> {
        use std::fs;

        let toolchain_dir = config.toolchain_dir();
        let toolchain_lib_dir = toolchain_dir.as_ref().map(|dir| dir.join("lib"));
        let search_paths = toolchain_lib_dir
            .iter()
            .chain(toolchain_dir.iter())
            .chain(config.search_path.iter())
            .chain(config.working_dir.iter());

        for search_path in search_paths {
            let reader = fs::read_dir(search_path).map_err(|err| {
                Report::msg(format!(
                    "invalid library search path '{}': {err}",
                    search_path.display()
                ))
            })?;
            for entry in reader {
                let Ok(entry) = entry else {
                    continue;
                };
                let path = entry.path();
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                if stem != self.name.as_ref() {
                    continue;
                }

                match self.kind {
                    LibraryKind::Masp => {
                        if !path.is_file() {
                            return Err(Report::msg(format!(
                                "unable to load Miden Assembly package from '{}': not a file",
                                path.display()
                            )));
                        }
                    }
                    LibraryKind::Masm => {
                        if !path.is_dir() {
                            return Err(Report::msg(format!(
                                "unable to load Miden Assembly library from '{}': not a directory",
                                path.display()
                            )));
                        }
                    }
                }
                return Ok(path);
            }
        }

        Err(Report::msg(format!(
            "unable to locate library '{}' using any of the provided search paths",
            &self.name
        )))
    }
}

pub(crate) fn load_package_from_path(
    path: &FsPath,
) -> Result<Arc<miden_mast_package::Package>, Report> {
    let bytes = std::fs::read(path).into_diagnostic()?;
    load_package_from_bytes(&bytes, &path.display().to_string())
}

/// Load a package, preferring the unchecked reader: packages produced with debug info trip the
/// untrusted deserializer's STRIPPED/HASHLESS expectations, which logs spurious errors on every
/// load even though the read succeeds. The strict reader remains the fallback (and the source of
/// the error message) for artifacts the unchecked reader does not understand.
pub(crate) fn load_package_from_bytes(
    bytes: &[u8],
    source: &str,
) -> Result<Arc<miden_mast_package::Package>, Report> {
    match read_package_from_bytes_unchecked(bytes) {
        Ok(package) => {
            log::warn!(
                "loading Miden package '{source}' without validating embedded MAST node hashes; \
                 use only trusted local build artifacts"
            );
            Ok(Arc::new(package))
        }
        Err(_) => Package::read_from_bytes(bytes).map(Arc::new).map_err(|strict_error| {
            Report::msg(format!("failed to load Miden package from {source}: {strict_error}"))
        }),
    }
}

pub(crate) fn load_local_package_from_path(path: &FsPath) -> Result<Arc<Package>, Report> {
    let bytes = std::fs::read(path).into_diagnostic()?;
    match read_package_from_bytes_unchecked(&bytes) {
        Ok(package) => Ok(Arc::new(package)),
        Err(unchecked_error) => {
            Package::read_from_bytes(&bytes).map(Arc::new).map_err(|strict_error| {
                Report::msg(format!(
                    "failed to load local Miden package from {}: unchecked reader failed: {}; \
                     strict reader failed: {}",
                    path.display(),
                    unchecked_error,
                    strict_error
                ))
            })
        }
    }
}

fn read_package_from_bytes_unchecked(bytes: &[u8]) -> Result<Package, DeserializationError> {
    const PACKAGE_BYTE_READ_BUDGET_MULTIPLIER: usize = 64;

    let budget = bytes.len().saturating_mul(PACKAGE_BYTE_READ_BUDGET_MULTIPLIER);
    let mut reader = BudgetedReader::new(SliceReader::new(bytes), budget);
    read_package_unchecked(&mut reader)
}

fn read_package_unchecked<R: ByteReader>(source: &mut R) -> Result<Package, DeserializationError> {
    let magic: [u8; 5] = source.read_array()?;
    if magic != *b"MASP\0" {
        return Err(DeserializationError::InvalidValue(format!(
            "invalid magic bytes. Expected '{:?}', got '{magic:?}'",
            b"MASP\0"
        )));
    }

    let format_version: [u8; 3] = source.read_array()?;
    if format_version != [4, 0, 0] {
        return Err(DeserializationError::InvalidValue(format!(
            "unsupported version. Got '{format_version:?}', but only '[4, 0, 0]' is supported"
        )));
    }

    let name = PackageId::read_from(source)?;
    let version = String::read_from(source)?
        .parse()
        .map_err(|err| DeserializationError::InvalidValue(format!("{err}")))?;
    let description = Option::<String>::read_from(source)?;
    let kind_tag = source.read_u8()?;
    let kind = TargetType::try_from(kind_tag)
        .map_err(|err| DeserializationError::InvalidValue(err.to_string()))?;
    let mast = Arc::new(Library::read_from_unchecked(source)?);
    let manifest = PackageManifest::read_from(source)?;
    let sections = Vec::<Section>::read_from(source)?;

    Ok(Package {
        name,
        version,
        description,
        kind,
        mast,
        manifest,
        sections,
    })
}

#[cfg(any(feature = "tui", feature = "repl", feature = "flamegraph"))]
impl clap::builder::ValueParserFactory for LinkLibrary {
    type Parser = LinkLibraryParser;

    fn value_parser() -> Self::Parser {
        LinkLibraryParser
    }
}

#[cfg(any(feature = "tui", feature = "repl", feature = "flamegraph"))]
#[doc(hidden)]
#[derive(Clone)]
pub struct LinkLibraryParser;

#[cfg(any(feature = "tui", feature = "repl", feature = "flamegraph"))]
impl clap::builder::TypedValueParser for LinkLibraryParser {
    type Value = LinkLibrary;

    fn possible_values(
        &self,
    ) -> Option<Box<dyn Iterator<Item = clap::builder::PossibleValue> + '_>> {
        use clap::builder::PossibleValue;

        Some(Box::new(
            [
                PossibleValue::new("masm").help("A Miden Assembly project directory"),
                PossibleValue::new("masp").help("A compiled Miden package file"),
            ]
            .into_iter(),
        ))
    }

    /// Parses the `-l` flag using the following format:
    ///
    /// `-l[KIND=]NAME`
    ///
    /// * `KIND` is one of: `masp`, `masm`; defaults to `masp`
    /// * `NAME` is either an absolute path, or a name (without extension)
    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::error::Error> {
        use clap::error::{Error, ErrorKind};

        let value = value.to_str().ok_or_else(|| Error::new(ErrorKind::InvalidUtf8))?;
        let (kind, name) = value
            .split_once('=')
            .map(|(kind, name)| (Some(kind), name))
            .unwrap_or((None, value));

        if name.is_empty() {
            return Err(Error::raw(
                ErrorKind::ValueValidation,
                "invalid link library: must specify a name or path",
            ));
        }

        let maybe_path = FsPath::new(name);
        let extension = maybe_path.extension().map(|ext| ext.to_str().unwrap());
        let kind = match kind {
            Some(kind) if !kind.is_empty() => kind.parse::<LibraryKind>().map_err(|_| {
                Error::raw(ErrorKind::InvalidValue, format!("'{kind}' is not a valid library kind"))
            })?,
            Some(_) | None => match extension {
                Some(kind) => kind.parse::<LibraryKind>().map_err(|_| {
                    Error::raw(
                        ErrorKind::InvalidValue,
                        format!("'{kind}' is not a valid library kind"),
                    )
                })?,
                None => LibraryKind::default(),
            },
        };

        if maybe_path.is_absolute() {
            let meta = maybe_path.metadata().map_err(|err| {
                Error::raw(
                    ErrorKind::ValueValidation,
                    format!(
                        "invalid link library: unable to load '{}': {err}",
                        maybe_path.display()
                    ),
                )
            })?;

            match kind {
                LibraryKind::Masp if !meta.is_file() => {
                    return Err(Error::raw(
                        ErrorKind::ValueValidation,
                        format!("invalid link library: '{}' is not a file", maybe_path.display()),
                    ));
                }
                LibraryKind::Masm if !meta.is_dir() => {
                    return Err(Error::raw(
                        ErrorKind::ValueValidation,
                        format!(
                            "invalid link library: kind 'masm' was specified, but '{}' is not a \
                             directory",
                            maybe_path.display()
                        ),
                    ));
                }
                _ => (),
            }

            let name = maybe_path.file_stem().unwrap().to_str().unwrap().to_string();

            Ok(LinkLibrary {
                name: name.into(),
                path: Some(maybe_path.to_path_buf()),
                kind,
            })
        } else if extension.is_some() {
            let name = name.strip_suffix(unsafe { extension.unwrap_unchecked() }).unwrap();
            let mut name = name.to_string();
            name.pop();

            Ok(LinkLibrary {
                name: name.into(),
                path: None,
                kind,
            })
        } else {
            Ok(LinkLibrary {
                name: name.to_string().into(),
                path: None,
                kind,
            })
        }
    }
}

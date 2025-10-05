use std::{
    borrow::Cow,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use miden_assembly::SourceManager;
use miden_assembly_syntax::{
    Library, LibraryNamespace,
    diagnostics::{IntoDiagnostic, Report, WrapErr},
};

use crate::config::DebuggerConfig;

/// A library requested by the user to be linked against during compilation
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkLibrary {
    /// The name of the library.
    ///
    /// If requested by name, e.g. `-l std`, the name is used as given.
    ///
    /// If requested by path, e.g. `-l ./target/libs/miden-base.masl`, then the name of the library
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

    /// Construct a LinkLibrary for the Miden standard library
    pub fn std() -> Self {
        LinkLibrary {
            name: "std".into(),
            path: None,
            kind: LibraryKind::Masp,
        }
    }

    /// Construct a LinkLibrary for Miden base(protocol/tx kernel) library
    pub fn base() -> Self {
        LinkLibrary {
            name: "base".into(),
            path: None,
            kind: LibraryKind::Masp,
        }
    }

    pub fn load(
        &self,
        config: &DebuggerConfig,
        source_manager: Arc<dyn SourceManager>,
    ) -> Result<Arc<Library>, Report> {
        if let Some(path) = self.path.as_deref() {
            return self.load_from_path(path, source_manager);
        }

        // Search for library among specified search paths
        let path = self.find(config)?;

        self.load_from_path(&path, source_manager)
    }

    fn load_from_path(
        &self,
        path: &Path,
        source_manager: Arc<dyn SourceManager>,
    ) -> Result<Arc<Library>, Report> {
        match self.kind {
            LibraryKind::Masm => {
                let ns = LibraryNamespace::new(&self.name)
                    .into_diagnostic()
                    .wrap_err_with(|| format!("invalid library namespace '{}'", &self.name))?;

                let modules = miden_assembly_syntax::parser::read_modules_from_dir(
                    ns,
                    path,
                    &source_manager,
                )?;

                miden_assembly::Assembler::new(source_manager.clone())
                    .with_debug_mode(true)
                    .assemble_library(modules)
                    .map(Arc::new)
            }
            LibraryKind::Masp => {
                use miden_core::utils::Deserializable;
                let bytes = std::fs::read(path).into_diagnostic()?;
                let package =
                    miden_mast_package::Package::read_from_bytes(&bytes).map_err(|e| {
                        Report::msg(format!(
                            "failed to load Miden package from {}: {e}",
                            path.display()
                        ))
                    })?;
                let lib = match package.mast {
                    miden_mast_package::MastArtifact::Executable(_) => {
                        return Err(Report::msg(format!(
                            "Expected Miden package to contain a Library, got Program: '{}'",
                            path.display()
                        )));
                    }
                    miden_mast_package::MastArtifact::Library(lib) => lib.clone(),
                };
                Ok(lib)
            }
        }
    }

    fn find(&self, config: &DebuggerConfig) -> Result<PathBuf, Report> {
        use std::fs;

        let toolchain_dir = config.toolchain_dir();
        let search_paths = toolchain_dir
            .iter()
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

#[cfg(feature = "tui")]
impl clap::builder::ValueParserFactory for LinkLibrary {
    type Parser = LinkLibraryParser;

    fn value_parser() -> Self::Parser {
        LinkLibraryParser
    }
}

#[cfg(feature = "tui")]
#[doc(hidden)]
#[derive(Clone)]
pub struct LinkLibraryParser;

#[cfg(feature = "tui")]
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

        let value = value
            .to_str()
            .ok_or_else(|| Error::new(ErrorKind::InvalidUtf8))?;
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

        let maybe_path = Path::new(name);
        let extension = maybe_path.extension().map(|ext| ext.to_str().unwrap());
        let kind = match kind {
            Some(kind) if !kind.is_empty() => kind.parse::<LibraryKind>().map_err(|_| {
                Error::raw(
                    ErrorKind::InvalidValue,
                    format!("'{kind}' is not a valid library kind"),
                )
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
                        format!(
                            "invalid link library: '{}' is not a file",
                            maybe_path.display()
                        ),
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

            let name = maybe_path
                .file_stem()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();

            Ok(LinkLibrary {
                name: name.into(),
                path: Some(maybe_path.to_path_buf()),
                kind,
            })
        } else if extension.is_some() {
            let name = name
                .strip_suffix(unsafe { extension.unwrap_unchecked() })
                .unwrap();
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

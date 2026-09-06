use alloc::boxed::Box;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use miden_assembly::{DefaultSourceManager, Linkage, ProjectTargetSelector};
use miden_assembly_syntax::diagnostics::{IntoDiagnostic, Report};
use miden_mast_package::{Package, PackageId};
use miden_package_registry::PackageCache;

use crate::read_package_from_bytes;

/// A library requested by the user to be linked against during compilation
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkLibrary {
    /// The name of the library.
    ///
    /// If requested by name, e.g. `-l std`, the name is used as given.
    ///
    /// If requested by path, e.g. `-l ./target/libs/miden-base.masl`, then the name of the library
    /// will be the basename of the file specified in the path.
    pub name: PackageId,
    /// If specified, the path from which this library should be loaded
    pub path: Option<PathBuf>,
    /// How to link against this library
    pub linkage: Linkage,
}

impl LinkLibrary {
    /// Get the name of this library
    pub fn name(&self) -> &str {
        self.name.as_ref()
    }

    pub fn is_core(&self) -> bool {
        matches!(self.name.as_ref(), "miden-core" | "core" | "std")
    }

    pub fn is_protocol(&self) -> bool {
        matches!(self.name.as_ref(), "miden-protocol" | "protocol" | "base")
    }

    pub fn load<S>(
        &self,
        search_paths: &[PathBuf],
        registry: &mut S,
    ) -> Result<Arc<Package>, Report>
    where
        S: PackageCache<Error = Report>,
    {
        if let Some(path) = self.path.as_deref() {
            return self.load_from_path(path, registry);
        }

        // Search for library among specified search paths
        let path = self.find(search_paths)?;

        self.load_from_path(&path, registry)
    }

    fn load_from_path<S>(&self, path: &Path, registry: &mut S) -> Result<Arc<Package>, Report>
    where
        S: PackageCache<Error = Report>,
    {
        if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("masm")) {
            let source_manager = Arc::new(DefaultSourceManager::default());
            return miden_assembly::Assembler::new(source_manager)
                .assemble_library_from_root(path, None)
                .map(Arc::from);
        }

        if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("masp")) {
            let bytes = std::fs::read(path).into_diagnostic()?;
            return read_package_from_bytes(&bytes, path.display());
        }

        let source_manager = Arc::new(DefaultSourceManager::default());
        let assembler = miden_assembly::Assembler::new(source_manager);
        let mut project_assembler = assembler.for_project_at_path(path, registry)?;
        project_assembler.assemble(ProjectTargetSelector::Library, "release")
    }

    fn find(&self, search_paths: &[PathBuf]) -> Result<PathBuf, Report> {
        use std::fs;

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
                if path.extension().is_none_or(|ext| !ext.eq_ignore_ascii_case("masp")) {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                    continue;
                };
                if stem != self.name() {
                    continue;
                }

                if !path.is_file() {
                    return Err(Report::msg(format!(
                        "unable to load Miden Assembly package from '{}': not a file",
                        path.display()
                    )));
                }
                return Ok(path);
            }
        }

        Err(Report::msg(format!(
            "unable to locate library '{}' using any of the provided search paths",
            self.name
        )))
    }
}

pub(crate) fn load_package_from_path(path: &Path) -> Result<Arc<Package>, Report> {
    let bytes = std::fs::read(path).into_diagnostic()?;
    read_package_from_bytes(&bytes, path.display())
}

#[cfg(feature = "std")]
impl clap::builder::ValueParserFactory for LinkLibrary {
    type Parser = LinkLibraryParser;

    fn value_parser() -> Self::Parser {
        LinkLibraryParser
    }
}

#[cfg(feature = "std")]
#[doc(hidden)]
#[derive(Clone)]
pub struct LinkLibraryParser;

#[cfg(feature = "std")]
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
    /// `-l[KIND[:<LINKAGE>]=]NAME`
    ///
    /// * `KIND` is one of: `masp`, `masm`; defaults to `masp`
    /// * `LINKAGE` is one of: `static`, `dynamic`; defaults to `dynamic`
    /// * `NAME` is either a path, or a name (without extension)
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

        let linkage = match kind {
            Some(kind) => match kind.split_once(':') {
                Some(("masp" | "masm", "static")) => Linkage::Static,
                Some(("masp" | "masm", "dynamic")) => Linkage::Dynamic,
                Some(("masp" | "masm", other)) => {
                    return Err(Error::raw(
                        ErrorKind::ValueValidation,
                        format!("unrecognized linkage modifier '{other}'"),
                    ));
                }
                None if matches!(kind, "masp" | "masm") => Linkage::Dynamic,
                Some(_) | None => {
                    return Err(Error::raw(
                        ErrorKind::ValueValidation,
                        "invalid link library kind: supported values are 'masp'",
                    ));
                }
            },
            None => Linkage::Dynamic,
        };

        if name.is_empty() {
            return Err(Error::raw(
                ErrorKind::ValueValidation,
                "invalid link library: must specify a name or path",
            ));
        }

        let maybe_path = Path::new(name);
        let extension = maybe_path.extension().map(|ext| ext.to_str().unwrap());
        let is_package = match kind {
            Some("masp") => true,
            Some("masm") => false,
            Some(kind) => {
                return Err(Error::raw(
                    ErrorKind::InvalidValue,
                    format!("'{kind}' is not a valid library kind"),
                ));
            }
            None => match extension {
                Some("masp") => true,
                Some("masm") | Some("toml") | None => false,
                Some(kind) => {
                    return Err(Error::raw(
                        ErrorKind::InvalidValue,
                        format!("'{kind}' is not a valid library kind"),
                    ));
                }
            },
        };

        let path = match maybe_path.components().count() {
            _ if extension.is_some() || maybe_path.is_dir() => {
                // If the path had an extension or exists as a directory, then we always treat it
                // like a path
                maybe_path.canonicalize().map_err(|err| {
                    Error::raw(
                        ErrorKind::ValueValidation,
                        format!("invalid link library '{}': {err}", maybe_path.display()),
                    )
                })?
            }
            1 => {
                // A single component path with no extension/not present as a direcotry is treated
                // as a library name, not a file path
                let name = maybe_path.file_name().unwrap().to_str().unwrap();
                return Ok(LinkLibrary {
                    name: name.into(),
                    path: None,
                    linkage,
                });
            }
            _ => {
                // A multi-component path is always treated as a path
                maybe_path.canonicalize().map_err(|err| {
                    Error::raw(
                        ErrorKind::ValueValidation,
                        format!("invalid link library: '{}': {err}", maybe_path.display()),
                    )
                })?
            }
        };

        // Normalize path and validate link library info
        let extension = path.extension();
        if is_package {
            // We require a .masp path for packages
            if extension.is_none_or(|ext| !ext.eq_ignore_ascii_case("masp")) {
                return Err(Error::raw(
                    ErrorKind::ValueValidation,
                    format!(
                        "invalid link library: expected '{}' to refer to a .masp file",
                        path.display()
                    ),
                ));
            }

            let name = path.file_stem().unwrap().to_str().unwrap();
            return Ok(LinkLibrary {
                name: name.into(),
                path: Some(path),
                linkage,
            });
        }

        let normalized_path = if extension.is_none() {
            path.join("miden-project.toml")
        } else {
            path.clone()
        };
        match extension {
            _ if normalized_path.ends_with("miden-project.toml") => {
                // We got a path to a project
                let source_manager = DefaultSourceManager::default();
                let name = match miden_project::Project::load(&normalized_path, &source_manager) {
                    Ok(
                        miden_project::Project::Package(package)
                        | miden_project::Project::WorkspacePackage { package, .. },
                    ) => package.name().into_inner(),
                    Err(err) => return Err(Error::raw(ErrorKind::ValueValidation, err)),
                };
                Ok(LinkLibrary {
                    name,
                    path: Some(normalized_path),
                    linkage,
                })
            }
            Some(ext) if ext.eq_ignore_ascii_case("masm") => {
                // We got a single MASM file
                let name = normalized_path.file_stem().unwrap().to_str().unwrap();
                Ok(LinkLibrary {
                    name: name.into(),
                    path: Some(normalized_path),
                    linkage,
                })
            }
            Some(_) => Err(Error::raw(
                ErrorKind::ValueValidation,
                format!(
                    "invalid link library: unrecognized file extension for '{}'",
                    normalized_path.display()
                ),
            )),
            // A missing extension must be a directory
            None => Err(Error::raw(
                ErrorKind::ValueValidation,
                format!(
                    "invalid link library: expected '{}' to be a directory, or have an explicit \
                     extension",
                    normalized_path.display()
                ),
            )),
        }
    }
}

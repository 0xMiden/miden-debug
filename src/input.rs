use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use crate::linker::LibraryKind;

#[derive(Debug, Clone)]
pub enum InputFile {
    Real(PathBuf),
    Stdin(Box<[u8]>),
}

impl Default for InputFile {
    fn default() -> Self {
        Self::Stdin(Box::from([]))
    }
}

impl InputFile {
    pub fn file_name(&self) -> &str {
        match self {
            Self::Real(path) => {
                path.file_name().and_then(|name| name.to_str()).unwrap_or("<noname>")
            }
            Self::Stdin(_) => "<noname>",
        }
    }

    pub fn bytes(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::Real(path) => std::fs::read(path).ok().map(Cow::Owned),
            Self::Stdin(bytes) => Some(Cow::Borrowed(bytes)),
        }
    }

    pub fn library_kind(&self) -> Option<LibraryKind> {
        match self {
            Self::Real(path) if path.is_file() => {
                if path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| ext == "masp") {
                    return Some(LibraryKind::Masp);
                }
                let bytes = std::fs::read(path).ok()?;
                if bytes.starts_with(b"MASP\0") {
                    Some(LibraryKind::Masp)
                } else {
                    None
                }
            }
            // Assume the path is a MASM project
            Self::Real(_) => Some(LibraryKind::Masm),
            Self::Stdin(bytes) if bytes.starts_with(b"MASP\0") => Some(LibraryKind::Masp),
            // Assume the input is MASM text
            Self::Stdin(_) => Some(LibraryKind::Masm),
        }
    }

    /// Get an [InputFile] representing the contents of `path`.
    ///
    /// This function returns an error if the contents are not a valid supported file type.
    #[cfg(feature = "std")]
    pub fn from_path<P: AsRef<Path>>(path: P) -> Self {
        let path = path.as_ref();
        Self::Real(path.to_path_buf())
    }

    /// Get an [InputFile] representing the contents received from standard input.
    ///
    /// This function returns an error if the contents are not a valid supported file type.
    #[cfg(feature = "std")]
    pub fn from_stdin() -> Result<Self, std::io::Error> {
        use std::io::Read;

        let mut input = Vec::with_capacity(1024);
        std::io::stdin().read_to_end(&mut input)?;
        Ok(Self::Stdin(input.into_boxed_slice()))
    }
}

#[cfg(feature = "std")]
impl clap::builder::ValueParserFactory for InputFile {
    type Parser = InputFileParser;

    fn value_parser() -> Self::Parser {
        InputFileParser
    }
}

#[doc(hidden)]
#[derive(Clone)]
#[cfg(feature = "std")]
pub struct InputFileParser;

#[cfg(feature = "std")]
impl clap::builder::TypedValueParser for InputFileParser {
    type Value = InputFile;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::error::Error> {
        use clap::error::{Error, ErrorKind};

        let input_file = match value.to_str() {
            Some("-") => InputFile::from_stdin().map_err(|err| Error::raw(ErrorKind::Io, err))?,
            Some(_) | None => InputFile::from_path(PathBuf::from(value)),
        };

        match &input_file {
            InputFile::Real(path) => {
                if !path.exists() {
                    return Err(Error::raw(
                        ErrorKind::ValueValidation,
                        format!("invalid input '{}': file does not exist", path.display()),
                    ));
                }
            }
            InputFile::Stdin(_) => (),
        }

        Ok(input_file)
    }
}

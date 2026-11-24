use rcmake_base::{BString, path_lex::Lexical};

/// Source form used by the compiler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceForm {
    /// Free form with ampersand continuations.
    Free,
    /// Fixed form with a continuation marker in column six.
    Fixed,
}

impl SourceForm {
    /// Select the source form from a conventional, case-insensitive file extension.
    pub fn from_path(path: &[u8]) -> Option<Self> {
        let (_, ext) = Lexical::new(path).stem_extension(true);
        match ext.to_ascii_lowercase().as_slice() {
            b".f" | b".for" | b".ftn" | b".f77" | b".fpp" => Some(Self::Fixed),
            b".f90" | b".f95" | b".f03" | b".f08" | b".f18" | b".f23" => Some(Self::Free),
            _ => None,
        }
    }
}

/// Letter case of module file basenames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleCase {
    /// Lowercase names.
    Lower,
    /// Uppercase names.
    Upper,
}

/// Compiler module file naming settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FortranCompiler<'a> {
    /// CMake Fortran compiler ID.
    pub id: &'a [u8],
    /// Separator between the ancestor module and submodule names.
    pub submodule_sep: &'a [u8],
    /// Submodule file extension, including the leading dot.
    pub submodule_ext: &'a [u8],
    /// Letter case of the basename.
    pub module_case: ModuleCase,
}

impl<'a> FortranCompiler<'a> {
    /// Get the default naming settings for a compiler ID.
    ///
    /// Callers with toolchain metadata can set the separator, extension, and
    /// case from that metadata.
    pub fn from_id(id: &'a [u8]) -> Self {
        let (submodule_sep, submodule_ext): (&[u8], &[u8]) = match id {
            b"Flang" | b"LLVMFlang" | b"Intel" | b"IntelLLVM" => (b"-", b".smod"),
            b"NVHPC" | b"PGI" => (b"-", b".mod"),
            b"Cray" => (b"", b".mod"),
            b"XL" => (b"_", b".smod"),
            b"Fujitsu" => (b".", b".smod"),
            b"NAG" => (b".", b".sub"),
            _ => (b"@", b".smod"),
        };
        let module_case = match id {
            b"Cray" | b"PGI" | b"Absoft" => ModuleCase::Upper,
            _ => ModuleCase::Lower,
        };
        Self {
            id,
            submodule_sep,
            submodule_ext,
            module_case,
        }
    }

    /// Map a logical name to a file basename. Logical submodule names use
    /// ancestor@child independently of the compiler.
    pub fn module_filename(&self, logical: &[u8]) -> Option<BString> {
        let mut parts = logical.splitn(2, |&byte| byte == b'@');
        let parent = parts.next()?;
        if !identifier(parent) {
            return None;
        }
        let mut basename = BString::from(parent);
        let extension = if let Some(child) = parts.next() {
            if !identifier(child) {
                return None;
            }
            basename.extend_from_slice(self.submodule_sep);
            basename.extend_from_slice(child);
            self.submodule_ext
        } else {
            b".mod"
        };
        match self.module_case {
            ModuleCase::Lower => basename.make_ascii_lowercase(),
            ModuleCase::Upper => basename.make_ascii_uppercase(),
        }
        basename.extend_from_slice(extension);
        Some(basename)
    }
}

pub(crate) fn identifier(word: &[u8]) -> bool {
    word.first().is_some_and(u8::is_ascii_alphabetic)
        && word
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
}

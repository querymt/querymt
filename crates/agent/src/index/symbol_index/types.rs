use crate::hash::RapidHash;

/// A compact digest of a symbol's body content for change detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolDigest {
    pub hash: RapidHash,
    pub byte_len: usize,
    pub line_count: usize,
}

impl SymbolDigest {
    pub fn new(bytes: &[u8], line_count: usize) -> Self {
        Self {
            hash: RapidHash::new(bytes),
            byte_len: bytes.len(),
            line_count,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Import,
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Impl,
    TypeAlias,
    Const,
    Static,
    Field,
    Interface,
    EnumVariant,
    Module,
    Macro,
    Test,
    Unknown,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Import => "import",
            Self::Function => "function",
            Self::Method => "method",
            Self::Class => "class",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::TypeAlias => "type",
            Self::Const => "const",
            Self::Static => "static",
            Self::Field => "field",
            Self::Interface => "interface",
            Self::EnumVariant => "enum_variant",
            Self::Module => "module",
            Self::Macro => "macro",
            Self::Test => "test",
            Self::Unknown => "unknown",
        }
    }

    /// Returns true if this kind matches the optional filter.
    /// `None` means "any" and always matches.
    pub fn matches_filter(self, filter: Option<SymbolKind>) -> bool {
        filter.is_none_or(|f| f == self)
    }
}

impl std::str::FromStr for SymbolKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "import" | "use" => Ok(Self::Import),
            "function" | "fn" => Ok(Self::Function),
            "method" => Ok(Self::Method),
            "class" => Ok(Self::Class),
            "struct" => Ok(Self::Struct),
            "enum" => Ok(Self::Enum),
            "trait" => Ok(Self::Trait),
            "impl" => Ok(Self::Impl),
            "type" | "type_alias" => Ok(Self::TypeAlias),
            "const" => Ok(Self::Const),
            "static" => Ok(Self::Static),
            "field" => Ok(Self::Field),
            "interface" => Ok(Self::Interface),
            "enum_variant" | "variant" => Ok(Self::EnumVariant),
            "module" | "mod" => Ok(Self::Module),
            "macro" => Ok(Self::Macro),
            "test" => Ok(Self::Test),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!("Unsupported symbol kind: {other}")),
        }
    }
}

/// Parse a kind filter string, returning `None` for "any"/"*".
pub fn parse_kind_filter(value: &str) -> Result<Option<SymbolKind>, String> {
    match value {
        "any" | "*" => Ok(None),
        _ => value.parse().map(Some),
    }
}

#[derive(Debug, Clone)]
pub struct SymbolEntry {
    pub kind: SymbolKind,
    pub name: String,
    pub qualified_name: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub body_start_line: Option<usize>,
    pub body_end_line: Option<usize>,
    pub parent: Option<String>,
    pub children: Vec<SymbolEntry>,
    pub digest: SymbolDigest,
}

impl SymbolEntry {
    pub fn matches_name(&self, needle: &str) -> bool {
        self.name == needle || self.qualified_name == needle
    }
}

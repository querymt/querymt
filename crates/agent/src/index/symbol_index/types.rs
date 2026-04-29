use crate::anchors::symbol_cache::SymbolDigest;

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

    pub fn matches_filter(self, filter: SymbolKindFilter) -> bool {
        filter == SymbolKindFilter::Any || SymbolKindFilter::from(self) == filter
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKindFilter {
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
    Any,
}

impl From<SymbolKind> for SymbolKindFilter {
    fn from(kind: SymbolKind) -> Self {
        match kind {
            SymbolKind::Import => Self::Import,
            SymbolKind::Function => Self::Function,
            SymbolKind::Method => Self::Method,
            SymbolKind::Class => Self::Class,
            SymbolKind::Struct => Self::Struct,
            SymbolKind::Enum => Self::Enum,
            SymbolKind::Trait => Self::Trait,
            SymbolKind::Impl => Self::Impl,
            SymbolKind::TypeAlias => Self::TypeAlias,
            SymbolKind::Const => Self::Const,
            SymbolKind::Static => Self::Static,
            SymbolKind::Field => Self::Field,
            SymbolKind::Interface => Self::Interface,
            SymbolKind::EnumVariant => Self::EnumVariant,
            SymbolKind::Module => Self::Module,
            SymbolKind::Macro => Self::Macro,
            SymbolKind::Test => Self::Test,
            SymbolKind::Unknown => Self::Unknown,
        }
    }
}

impl std::str::FromStr for SymbolKindFilter {
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
            "any" | "*" => Ok(Self::Any),
            other => Err(format!("Unsupported symbol kind filter: {other}")),
        }
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

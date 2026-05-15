use crate::index::outline_index::common::{IndexOptions, Section, SkeletonEntry};
use crate::index::symbol_index::{SymbolEntry, SymbolKind};

pub fn symbols_to_sections(symbols: &[SymbolEntry], options: &IndexOptions) -> Vec<Section> {
    let mut package = Vec::new();
    let mut includes = Vec::new();
    let mut imports = Vec::new();
    let mut usings = Vec::new();
    let mut requires = Vec::new();
    let mut namespaces = Vec::new();
    let mut modules = Vec::new();
    let mut types = Vec::new();
    let mut interfaces = Vec::new();
    let mut enums = Vec::new();
    let mut traits = Vec::new();
    let mut impls = Vec::new();
    let mut classes = Vec::new();
    let mut functions = Vec::new();
    let mut tests = Vec::new();
    let mut macros = Vec::new();
    let mut constants = Vec::new();
    let mut split_interface_enum_sections = false;

    for symbol in symbols {
        if symbol.kind == SymbolKind::Test {
            if options.include_tests {
                tests.push(symbol_to_entry(symbol, options));
            }
            continue;
        }

        match symbol.kind {
            SymbolKind::Import => {
                if symbol.signature.starts_with("package ") {
                    package.push(symbol_to_entry(symbol, options));
                    split_interface_enum_sections = true;
                } else if symbol.signature.starts_with("#include") {
                    includes.push(symbol_to_entry(symbol, options));
                } else if symbol.signature.starts_with("using ") {
                    usings.push(symbol_to_entry(symbol, options));
                    split_interface_enum_sections = true;
                } else if symbol.signature.starts_with("require") {
                    if symbol.parent.is_some() {
                        imports.push(symbol_to_entry(symbol, options));
                    } else {
                        requires.push(symbol_to_entry(symbol, options));
                    }
                } else {
                    imports.push(symbol_to_entry(symbol, options));
                }
            }
            SymbolKind::Struct | SymbolKind::TypeAlias => {
                types.push(symbol_to_entry(symbol, options));
            }
            SymbolKind::Module => {
                if symbol.signature.starts_with("namespace ") {
                    namespaces.push(symbol_to_entry(symbol, options));
                    split_interface_enum_sections = true;
                    collect_namespace_members(
                        symbol,
                        options,
                        &mut classes,
                        &mut interfaces,
                        &mut enums,
                    );
                } else if symbol.signature.starts_with("module ")
                    || symbol.signature.starts_with("defmodule ")
                {
                    modules.push(symbol_to_entry(symbol, options));
                } else {
                    types.push(symbol_to_entry(symbol, options));
                }
            }
            SymbolKind::Interface => interfaces.push(symbol_to_entry(symbol, options)),
            SymbolKind::Enum => enums.push(symbol_to_entry(symbol, options)),
            SymbolKind::Class => classes.push(symbol_to_entry(symbol, options)),
            SymbolKind::Trait => traits.push(symbol_to_entry(symbol, options)),
            SymbolKind::Impl => impls.push(symbol_to_entry(symbol, options)),
            SymbolKind::Function | SymbolKind::Method => {
                functions.push(symbol_to_entry(symbol, options))
            }
            SymbolKind::Macro => macros.push(symbol_to_entry(symbol, options)),
            SymbolKind::Const | SymbolKind::Static => {
                constants.push(symbol_to_entry(symbol, options));
            }
            _ => {}
        }
    }

    let mut sections = Vec::new();
    push_section(&mut sections, "package", package);
    push_section(&mut sections, "includes", includes);
    push_section(&mut sections, "imports", imports);
    push_section(&mut sections, "usings", usings);
    push_section(&mut sections, "requires", requires);
    push_section(&mut sections, "namespaces", namespaces);
    push_section(&mut sections, "modules", modules);
    if split_interface_enum_sections {
        push_section(&mut sections, "types", types);
        push_section(&mut sections, "interfaces", interfaces);
        push_section(&mut sections, "enums", enums);
    } else {
        types.extend(interfaces);
        types.extend(enums);
        push_section(&mut sections, "types", types);
    }
    push_section(&mut sections, "classes", classes);
    push_section(&mut sections, "traits", traits);
    push_section(&mut sections, "impls", impls);
    push_section(&mut sections, "functions", functions);
    push_section(&mut sections, "macros", macros);
    push_section(&mut sections, "constants", constants);
    push_section(&mut sections, "tests", tests);
    sections
}

fn collect_namespace_members(
    symbol: &SymbolEntry,
    options: &IndexOptions,
    classes: &mut Vec<SkeletonEntry>,
    interfaces: &mut Vec<SkeletonEntry>,
    enums: &mut Vec<SkeletonEntry>,
) {
    for child in &symbol.children {
        match child.kind {
            SymbolKind::Class => classes.push(symbol_to_entry(child, options)),
            SymbolKind::Interface => interfaces.push(symbol_to_entry(child, options)),
            SymbolKind::Enum => enums.push(symbol_to_entry(child, options)),
            _ => {}
        }
    }
}

fn push_section(sections: &mut Vec<Section>, name: &str, entries: Vec<SkeletonEntry>) {
    if !entries.is_empty() {
        sections.push(Section::with_entries(name, entries));
    }
}

fn symbol_to_entry(symbol: &SymbolEntry, options: &IndexOptions) -> SkeletonEntry {
    SkeletonEntry::with_children(
        outline_label(symbol),
        symbol.start_line,
        symbol.end_line,
        truncate_children(
            symbol
                .children
                .iter()
                .filter(|child| options.include_tests || child.kind != SymbolKind::Test)
                .map(|child| symbol_to_entry(child, options))
                .collect(),
            options.max_children_per_item,
        ),
    )
}

fn outline_label(symbol: &SymbolEntry) -> String {
    match symbol.kind {
        SymbolKind::EnumVariant | SymbolKind::Field => {
            symbol.signature.trim_end_matches(',').to_string()
        }
        _ => symbol.signature.clone(),
    }
}

fn truncate_children(mut children: Vec<SkeletonEntry>, max: Option<usize>) -> Vec<SkeletonEntry> {
    if let Some(max) = max
        && children.len() > max
    {
        let total = children.len();
        children.truncate(max);
        children.push(SkeletonEntry::new(
            format!("... ({} more)", total - max),
            0,
            0,
        ));
    }
    children
}

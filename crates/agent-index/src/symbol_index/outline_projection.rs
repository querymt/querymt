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
    let mut sections = Vec::new();
    let mut markup = Vec::new();

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
                    || is_nix_module_signature(&symbol.signature)
                {
                    modules.push(symbol_to_entry(symbol, options));
                } else if symbol.signature.starts_with('#') || symbol.signature.starts_with('[') {
                    // Markdown headings, TOML `[table]`/`[[array]]` headers.
                    sections.push(symbol_to_entry(symbol, options));
                } else if symbol.signature.starts_with("{#") || symbol.signature.starts_with('<') {
                    // Svelte template structure (elements, blocks, `<style>`).
                    markup.push(symbol_to_entry(symbol, options));
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

    let mut sections_out = Vec::new();
    push_section(&mut sections_out, "package", package);
    push_section(&mut sections_out, "includes", includes);
    push_section(&mut sections_out, "imports", imports);
    push_section(&mut sections_out, "usings", usings);
    push_section(&mut sections_out, "requires", requires);
    push_section(&mut sections_out, "namespaces", namespaces);
    push_section(&mut sections_out, "modules", modules);
    push_section(&mut sections_out, "sections", sections);
    if split_interface_enum_sections {
        push_section(&mut sections_out, "types", types);
        push_section(&mut sections_out, "interfaces", interfaces);
        push_section(&mut sections_out, "enums", enums);
    } else {
        types.extend(interfaces);
        types.extend(enums);
        push_section(&mut sections_out, "types", types);
    }
    push_section(&mut sections_out, "classes", classes);
    push_section(&mut sections_out, "traits", traits);
    push_section(&mut sections_out, "impls", impls);
    push_section(&mut sections_out, "functions", functions);
    push_section(&mut sections_out, "macros", macros);
    push_section(&mut sections_out, "constants", constants);
    push_section(&mut sections_out, "markup", markup);
    push_section(&mut sections_out, "tests", tests);
    sections_out
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

fn is_nix_module_signature(signature: &str) -> bool {
    signature.contains(" = {") || signature.contains(" = rec {") || signature.contains(" = let {")
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

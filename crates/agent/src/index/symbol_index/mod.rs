mod extractors;
pub mod outline_projection;
mod types;

use std::path::Path;

use crate::index::outline_index::common::get_language_for_extension;

pub use types::{SymbolDigest, SymbolEntry, SymbolKind, parse_kind_filter};

#[derive(Debug, thiserror::Error)]
pub enum SymbolError {
    #[error("I/O error: {0}")]
    Io(String),

    #[error("Unsupported file extension: .{0}")]
    UnsupportedExtension(String),

    #[error("Unsupported language: {0}")]
    UnsupportedLanguage(String),

    #[error("Parse error: {0}")]
    ParseError(String),
}

#[derive(Debug, Clone)]
pub struct SymbolIndex {
    pub symbols: Vec<SymbolEntry>,
}

impl SymbolIndex {
    pub fn from_file(path: &Path) -> Result<Self, SymbolError> {
        let source = std::fs::read_to_string(path).map_err(|e| SymbolError::Io(e.to_string()))?;
        Self::from_source_for_path(path, &source)
    }

    pub fn from_source_for_path(path: &Path, source: &str) -> Result<Self, SymbolError> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let language = get_language_for_extension(ext)
            .ok_or_else(|| SymbolError::UnsupportedExtension(ext.to_string()))?;
        Self::from_source(source, language)
    }

    pub fn from_source(source: &str, language: &str) -> Result<Self, SymbolError> {
        Ok(Self {
            symbols: extractors::extract_symbols(source, language)?,
        })
    }

    pub fn find_by_name(&self, name: &str, kind: Option<SymbolKind>) -> Vec<&SymbolEntry> {
        let mut matches = Vec::new();
        for symbol in &self.symbols {
            collect_name_matches(symbol, name, kind, &mut matches);
        }
        matches
    }

    pub fn find_by_range(&self, line: usize) -> Vec<&SymbolEntry> {
        let mut matches = Vec::new();
        for symbol in &self.symbols {
            collect_range_matches(symbol, line, &mut matches);
        }
        matches
    }

    /// Find the parent symbol that contains the given child at the specified line range.
    /// Returns the parent entry if found.
    pub fn find_parent_of(&self, child: &SymbolEntry) -> Option<&SymbolEntry> {
        for symbol in &self.symbols {
            if let Some(parent) = find_parent_recursive(symbol, child) {
                return Some(parent);
            }
        }
        None
    }

    /// Collect all import symbols in this index.
    pub fn imports(&self) -> Vec<&SymbolEntry> {
        let mut result = Vec::new();
        for symbol in &self.symbols {
            collect_by_kind(symbol, SymbolKind::Import, &mut result);
        }
        result
    }
}

fn collect_name_matches<'a>(
    symbol: &'a SymbolEntry,
    name: &str,
    kind: Option<SymbolKind>,
    matches: &mut Vec<&'a SymbolEntry>,
) {
    if symbol.kind.matches_filter(kind) && symbol.matches_name(name) {
        matches.push(symbol);
    }
    for child in &symbol.children {
        collect_name_matches(child, name, kind, matches);
    }
}

fn collect_range_matches<'a>(
    symbol: &'a SymbolEntry,
    line: usize,
    matches: &mut Vec<&'a SymbolEntry>,
) {
    if symbol.start_line <= line && line <= symbol.end_line {
        matches.push(symbol);
    }
    for child in &symbol.children {
        collect_range_matches(child, line, matches);
    }
}

fn find_parent_recursive<'a>(
    candidate: &'a SymbolEntry,
    child: &SymbolEntry,
) -> Option<&'a SymbolEntry> {
    for c in &candidate.children {
        if c.start_line == child.start_line
            && c.end_line == child.end_line
            && c.qualified_name == child.qualified_name
        {
            return Some(candidate);
        }
        if let Some(found) = find_parent_recursive(c, child) {
            return Some(found);
        }
    }
    None
}

fn collect_by_kind<'a>(
    symbol: &'a SymbolEntry,
    kind: SymbolKind,
    result: &mut Vec<&'a SymbolEntry>,
) {
    if symbol.kind == kind {
        result.push(symbol);
    }
    for child in &symbol.children {
        collect_by_kind(child, kind, result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_source() -> &'static str {
        r#"pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

pub fn run() {}
"#
    }

    #[test]
    fn rust_symbols_include_top_level_and_nested_methods() {
        let index = SymbolIndex::from_source(rust_source(), "rust").unwrap();

        let config = index.find_by_name("Config", Some(SymbolKind::Struct));
        assert_eq!(config.len(), 1);
        assert_eq!(config[0].kind, SymbolKind::Struct);
        assert_eq!(config[0].children[0].qualified_name, "Config::name");

        let imports = index.find_by_name("use std::fmt", Some(SymbolKind::Import));
        assert!(imports.is_empty());

        let method = index.find_by_name("Config::new", Some(SymbolKind::Method));
        assert_eq!(method.len(), 1);
        assert_eq!(method[0].name, "new");
        assert_eq!(method[0].parent.as_deref(), Some("Config"));

        let run = index.find_by_name("run", Some(SymbolKind::Function));
        assert_eq!(run.len(), 1);
        assert_eq!(run[0].signature, "pub fn run()");
    }

    #[test]
    fn rust_symbols_have_ranges_and_digests() {
        let index = SymbolIndex::from_source(rust_source(), "rust").unwrap();
        let run = index.find_by_name("run", Some(SymbolKind::Function))[0];

        assert!(run.start_line <= run.end_line);
        assert!(run.start_byte < run.end_byte);
        assert!(run.digest.byte_len > 0);
        assert_eq!(run.digest.line_count, 1);

        let containing = index.find_by_range(run.start_line);
        assert!(
            containing
                .iter()
                .any(|symbol| symbol.qualified_name == "run")
        );
    }

    #[test]
    fn typescript_symbols_include_classes_interfaces_and_functions() {
        let source = r#"import axios from 'axios';

interface Config {
    name: string;
    validate(): boolean;
}

export class AppService {
    async fetchData(): Promise<void> {
        return;
    }
}

export function run(args: string[]): void {
    console.log(args);
}

const DEFAULT_TIMEOUT = 5000;
"#;
        let index = SymbolIndex::from_source(source, "typescript").unwrap();

        let config = index.find_by_name("Config", Some(SymbolKind::Interface));
        assert_eq!(config.len(), 1);
        assert!(
            config[0]
                .children
                .iter()
                .any(|child| child.name == "validate")
        );

        let class = index.find_by_name("AppService", Some(SymbolKind::Class));
        assert_eq!(class.len(), 1);
        assert!(
            class[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "AppService::fetchData")
        );

        let run = index.find_by_name("run", Some(SymbolKind::Function));
        assert_eq!(run.len(), 1);
        assert!(run[0].signature.contains("export function run"));

        let constant = index.find_by_name("DEFAULT_TIMEOUT", Some(SymbolKind::Const));
        assert_eq!(constant.len(), 1);
    }

    #[test]
    fn python_symbols_include_classes_methods_functions_and_tests() {
        let source = r#"import os
from pathlib import Path

class Config:
    def __init__(self, name: str):
        self.name = name

    def validate(self) -> bool:
        return True

def main():
    config = Config("test")

def test_something():
    assert True

DEFAULT_TIMEOUT = 5000
"#;
        let index = SymbolIndex::from_source(source, "python").unwrap();

        let config = index.find_by_name("Config", Some(SymbolKind::Class));
        assert_eq!(config.len(), 1);
        assert!(
            config[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "Config::validate")
        );

        let main = index.find_by_name("main", Some(SymbolKind::Function));
        assert_eq!(main.len(), 1);
        assert!(main[0].signature.contains("def main"));

        let test = index.find_by_name("test_something", Some(SymbolKind::Test));
        assert_eq!(test.len(), 1);

        let constant = index.find_by_name("DEFAULT_TIMEOUT", Some(SymbolKind::Const));
        assert_eq!(constant.len(), 1);
    }

    #[test]
    fn java_symbols_include_package_imports_classes_interfaces_and_enums() {
        let source = r#"package com.example;

import java.util.List;

public class Config {
    private String name;

    public Config(String name) {
        this.name = name;
    }

    public String getName() {
        return name;
    }
}

interface Validator {
    boolean validate();
}

enum Mode {
    FAST,
    SAFE
}
"#;
        let index = SymbolIndex::from_source(source, "java").unwrap();

        let package = index.find_by_name("package com.example", Some(SymbolKind::Import));
        assert_eq!(package.len(), 1);

        let import = index.find_by_name("import java.util.List", Some(SymbolKind::Import));
        assert_eq!(import.len(), 1);

        let class = index.find_by_name("Config", Some(SymbolKind::Class));
        assert_eq!(class.len(), 1);
        assert!(
            class[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "Config::getName")
        );
        assert!(
            class[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "Config::name")
        );

        let interface = index.find_by_name("Validator", Some(SymbolKind::Interface));
        assert_eq!(interface.len(), 1);
        assert!(
            interface[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "Validator::validate")
        );

        let mode = index.find_by_name("Mode", Some(SymbolKind::Enum));
        assert_eq!(mode.len(), 1);
        assert!(
            mode[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "Mode::FAST")
        );
    }

    #[test]
    fn go_symbols_include_imports_types_functions_and_tests() {
        let source = r#"package main

import (
    \"fmt\"
    \"os\"
)

type Config struct {
    Name string
}

type Runner interface {
    Run() error
}

func NewConfig(name string) *Config {
    return &Config{Name: name}
}

func TestConfig(t *testing.T) {}
"#;

        let index = SymbolIndex::from_source(source, "go").unwrap();

        assert!(
            index
                .symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Import && symbol.signature.contains("fmt"))
        );

        let config = index.find_by_name("Config", Some(SymbolKind::Struct));
        assert_eq!(config.len(), 1);
        assert!(
            config[0]
                .children
                .iter()
                .any(|child| child.qualified_name.contains("Config::"))
        );

        let runner = index.find_by_name("Runner", Some(SymbolKind::Interface));
        assert_eq!(runner.len(), 1);

        let new_config = index.find_by_name("NewConfig", Some(SymbolKind::Function));
        assert_eq!(new_config.len(), 1);

        let test = index.find_by_name("TestConfig", Some(SymbolKind::Test));
        assert_eq!(test.len(), 1);
    }

    #[test]
    fn c_family_symbols_include_types_functions_and_constants() {
        let c_source = r#"#include <stdio.h>
#define MAX_SIZE 1024

struct Config {
    int retries;
};

enum Status {
    ACTIVE,
    INACTIVE
};

void run(void) {}
"#;
        let c_index = SymbolIndex::from_source(c_source, "c").unwrap();

        let include = c_index.find_by_name("#include <stdio.h>", Some(SymbolKind::Import));
        assert_eq!(include.len(), 1);

        let config = c_index.find_by_name("Config", Some(SymbolKind::Struct));
        assert_eq!(config.len(), 1);

        let status = c_index.find_by_name("Status", Some(SymbolKind::Enum));
        assert_eq!(status.len(), 1);

        assert!(
            c_index
                .symbols
                .iter()
                .any(|symbol| symbol.kind == SymbolKind::Function
                    && symbol.signature.contains("run"))
        );

        let cpp_source = r#"class Box {
public:
    int size;
    void grow() {}
};
"#;
        let cpp_index = SymbolIndex::from_source(cpp_source, "cpp").unwrap();

        let class_box = cpp_index.find_by_name("Box", Some(SymbolKind::Class));
        assert_eq!(class_box.len(), 1);
        assert!(!class_box[0].children.is_empty());
    }

    #[test]
    fn csharp_symbols_include_namespaces_types_and_members() {
        let source = r#"using System;

namespace MyApp {
    public class Config {
        public string Name { get; set; }
        public bool Validate() { return true; }
    }

    public interface IValidator {
        bool Validate();
    }
}
"#;
        let index = SymbolIndex::from_source(source, "csharp").unwrap();

        let using_directive = index.find_by_name("using System", Some(SymbolKind::Import));
        assert_eq!(using_directive.len(), 1);

        let ns = index.find_by_name("MyApp", Some(SymbolKind::Module));
        assert_eq!(ns.len(), 1);

        let class_config = index.find_by_name("Config", Some(SymbolKind::Class));
        assert_eq!(class_config.len(), 1);

        let interface = index.find_by_name("IValidator", Some(SymbolKind::Interface));
        assert_eq!(interface.len(), 1);
    }

    #[test]
    fn ruby_symbols_include_requires_classes_functions_and_tests() {
        let source = r#"require 'json'
require_relative 'helper'

class Config
  def validate
    true
  end
end

def run(args)
  puts args
end

def test_happy_path
  true
end
"#;
        let index = SymbolIndex::from_source(source, "ruby").unwrap();

        let requires = index.find_by_name("require 'json'", Some(SymbolKind::Import));
        assert_eq!(requires.len(), 1);

        let class_config = index.find_by_name("Config", Some(SymbolKind::Class));
        assert_eq!(class_config.len(), 1);

        let run = index.find_by_name("run", Some(SymbolKind::Function));
        assert_eq!(run.len(), 1);

        let test = index.find_by_name("test_happy_path", Some(SymbolKind::Test));
        assert_eq!(test.len(), 1);
    }
}

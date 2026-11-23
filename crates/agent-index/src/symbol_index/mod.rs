mod extractors;
pub mod outline_projection;
mod types;

use std::path::Path;

use crate::outline_index::common::get_language_for_extension;

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

    #[test]
    fn nix_symbols_include_imports_modules_functions_consts_and_nested_attrs() {
        let source = r#"{ lib, stdenv, pkgs, system }:
{
  imports = [ ./hardware.nix <nixpkgs/nixos/modules> ];
  version = "1.0";
  system = "x86_64-linux";
  mkPackage = { pname, ... }: stdenv.mkDerivation { inherit pname; };
  overlay = final: prev: { };
  packages.${system}.default = { };
  devShells = {
    default = pkgs.mkShell { };
    nested = x: x;
  };
  nixosModules.default = { config, ... }: { };
  fromImport = import ./foo.nix;
  fromBuiltins = builtins.import <bar>;
}
"#;
        let index = SymbolIndex::from_source(source, "nix").unwrap();

        assert_eq!(
            index
                .find_by_name("./foo.nix", Some(SymbolKind::Import))
                .len(),
            1
        );
        assert_eq!(
            index.find_by_name("<bar>", Some(SymbolKind::Import)).len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("./hardware.nix", Some(SymbolKind::Import))
                .len(),
            1
        );

        let mk_package = index.find_by_name("mkPackage", Some(SymbolKind::Function));
        assert_eq!(mk_package.len(), 1);
        assert_eq!(mk_package[0].start_line, 6);
        assert!(mk_package[0].signature.contains("mkPackage = { pname"));
        assert!(mk_package[0].digest.byte_len > 0);

        let overlay = index.find_by_name("overlay", Some(SymbolKind::Function));
        assert_eq!(overlay.len(), 1);

        let dev_shells = index.find_by_name("devShells", Some(SymbolKind::Module));
        assert_eq!(dev_shells.len(), 1);
        assert!(
            dev_shells[0]
                .children
                .iter()
                .any(|child| child.name == "default" && child.kind == SymbolKind::Field)
        );
        assert!(
            dev_shells[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "devShells::nested"
                    && child.kind == SymbolKind::Method)
        );

        let dynamic = index.find_by_name("packages.$system.default", Some(SymbolKind::Module));
        assert_eq!(dynamic.len(), 1);

        let nixos_module = index.find_by_name("nixosModules.default", Some(SymbolKind::Module));
        assert_eq!(nixos_module.len(), 1);

        let version = index.find_by_name("version", Some(SymbolKind::Const));
        assert_eq!(version.len(), 1);
        assert_eq!(version[0].start_line, 4);
    }

    #[test]
    fn nix_function_module_children_include_let_and_returned_attrset_bindings() {
        let source = r#"{ pkgs, ... }:
{
  modules.example = { config, lib, ... }:
    let
      localValue = 1;
      makeName = name: "prefix-${name}";
    in {
      options.example.enable = lib.mkEnableOption "example";
      config = lib.mkIf config.example.enable {
        environment.systemPackages = [ pkgs.hello ];
      };
    };

  packages.${system}.default = let
    pname = "demo";
  in pkgs.stdenv.mkDerivation {
    inherit pname;
    version = "1.0";
  };
}
"#;
        let index = SymbolIndex::from_source(source, "nix").unwrap();

        let module = index.find_by_name("modules.example", Some(SymbolKind::Module));
        assert_eq!(module.len(), 1);
        assert!(
            module[0]
                .children
                .iter()
                .any(|child| child.name == "localValue" && child.kind == SymbolKind::Field)
        );
        assert!(
            module[0]
                .children
                .iter()
                .any(|child| child.name == "makeName" && child.kind == SymbolKind::Method)
        );
        assert!(module[0].children.iter().any(|child| {
            child.name == "options.example.enable" && child.kind == SymbolKind::Field
        }));
        assert!(
            module[0]
                .children
                .iter()
                .any(|child| child.name == "config" && child.kind == SymbolKind::Field)
        );

        let package = index.find_by_name("packages.$system.default", Some(SymbolKind::Module));
        assert_eq!(package.len(), 1);
        assert!(
            package[0]
                .children
                .iter()
                .any(|child| child.name == "pname" && child.kind == SymbolKind::Field)
        );
    }

    #[test]
    fn lua_symbols_include_imports_modules_functions_methods_consts_and_tests() {
        let source = r#"local json = require("json")
require "foo"
require("bar")

local M = {}
local DEFAULT_TIMEOUT = 30
DEFAULT_LIMIT = 5

function run(a, b)
  return a
end

local function helper(x)
  return x
end

function M.run(x)
  return x
end

function M:call(x)
  return x
end

M.configure = function(opts)
  return opts
end

local local_helper = function(value)
  return value
end

describe("feature", function()
  it("works", function()
  end)
  pending("later", function()
  end)
end)

function test_unit()
end

function integration_test()
end
"#;
        let index = SymbolIndex::from_source(source, "lua").unwrap();

        assert_eq!(
            index.find_by_name("json", Some(SymbolKind::Import)).len(),
            1
        );
        assert_eq!(index.find_by_name("foo", Some(SymbolKind::Import)).len(), 1);
        assert_eq!(index.find_by_name("bar", Some(SymbolKind::Import)).len(), 1);

        let module = index.find_by_name("M", Some(SymbolKind::Module));
        assert_eq!(module.len(), 1);
        assert_eq!(module[0].start_line, 5);

        assert_eq!(
            index.find_by_name("run", Some(SymbolKind::Function)).len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("helper", Some(SymbolKind::Function))
                .len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("local_helper", Some(SymbolKind::Function))
                .len(),
            1
        );
        assert_eq!(
            index.find_by_name("M.run", Some(SymbolKind::Method)).len(),
            1
        );
        assert_eq!(
            index.find_by_name("M:call", Some(SymbolKind::Method)).len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("M.configure", Some(SymbolKind::Method))
                .len(),
            1
        );

        assert_eq!(
            index
                .find_by_name("DEFAULT_TIMEOUT", Some(SymbolKind::Const))
                .len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("DEFAULT_LIMIT", Some(SymbolKind::Const))
                .len(),
            1
        );

        let describe = index.find_by_name("feature", Some(SymbolKind::Test));
        assert_eq!(describe.len(), 1);
        assert!(
            describe[0]
                .children
                .iter()
                .any(|child| child.name == "works" && child.kind == SymbolKind::Test)
        );
        assert!(
            describe[0]
                .children
                .iter()
                .any(|child| child.name == "later" && child.kind == SymbolKind::Test)
        );
        assert_eq!(
            index
                .find_by_name("test_unit", Some(SymbolKind::Test))
                .len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("integration_test", Some(SymbolKind::Test))
                .len(),
            1
        );
    }

    #[test]
    fn elixir_symbols_include_modules_functions_macros_imports_and_tests() {
        let source = r#"defmodule MyApp.ConfigTest do
  use ExUnit.Case
  alias MyApp.Repo
  import Ecto.Query
  require Logger

  def start_link(opts) do
    GenServer.start_link(__MODULE__, opts)
  end

  defp normalize(opts), do: opts

  defmacro field(name) do
    quote do: unquote(name)
  end

  describe "validate/1" do
    test "accepts valid opts" do
      assert :ok
    end
  end
end

def run(args), do: args

defprotocol MyApp.Renderable do
  def render(value)
end

defimpl MyApp.Renderable, for: Atom do
  def render(value), do: Atom.to_string(value)
end
"#;
        let index = SymbolIndex::from_source(source, "elixir").unwrap();

        let module = index.find_by_name("MyApp.ConfigTest", Some(SymbolKind::Module));
        assert_eq!(module.len(), 1);
        assert_eq!(module[0].start_line, 1);
        assert!(module[0].end_line > module[0].start_line);
        assert!(module[0].digest.byte_len > 0);
        assert!(
            module[0]
                .children
                .iter()
                .any(|child| child.name == "start_link" && child.kind == SymbolKind::Method)
        );
        assert!(
            module[0]
                .children
                .iter()
                .any(|child| child.name == "field" && child.kind == SymbolKind::Macro)
        );

        let use_exunit = index.find_by_name("ExUnit.Case", Some(SymbolKind::Import));
        assert_eq!(use_exunit.len(), 1);

        let run = index.find_by_name("run", Some(SymbolKind::Function));
        assert_eq!(run.len(), 1);

        let describe = index.find_by_name("validate/1", Some(SymbolKind::Test));
        assert_eq!(describe.len(), 1);
        assert!(
            describe[0]
                .children
                .iter()
                .any(|child| child.name == "accepts valid opts" && child.kind == SymbolKind::Test)
        );

        let nested_test = index.find_by_name(
            "MyApp.ConfigTest::validate/1::accepts valid opts",
            Some(SymbolKind::Test),
        );
        assert_eq!(nested_test.len(), 1);

        let protocol = index.find_by_name("MyApp.Renderable", Some(SymbolKind::Trait));
        assert_eq!(protocol.len(), 1);

        let impls = index.find_by_name("MyApp.Renderable, for: Atom", Some(SymbolKind::Impl));
        assert_eq!(impls.len(), 1);
    }

    #[test]
    fn bash_symbols_include_imports_consts_functions_and_tests() {
        let source = r#"#!/usr/bin/env bash
set -euo pipefail

source ./lib.sh
. ./other.sh

MAX_RETRIES=5
name="hello"

run_build() {
  echo "building"
}

function deploy {
  echo "deploying"
}

test_pipeline() {
  run_build
}
"#;
        let index = SymbolIndex::from_source(source, "bash").unwrap();

        assert_eq!(
            index
                .find_by_name("./lib.sh", Some(SymbolKind::Import))
                .len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("./other.sh", Some(SymbolKind::Import))
                .len(),
            1
        );

        let max_retries = index.find_by_name("MAX_RETRIES", Some(SymbolKind::Const));
        assert_eq!(max_retries.len(), 1);
        assert_eq!(max_retries[0].start_line, 7);

        let run_build = index.find_by_name("run_build", Some(SymbolKind::Function));
        assert_eq!(run_build.len(), 1);
        assert_eq!(run_build[0].start_line, 10);
        assert_eq!(run_build[0].end_line, 12);

        assert_eq!(
            index
                .find_by_name("deploy", Some(SymbolKind::Function))
                .len(),
            1
        );

        let test = index.find_by_name("test_pipeline", Some(SymbolKind::Test));
        assert_eq!(test.len(), 1);
        assert_eq!(test[0].start_line, 18);
        assert_eq!(test[0].end_line, 20);
    }

    #[test]
    fn php_symbols_include_namespaces_classes_functions_imports_and_consts() {
        let source = r#"<?php
namespace App\Services;

use App\Models\User;

const MAX_ITEMS = 10;

class UserService {
    public function find(int $id): ?User {
        return null;
    }
}

interface Repository {
    public function find(int $id);
}

trait Loggable {
    public function log(string $msg): void {}
}

enum Status: string {
    case Active = 'active';
}

function helper(string $name): string {
    return $name;
}
"#;
        let index = SymbolIndex::from_source(source, "php").unwrap();

        let namespace = index.find_by_name("App\\Services", Some(SymbolKind::Module));
        assert_eq!(namespace.len(), 1);
        assert_eq!(namespace[0].start_line, 2);

        let import = index.find_by_name("App\\Models\\User", Some(SymbolKind::Import));
        let import = if import.is_empty() {
            index.find_by_name("use App\\Models\\User;", Some(SymbolKind::Import))
        } else {
            import
        };
        assert_eq!(import.len(), 1);
        assert_eq!(import[0].start_line, 4);

        let max_items = index.find_by_name("MAX_ITEMS", Some(SymbolKind::Const));
        assert_eq!(max_items.len(), 1);
        assert_eq!(max_items[0].start_line, 6);

        let class = index.find_by_name("UserService", Some(SymbolKind::Class));
        assert_eq!(class.len(), 1);
        assert_eq!(class[0].start_line, 8);
        assert!(
            class[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "UserService::find"
                    && child.kind == SymbolKind::Method)
        );

        assert_eq!(
            index
                .find_by_name("Repository", Some(SymbolKind::Interface))
                .len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("Loggable", Some(SymbolKind::Trait))
                .len(),
            1
        );

        let status = index.find_by_name("Status", Some(SymbolKind::Enum));
        assert_eq!(status.len(), 1);
        assert!(
            status[0]
                .children
                .iter()
                .any(|child| child.name == "Active" && child.kind == SymbolKind::EnumVariant)
        );

        let helper = index.find_by_name("helper", Some(SymbolKind::Function));
        assert_eq!(helper.len(), 1);
        assert_eq!(helper[0].start_line, 26);
    }

    #[test]
    fn kotlin_symbols_include_imports_consts_classes_interfaces_functions_and_typealiases() {
        let source = r#"package com.example

import kotlin.math.PI

const val MAX = 10
val globalName: String = "x"
var mutableCount: Int = 1

typealias UserId = String

class UserService(val repo: Repo) {
    fun find(id: UserId): String? = null
}

object Singleton {
    val answer = 42
}

interface Repository {
    fun find(id: String): String?
}

enum class Status { ACTIVE, INACTIVE }

fun helper(name: String): String {
    return name
}
"#;
        let index = SymbolIndex::from_source(source, "kotlin").unwrap();

        let import = index.find_by_name("import kotlin.math.PI", Some(SymbolKind::Import));
        assert_eq!(import.len(), 1);
        assert_eq!(import[0].start_line, 3);

        let max = index.find_by_name("MAX", Some(SymbolKind::Const));
        assert_eq!(max.len(), 1);
        assert_eq!(max[0].start_line, 5);
        assert_eq!(
            index
                .find_by_name("globalName", Some(SymbolKind::Const))
                .len(),
            1
        );
        assert_eq!(
            index
                .find_by_name("mutableCount", Some(SymbolKind::Const))
                .len(),
            1
        );

        let type_alias = index.find_by_name("UserId", Some(SymbolKind::TypeAlias));
        assert_eq!(type_alias.len(), 1);
        assert_eq!(type_alias[0].start_line, 9);

        let class = index.find_by_name("UserService", Some(SymbolKind::Class));
        assert_eq!(class.len(), 1);
        assert_eq!(class[0].start_line, 11);
        assert!(
            class[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "UserService::find"
                    && child.kind == SymbolKind::Method)
        );

        let singleton = index.find_by_name("Singleton", Some(SymbolKind::Class));
        assert_eq!(singleton.len(), 1);
        assert_eq!(singleton[0].signature, "object Singleton {");

        let interface = index.find_by_name("Repository", Some(SymbolKind::Interface));
        assert_eq!(interface.len(), 1);
        assert_eq!(interface[0].start_line, 19);

        let status = index.find_by_name("Status", Some(SymbolKind::Enum));
        assert_eq!(status.len(), 1);
        assert!(
            status[0]
                .children
                .iter()
                .any(|child| child.name == "ACTIVE" && child.kind == SymbolKind::EnumVariant)
        );

        let helper = index.find_by_name("helper", Some(SymbolKind::Function));
        assert_eq!(helper.len(), 1);
        assert_eq!(helper[0].start_line, 25);
    }

    #[test]
    fn swift_symbols_include_imports_consts_types_protocols_extensions_and_functions() {
        let source = r#"import Foundation

let maxRetries = 5
var counter = 0

public class UserService {
    var name: String = ""
    func find(_ id: Int) -> String? { return nil }
}

struct Config {
    var retries: Int
}

enum Status {
    case active, inactive
}

protocol Repository {
    func find(_ id: Int) -> String?
}

extension UserService: Repository {
    func find(_ id: Int) -> String? { return nil }
}

typealias Handler = (Int) -> Void

func helper(name: String) -> String {
    return name
}
"#;
        let index = SymbolIndex::from_source(source, "swift").unwrap();

        let import = index.find_by_name("import Foundation", Some(SymbolKind::Import));
        assert_eq!(import.len(), 1);
        assert_eq!(import[0].start_line, 1);

        let max_retries = index.find_by_name("maxRetries", Some(SymbolKind::Const));
        assert_eq!(max_retries.len(), 1);
        assert_eq!(max_retries[0].start_line, 3);
        assert_eq!(
            index.find_by_name("counter", Some(SymbolKind::Const)).len(),
            1
        );

        let class = index.find_by_name("UserService", Some(SymbolKind::Class));
        assert_eq!(class.len(), 1);
        assert_eq!(class[0].start_line, 6);
        assert!(
            class[0]
                .children
                .iter()
                .any(|child| child.qualified_name == "UserService::find"
                    && child.kind == SymbolKind::Method)
        );

        let config = index.find_by_name("Config", Some(SymbolKind::Struct));
        assert_eq!(config.len(), 1);
        assert!(
            config[0]
                .children
                .iter()
                .any(|child| child.name == "retries" && child.kind == SymbolKind::Field)
        );

        let status = index.find_by_name("Status", Some(SymbolKind::Enum));
        assert_eq!(status.len(), 1);
        assert!(
            status[0]
                .children
                .iter()
                .any(|child| child.kind == SymbolKind::EnumVariant)
        );

        let protocol = index.find_by_name("Repository", Some(SymbolKind::Trait));
        assert_eq!(protocol.len(), 1);
        assert_eq!(protocol[0].start_line, 19);

        let extension = index.find_by_name("UserService", Some(SymbolKind::Impl));
        assert_eq!(extension.len(), 1);
        assert_eq!(extension[0].start_line, 23);

        let type_alias = index.find_by_name("Handler", Some(SymbolKind::TypeAlias));
        assert_eq!(type_alias.len(), 1);

        let helper = index.find_by_name("helper", Some(SymbolKind::Function));
        assert_eq!(helper.len(), 1);
        assert_eq!(helper[0].start_line, 29);
    }

    #[test]
    fn svelte_symbols_include_script_imports_functions_consts_and_markup() {
        let source = r#"<script lang="ts">
  import Widget from './Widget.svelte';
  let count = 0;
  function increment(): void {
    count += 1;
  }
</script>

<script context="module">
  export const VERSION = 1;
  function boot(): void {}
</script>

<Widget prop={count} on:click={increment} />

{#if count > 0}
  <p>positive</p>
{:else}
  <p>zero</p>
{/if}

{#each items as item}
  <span>{item}</span>
{/each}

{#snippet row(item)}
  <li>{item}</li>
{/snippet}

<style>
  p { color: red; }
</style>
"#;
        let index = SymbolIndex::from_source(source, "svelte").unwrap();

        // Imports are named by their full statement text (shared TS extractor).
        let imports: Vec<&SymbolEntry> = index
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);
        assert!(imports[0].name.contains("Widget"));
        assert_eq!(imports[0].start_line, 2);

        let count = index.find_by_name("count", Some(SymbolKind::Const));
        assert_eq!(count.len(), 1);
        assert_eq!(count[0].start_line, 3);

        let increment = index.find_by_name("increment", Some(SymbolKind::Function));
        assert_eq!(increment.len(), 1);
        assert_eq!(increment[0].start_line, 4);

        // Module script symbols share the same section mapping.
        let version = index.find_by_name("VERSION", Some(SymbolKind::Const));
        assert_eq!(version.len(), 1);
        assert_eq!(version[0].start_line, 10);
        let boot = index.find_by_name("boot", Some(SymbolKind::Function));
        assert_eq!(boot.len(), 1);
        assert_eq!(boot[0].start_line, 11);

        // `{#snippet}` declarations are functions.
        let snippet = index.find_by_name("row", Some(SymbolKind::Function));
        assert_eq!(snippet.len(), 1);
        assert_eq!(snippet[0].signature, "{#snippet row(item)}");
        assert_eq!(snippet[0].start_line, 26);

        // Top-level structure is reported as markup-prefixed Module entries.
        let markup: Vec<&SymbolEntry> = index
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module && s.signature.starts_with('<'))
            .collect();
        assert!(
            markup
                .iter()
                .any(|s| s.signature == "<Widget prop on:click>")
        );
        assert!(markup.iter().any(|s| s.signature == "<style>"));

        let blocks: Vec<&SymbolEntry> = index
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Module && s.signature.starts_with("{#"))
            .collect();
        assert!(blocks.iter().any(|s| s.signature == "{#if count > 0}"));
        assert!(
            blocks
                .iter()
                .any(|s| s.signature.starts_with("{#each items as item}"))
        );
    }

    #[test]
    fn svelte_script_symbol_coordinates_are_absolute() {
        let source = r#"<script>
  let pad = 1;

  function resize(delta: number): void {
    pad += delta;
  }
</script>

<p>{pad}</p>
"#;
        let index = SymbolIndex::from_source(source, "svelte").unwrap();
        let resize = index.find_by_name("resize", Some(SymbolKind::Function));
        assert_eq!(resize.len(), 1);
        // Declared on file line 4 (inside the script on line 1).
        assert_eq!(resize[0].start_line, 4);
        assert_eq!(resize[0].end_line, 6);

        // Byte offsets must be valid whole-file char boundaries (UTF-8 safe),
        // and slicing the whole file by the symbol's byte range must yield the
        // declaration.
        assert!(source.is_char_boundary(resize[0].start_byte));
        assert!(source.is_char_boundary(resize[0].end_byte));
        let sliced =
            super::extractors::safe_slice(source, resize[0].start_byte, resize[0].end_byte);
        assert!(sliced.starts_with("function resize"));
        assert!(sliced.contains("pad += delta"));
    }

    #[test]
    fn json_symbols_include_top_level_keys_with_nested_children() {
        let source = r#"{
    "name": "demo",
    "retries": 3,
    "server": {
        "host": "localhost",
        "port": 8080
    },
    "tags": ["a", "b"]
}
"#;
        let index = SymbolIndex::from_source(source, "json").unwrap();

        let name = index.find_by_name("name", Some(SymbolKind::Const));
        assert_eq!(name.len(), 1);
        assert_eq!(name[0].signature, "name: \"demo\"");

        let server = index.find_by_name("server", Some(SymbolKind::Const));
        assert_eq!(server.len(), 1);
        assert!(
            server[0]
                .children
                .iter()
                .any(|child| child.name == "host" && child.kind == SymbolKind::Const)
        );
        assert!(server[0].children.iter().any(|child| child.name == "port"));

        let tags = index.find_by_name("tags", Some(SymbolKind::Const));
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].signature, "tags: [2 items]");
    }

    #[test]
    fn yaml_symbols_include_top_level_keys_nested_mappings_and_sequences() {
        let source = "name: demo\nretries: 3\nserver:\n  host: localhost\n  port: 8080\ntags:\n  - a\n  - b\nnested:\n  deep:\n    key: value\n";
        let index = SymbolIndex::from_source(source, "yaml").unwrap();

        let name = index.find_by_name("name", Some(SymbolKind::Const));
        assert_eq!(name.len(), 1);
        assert_eq!(name[0].signature, "name: demo");
        assert_eq!(name[0].start_line, 1);

        let server = index.find_by_name("server", Some(SymbolKind::Const));
        assert_eq!(server.len(), 1);
        assert!(server[0].children.iter().any(|child| child.name == "host"));
        assert!(server[0].children.iter().any(|child| child.name == "port"));

        let tags = index.find_by_name("tags", Some(SymbolKind::Const));
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].signature, "tags: [2 items]");

        let nested = index.find_by_name("nested", Some(SymbolKind::Const));
        assert_eq!(nested.len(), 1);
        let deep = &nested[0].children[0];
        assert_eq!(deep.name, "deep");
        assert!(deep.children.iter().any(|child| child.name == "key"));
    }

    #[test]
    fn toml_symbols_include_root_pairs_tables_and_arrays() {
        let source = "name = \"demo\"\nretries = 3\n\n[server]\nhost = \"localhost\"\nport = 8080\n\n[[servers]]\nhost = \"a\"\n\n[servers.env]\nkey = \"v\"\n";
        let index = SymbolIndex::from_source(source, "toml").unwrap();

        let name = index.find_by_name("name", Some(SymbolKind::Const));
        assert_eq!(name.len(), 1);
        assert_eq!(name[0].signature, "name = \"demo\"");

        // The key is the symbol name; the `[table]` header is the signature.
        let server = index.find_by_name("server", Some(SymbolKind::Module));
        assert_eq!(server.len(), 1);
        assert_eq!(server[0].signature, "[server]");
        assert_eq!(server[0].start_line, 4);
        assert!(server[0].children.iter().any(|child| child.name == "host"));
        assert!(server[0].children.iter().any(|child| child.name == "port"));

        let servers = index.find_by_name("servers", Some(SymbolKind::Module));
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].signature, "[[servers]]");
        assert!(servers[0].children.iter().any(|child| child.name == "host"));

        let env = index.find_by_name("servers.env", Some(SymbolKind::Module));
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].signature, "[servers.env]");
        assert!(env[0].children.iter().any(|child| child.name == "key"));
    }

    #[test]
    fn markdown_symbols_nest_headings_by_level() {
        let source = "# Title\n\nIntro text here.\n\n## Section One\n\nSome content.\n\n### Sub Section\n\nMore content.\n\nSection Two\n-----------\n\nSetext body.\n";
        let index = SymbolIndex::from_source(source, "markdown").unwrap();

        let title = index.find_by_name("# Title", Some(SymbolKind::Module));
        assert_eq!(title.len(), 1);
        assert_eq!(title[0].start_line, 1);

        let section_one = index.find_by_name("## Section One", Some(SymbolKind::Module));
        assert_eq!(section_one.len(), 1);
        assert_eq!(section_one[0].start_line, 5);
        assert!(
            title[0]
                .children
                .iter()
                .any(|child| child.signature == "## Section One")
        );

        let sub = index.find_by_name("### Sub Section", Some(SymbolKind::Module));
        assert_eq!(sub.len(), 1);
        assert!(
            section_one[0]
                .children
                .iter()
                .any(|child| child.signature == "### Sub Section")
        );

        // Setext heading is folded in with an ATX-equivalent level.
        let section_two = index.find_by_name("## Section Two", Some(SymbolKind::Module));
        assert_eq!(section_two.len(), 1);
        assert_eq!(section_two[0].start_line, 13);
    }

    #[test]
    fn elixir_defimpl_names_include_for_target() {
        let source = r#"defimpl MyProto, for: Atom do
  def render(value), do: value
end

defimpl MyProto, for: BitString do
  def render(value), do: value
end
"#;
        let index = SymbolIndex::from_source(source, "elixir").unwrap();

        let atom_impl = index.find_by_name("MyProto, for: Atom", Some(SymbolKind::Impl));
        let bitstring_impl = index.find_by_name("MyProto, for: BitString", Some(SymbolKind::Impl));
        assert_eq!(atom_impl.len(), 1);
        assert_eq!(bitstring_impl.len(), 1);
        assert_ne!(
            atom_impl[0].qualified_name,
            bitstring_impl[0].qualified_name
        );
    }

    #[test]
    fn elixir_test_named_functions_require_test_module_context() {
        let source = r#"defmodule MyApp.Connection do
  def test_connection, do: :ok
  def connection_test, do: :ok
end

defmodule MyApp.ConnectionTest do
  def test_connection, do: :ok
end
"#;
        let index = SymbolIndex::from_source(source, "elixir").unwrap();

        let module = index.find_by_name("MyApp.Connection", Some(SymbolKind::Module));
        assert_eq!(module.len(), 1);
        assert!(
            module[0]
                .children
                .iter()
                .any(|child| child.name == "test_connection" && child.kind == SymbolKind::Method)
        );
        assert!(
            module[0]
                .children
                .iter()
                .any(|child| child.name == "connection_test" && child.kind == SymbolKind::Method)
        );

        let test_module = index.find_by_name("MyApp.ConnectionTest", Some(SymbolKind::Module));
        assert_eq!(test_module.len(), 1);
        assert!(
            test_module[0]
                .children
                .iter()
                .any(|child| child.name == "test_connection" && child.kind == SymbolKind::Test)
        );
    }

    #[test]
    fn elixir_describe_and_test_calls_require_exunit_context() {
        let source = r#"defmodule MyApp.Schema do
  describe "fields" do
    test "accepts options" do
      :ok
    end
  end
end

defmodule MyApp.SchemaTest do
  use ExUnit.Case, async: true

  describe "fields" do
    test "accepts options" do
      assert :ok
    end
  end
end
"#;
        let index = SymbolIndex::from_source(source, "elixir").unwrap();

        let module = index.find_by_name("MyApp.Schema", Some(SymbolKind::Module));
        assert_eq!(module.len(), 1);
        let non_exunit_describe = module[0]
            .children
            .iter()
            .find(|child| child.name == "fields")
            .unwrap();
        assert_eq!(non_exunit_describe.kind, SymbolKind::Method);
        assert!(
            non_exunit_describe
                .children
                .iter()
                .any(|child| child.name == "accepts options" && child.kind == SymbolKind::Method)
        );
        assert!(
            index
                .find_by_name("MyApp.Schema::fields", Some(SymbolKind::Test))
                .is_empty()
        );

        let exunit_describe =
            index.find_by_name("MyApp.SchemaTest::fields", Some(SymbolKind::Test));
        assert_eq!(exunit_describe.len(), 1);
        assert!(
            exunit_describe[0]
                .children
                .iter()
                .any(|child| child.name == "accepts options" && child.kind == SymbolKind::Test)
        );
    }

    fn lean_find<'a>(
        index: &'a SymbolIndex,
        name: &str,
        kind: Option<SymbolKind>,
    ) -> &'a SymbolEntry {
        let found = index.find_by_name(name, kind);
        assert_eq!(found.len(), 1, "expected exactly one match for {name}");
        found[0]
    }

    #[test]
    fn lean_imports_and_opens_are_imports() {
        let source = "import Mathlib.Data.Nat.Basic\nopen Nat\nexport Foo (bar)\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let imports: Vec<_> = index
            .symbols
            .iter()
            .map(|s| (s.kind, s.signature.as_str(), s.start_line))
            .collect();
        assert_eq!(
            imports,
            vec![
                (SymbolKind::Import, "import Mathlib.Data.Nat.Basic", 1),
                (SymbolKind::Import, "open Nat", 2),
                (SymbolKind::Import, "export Foo (bar)", 3),
            ]
        );
    }

    #[test]
    fn lean_definitions_are_functions() {
        let source = "def a : Nat := 1\n\nabbrev MyNat := Nat\n\nopaque secret : Nat\n\nnoncomputable def bar (x : Nat) : Nat := x\n  where\n    helper (y : Nat) : Nat := y\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        for (name, line) in [("a", 1), ("MyNat", 3), ("secret", 5), ("bar", 7)] {
            let symbol = lean_find(&index, name, Some(SymbolKind::Function));
            assert_eq!(symbol.start_line, line);
        }
        let bar = lean_find(&index, "bar", None);
        assert_eq!(bar.signature, "noncomputable def bar (x : Nat) : Nat := x");
        assert_eq!(bar.children.len(), 1);
        assert_eq!(bar.children[0].name, "helper");
        assert_eq!(bar.children[0].kind, SymbolKind::Method);
        assert_eq!(bar.children[0].qualified_name, "bar.helper");
        assert_eq!(bar.children[0].parent.as_deref(), Some("bar"));
    }

    #[test]
    fn lean_types_and_instances() {
        let source = "structure Point where\n  x : Nat\n  y : Nat\n\nclass Shape (a : Type) where\n  area : a → Nat\n\ninductive Color where\n  | red\n  | green : Color\n\ninstance : ToString Point where\n  toString p := \"p\"\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();

        let point = lean_find(&index, "Point", None);
        assert_eq!(point.kind, SymbolKind::Struct);
        let fields: Vec<_> = point
            .children
            .iter()
            .map(|c| (c.name.as_str(), c.kind))
            .collect();
        assert_eq!(
            fields,
            vec![("x", SymbolKind::Field), ("y", SymbolKind::Field)]
        );

        let shape = lean_find(&index, "Shape", None);
        assert_eq!(shape.kind, SymbolKind::Trait);
        assert_eq!(shape.children.len(), 1);
        assert_eq!(shape.children[0].name, "area");
        assert_eq!(shape.children[0].kind, SymbolKind::Method);

        let color = lean_find(&index, "Color", None);
        assert_eq!(color.kind, SymbolKind::Enum);
        let variants: Vec<_> = color
            .children
            .iter()
            .map(|c| (c.name.as_str(), c.kind))
            .collect();
        assert_eq!(
            variants,
            vec![
                ("red", SymbolKind::EnumVariant),
                ("green", SymbolKind::EnumVariant)
            ]
        );

        let instance = lean_find(&index, "ToString Point", None);
        assert_eq!(instance.kind, SymbolKind::Impl);
        assert_eq!(instance.children.len(), 1);
        assert_eq!(instance.children[0].name, "toString");
        assert_eq!(instance.children[0].kind, SymbolKind::Method);
    }

    #[test]
    fn lean_axioms_and_constants() {
        let source = "axiom ax : 1 = 1\nconstant k : Nat\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(lean_find(&index, "ax", None).kind, SymbolKind::Const);
        assert_eq!(lean_find(&index, "k", None).kind, SymbolKind::Const);
    }

    #[test]
    fn lean_macro_commands_are_macros() {
        let source = "macro \"foo\" : term => `(1)\nsyntax \"bar\" term : term\nnotation \"⊕\" => Nat.add\nmacro_rules\n  | `(bar $x) => `($x)\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let macros: Vec<_> = index
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Macro)
            .map(|s| (s.start_line, s.signature.as_str()))
            .collect();
        assert_eq!(macros.len(), 4);
        assert_eq!(macros[0].0, 1);
        assert!(macros[0].1.starts_with("macro "));
        assert!(macros[1].1.starts_with("syntax "));
        assert!(macros[2].1.starts_with("notation "));
        assert!(macros[3].1.starts_with("macro_rules"));
    }

    #[test]
    fn lean_example_is_test() {
        let index = SymbolIndex::from_source("example : 1 = 1 := rfl\n", "lean").unwrap();
        assert_eq!(index.symbols.len(), 1);
        assert_eq!(index.symbols[0].kind, SymbolKind::Test);
    }

    #[test]
    fn attributed_declaration_label_and_range() {
        let source = "import Foo\n\n@[simp] private theorem foo : 1 = 1 :=\n  rfl\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let foo = lean_find(&index, "foo", None);
        assert_eq!(foo.kind, SymbolKind::Function);
        assert_eq!(foo.start_line, 3);
        assert_eq!(foo.end_line, 4);
        assert!(foo.signature.starts_with("private theorem foo"));
        assert!(!foo.signature.contains("@[simp]"));
        assert!(source[foo.start_byte..foo.end_byte].starts_with("@[simp]"));
    }

    #[test]
    fn declarations_nest_under_namespace() {
        let source = "namespace Geometry\nstructure Point where\n  x : Nat\n\ndef dist (p : Nat) : Nat := p\n\ntheorem dist_self : 1 = 1 := rfl\nend Geometry\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(index.symbols.len(), 1);
        let ns = &index.symbols[0];
        assert_eq!(ns.kind, SymbolKind::Module);
        assert_eq!(ns.signature, "namespace Geometry");
        assert_eq!(ns.start_line, 1);
        assert_eq!(ns.end_line, 8);
        let children: Vec<_> = ns
            .children
            .iter()
            .map(|c| {
                (
                    c.name.as_str(),
                    c.qualified_name.as_str(),
                    c.parent.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            children,
            vec![
                ("Point", "Geometry.Point", Some("Geometry")),
                ("dist", "Geometry.dist", Some("Geometry")),
                ("dist_self", "Geometry.dist_self", Some("Geometry")),
            ]
        );
        assert_eq!(
            ns.children[0].children[0].qualified_name,
            "Geometry.Point.x"
        );
    }

    #[test]
    fn nested_namespaces_nest_further() {
        let source = "namespace A\nnamespace B\ndef f := 1\nend B\nend A\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(index.symbols.len(), 1);
        let a = &index.symbols[0];
        assert_eq!(a.name, "A");
        assert_eq!(a.children.len(), 1);
        let b = &a.children[0];
        assert_eq!(b.name, "B");
        assert_eq!(b.kind, SymbolKind::Module);
        assert_eq!(b.qualified_name, "A.B");
        assert_eq!(b.parent.as_deref(), Some("A"));
        assert_eq!((b.start_line, b.end_line), (2, 4));
        assert_eq!(b.children.len(), 1);
        assert_eq!(b.children[0].name, "f");
        assert_eq!(b.children[0].qualified_name, "A.B.f");
        assert_eq!((a.start_line, a.end_line), (1, 5));
    }

    #[test]
    fn unclosed_namespace_runs_to_end_of_file() {
        let source = "namespace Foo\ndef a := 1\n\ndef b := 2\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(index.symbols.len(), 1);
        let ns = &index.symbols[0];
        assert_eq!(ns.name, "Foo");
        assert_eq!(ns.start_line, 1);
        assert_eq!(ns.end_line, 4);
        assert_eq!(ns.end_byte, source.trim_end().len());
        let names: Vec<_> = ns.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn sections_and_mutual_blocks_are_transparent() {
        let source = "namespace Foo\nsection Bar\ndef a := 1\nend Bar\n\nmutual\ndef b := 1\ndef c := 2\nend\n\ndef d := 3\nend Foo\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(index.symbols.len(), 1);
        let ns = &index.symbols[0];
        assert_eq!(ns.name, "Foo");
        assert_eq!(ns.end_line, 12);
        let names: Vec<_> = ns.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c", "d"]);
        assert!(ns.children.iter().all(|c| c.kind == SymbolKind::Function));
    }

    #[test]
    fn reopened_namespace_yields_separate_entries() {
        let source = "namespace Foo\ndef a := 1\nend Foo\n\nnamespace Foo\ndef b := 2\nend Foo\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let namespaces = index.find_by_name("Foo", Some(SymbolKind::Module));
        assert_eq!(namespaces.len(), 2);
        assert_eq!((namespaces[0].start_line, namespaces[0].end_line), (1, 3));
        assert_eq!((namespaces[1].start_line, namespaces[1].end_line), (5, 7));
        assert_eq!(namespaces[0].children[0].name, "a");
        assert_eq!(namespaces[1].children[0].name, "b");
    }

    #[test]
    fn dotted_declaration_name_inside_namespace() {
        let source = "namespace Geometry\ndef Point.norm (p : Nat) : Nat := p\nend Geometry\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let found = index.find_by_name("Geometry.Point.norm", None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Point.norm");
        assert_eq!(found[0].qualified_name, "Geometry.Point.norm");
        assert_eq!(
            lean_find(&index, "Point.norm", None).qualified_name,
            "Geometry.Point.norm"
        );
    }

    #[test]
    fn stray_named_end_does_not_close_namespace() {
        let source = "namespace Foo\nend Nope\ndef a := 1\nend Foo\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(index.symbols.len(), 1);
        assert_eq!(index.symbols[0].children.len(), 1);
    }

    #[test]
    fn partial_parse_still_yields_declarations() {
        let source = "def f (x : Nat) : Nat := x ⊕⊕ ⟦ x ⟧ ⟧\n\ndef g := 1\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let g = lean_find(&index, "g", Some(SymbolKind::Function));
        assert_eq!((g.start_line, g.end_line), (3, 3));
        assert_eq!(lean_find(&index, "f", None).start_line, 1);
    }

    #[test]
    fn lean_declaration_without_name_yields_no_symbol() {
        let source = "def a := ) )\ndef b := 3\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert!(index.find_by_name("unknown", None).is_empty());
        assert_eq!(lean_find(&index, "b", None).start_line, 2);
    }

    #[test]
    fn lean_in_prefixes_do_not_hide_declarations() {
        let source = "set_option maxHeartbeats 400000 in\ntheorem t1 : 1 = 1 := rfl\n\nopen Classical in\ntheorem t2 : 2 = 2 := rfl\n\nvariable (n : Nat) in\ndef f := n\n\nopen Nat\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(
            lean_find(&index, "t1", Some(SymbolKind::Function)).start_line,
            2
        );
        assert_eq!(
            lean_find(&index, "t2", Some(SymbolKind::Function)).start_line,
            5
        );
        assert_eq!(
            lean_find(&index, "f", Some(SymbolKind::Function)).start_line,
            8
        );
        // Only the plain `open` is an import; `open … in` is not one.
        let imports: Vec<_> = index
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Import)
            .collect();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].start_line, 10);
    }

    #[test]
    fn lean_in_prefix_inside_namespace_keeps_qualified_name() {
        let source =
            "namespace Foo\nset_option pp.all true in\ntheorem t : 1 = 1 := rfl\nend Foo\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let t = lean_find(&index, "Foo.t", Some(SymbolKind::Function));
        assert_eq!(t.start_line, 3);
    }

    #[test]
    fn dotted_namespace_closed_by_components() {
        // `namespace A.B` opens two scopes; `end B` then `end A` closes both.
        let source = "namespace A.B\ndef x := 1\nend B\ndef y := 2\nend A\ndef z := 3\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let ns = lean_find(&index, "A.B", Some(SymbolKind::Module));
        assert_eq!(ns.end_line, 5);
        assert_eq!(ns.signature, "namespace A.B");
        assert_eq!(ns.qualified_name, "A.B");
        let names: Vec<_> = ns
            .children
            .iter()
            .map(|c| c.qualified_name.as_str())
            .collect();
        assert_eq!(names, vec!["A.B.x", "A.y"]);
        assert_eq!(lean_find(&index, "z", None).qualified_name, "z");
    }

    #[test]
    fn dotted_end_closes_nested_namespaces() {
        let source = "namespace A\nnamespace B\ndef x := 1\nend A.B\ndef y := 2\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(index.symbols.len(), 2);
        assert_eq!(index.symbols[0].name, "A");
        assert_eq!(index.symbols[0].end_line, 4);
        assert_eq!(index.symbols[0].children[0].name, "B");
        assert_eq!(lean_find(&index, "y", None).qualified_name, "y");

        let whole = "namespace A.B\ndef x := 1\nend A.B\ndef y := 2\n";
        let index = SymbolIndex::from_source(whole, "lean").unwrap();
        assert_eq!(lean_find(&index, "A.B", None).end_line, 3);
        assert_eq!(lean_find(&index, "y", None).qualified_name, "y");
    }

    #[test]
    fn public_section_is_closed_by_end() {
        let source = "namespace Foo\npublic section\ndef a := 1\nend\ndef b := 2\nend Foo\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(index.symbols.len(), 1);
        let ns = &index.symbols[0];
        assert_eq!(ns.end_line, 6);
        let names: Vec<_> = ns.children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn unclosed_public_section_does_not_break_namespace() {
        let source = "public section\nnamespace Foo\ndef a := 1\nend Foo\ndef b := 2\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(lean_find(&index, "Foo", None).end_line, 4);
        assert_eq!(lean_find(&index, "b", None).qualified_name, "b");
    }

    #[test]
    fn lean_comment_between_attribute_and_declaration_is_not_the_label() {
        let source = "@[simp] -- note\ntheorem foo : 1 = 1 := rfl\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let foo = lean_find(&index, "foo", None);
        assert!(
            foo.signature.starts_with("theorem foo"),
            "{}",
            foo.signature
        );
    }

    #[test]
    fn lean_anonymous_instance_name_is_whitespace_normalised() {
        let source = "instance :   ToString\n    Point where\n  toString p := \"p\"\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(
            lean_find(&index, "ToString Point", Some(SymbolKind::Impl)).start_line,
            1
        );
    }

    #[test]
    fn lean_open_inside_namespace_stays_top_level_import() {
        let source = "namespace Foo\nopen Nat\ndef a := 1\nend Foo\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        let open = index
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Import)
            .expect("open is a top-level import");
        assert!(open.parent.is_none());
        let ns = lean_find(&index, "Foo", Some(SymbolKind::Module));
        assert_eq!(ns.children.len(), 1);
    }

    #[test]
    fn lean_root_prefix_ignores_enclosing_namespace() {
        let source = "namespace X\ndef _root_.Foo.bar := 1\nend X\n";
        let index = SymbolIndex::from_source(source, "lean").unwrap();
        assert_eq!(
            index
                .find_by_name("Foo.bar", Some(SymbolKind::Function))
                .len(),
            1
        );
    }
}

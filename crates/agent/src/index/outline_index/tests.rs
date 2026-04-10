//! Tests for outline_index extraction and formatting.

use super::common::IndexOptions;
use super::{format_outline, index_source};

fn default_opts() -> IndexOptions {
    IndexOptions::default()
}

fn find_section<'a>(
    sections: &'a [super::common::Section],
    name: &str,
) -> Option<&'a super::common::Section> {
    sections.iter().find(|s| s.name == name)
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

#[test]
fn rust_basic_outline() {
    let source = r#"
use std::collections::HashMap;
use std::io;

pub struct Config {
    pub name: String,
    pub retries: usize,
}

enum Status {
    Active,
    Inactive,
}

pub trait Validator {
    fn validate(&self) -> bool;
}

impl Config {
    pub fn new(name: String) -> Self {
        Self { name, retries: 3 }
    }
}

pub fn run(args: Vec<String>) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {}
}
"#;

    let sections = index_source(source, "rust", &default_opts()).unwrap();

    // imports
    let imports = find_section(&sections, "imports").unwrap();
    assert_eq!(imports.entries.len(), 2);
    assert!(imports.entries[0].label.contains("HashMap"));

    // types
    let types = find_section(&sections, "types").unwrap();
    assert!(
        types
            .entries
            .iter()
            .any(|e| e.label.contains("struct Config"))
    );
    assert!(
        types
            .entries
            .iter()
            .any(|e| e.label.contains("enum Status"))
    );

    // struct fields
    let config = types
        .entries
        .iter()
        .find(|e| e.label.contains("Config"))
        .unwrap();
    assert_eq!(config.children.len(), 2);
    assert!(config.children[0].label.contains("name"));

    // enum variants
    let status = types
        .entries
        .iter()
        .find(|e| e.label.contains("Status"))
        .unwrap();
    assert_eq!(status.children.len(), 2);

    // traits
    let traits = find_section(&sections, "traits").unwrap();
    assert!(traits.entries[0].label.contains("Validator"));

    // impls
    let impls = find_section(&sections, "impls").unwrap();
    assert!(impls.entries[0].label.contains("impl Config"));
    assert!(!impls.entries[0].children.is_empty());

    // functions
    let functions = find_section(&sections, "functions").unwrap();
    assert!(functions.entries[0].label.contains("fn run"));

    // tests
    let tests = find_section(&sections, "tests").unwrap();
    assert!(!tests.entries.is_empty());

    // line ranges are valid
    for section in &sections {
        for entry in &section.entries {
            assert!(
                entry.start_line <= entry.end_line,
                "Invalid range for {}",
                entry.label
            );
            assert!(entry.start_line > 0, "Zero start_line for {}", entry.label);
        }
    }
}

#[test]
fn rust_exclude_tests() {
    let source = r#"
pub fn run() {}

#[cfg(test)]
mod tests {
    #[test]
    fn test_it() {}
}
"#;

    let mut opts = default_opts();
    opts.include_tests = false;
    let sections = index_source(source, "rust", &opts).unwrap();

    assert!(find_section(&sections, "tests").is_none());
    assert!(find_section(&sections, "functions").is_some());
}

#[test]
fn rust_truncate_children() {
    let source = r#"
pub struct Big {
    pub a: u32,
    pub b: u32,
    pub c: u32,
    pub d: u32,
    pub e: u32,
}
"#;

    let mut opts = default_opts();
    opts.max_children_per_item = Some(2);
    let sections = index_source(source, "rust", &opts).unwrap();

    let types = find_section(&sections, "types").unwrap();
    let big = &types.entries[0];
    assert_eq!(big.children.len(), 3); // 2 real + 1 "... (3 more)"
    assert!(big.children[2].label.contains("3 more"));
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

#[test]
fn python_basic_outline() {
    let source = r#"
import os
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
"#;

    let sections = index_source(source, "python", &default_opts()).unwrap();

    let imports = find_section(&sections, "imports").unwrap();
    assert_eq!(imports.entries.len(), 2);

    let classes = find_section(&sections, "classes").unwrap();
    assert!(classes.entries[0].label.contains("class Config"));
    assert_eq!(classes.entries[0].children.len(), 2); // __init__ and validate

    let functions = find_section(&sections, "functions").unwrap();
    assert!(
        functions
            .entries
            .iter()
            .any(|e| e.label.contains("def main"))
    );

    let tests = find_section(&sections, "tests").unwrap();
    assert!(
        tests
            .entries
            .iter()
            .any(|e| e.label.contains("test_something"))
    );
}

// ---------------------------------------------------------------------------
// TypeScript
// ---------------------------------------------------------------------------

#[test]
fn typescript_basic_outline() {
    let source = r#"
import { useState } from 'react';
import axios from 'axios';

interface Config {
    name: string;
    retries: number;
}

export class AppService {
    constructor(private config: Config) {}

    async fetchData(): Promise<void> {
        return;
    }
}

export function run(args: string[]): void {
    console.log(args);
}

const DEFAULT_TIMEOUT = 5000;
"#;

    let sections = index_source(source, "typescript", &default_opts()).unwrap();

    let imports = find_section(&sections, "imports").unwrap();
    assert_eq!(imports.entries.len(), 2);

    let types = find_section(&sections, "types").unwrap();
    assert!(
        types
            .entries
            .iter()
            .any(|e| e.label.contains("interface Config"))
    );

    let classes = find_section(&sections, "classes").unwrap();
    assert!(
        classes
            .entries
            .iter()
            .any(|e| e.label.contains("class AppService"))
    );

    let functions = find_section(&sections, "functions").unwrap();
    assert!(
        functions
            .entries
            .iter()
            .any(|e| e.label.contains("function run"))
    );
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

#[test]
fn go_basic_outline() {
    let source = r#"
package main

import (
    "fmt"
    "os"
)

type Config struct {
    Name    string
    Retries int
}

type Runner interface {
    Run() error
}

func NewConfig(name string) *Config {
    return &Config{Name: name}
}

func (c *Config) Validate() bool {
    return true
}

func TestConfig(t *testing.T) {
    // test
}
"#;

    let sections = index_source(source, "go", &default_opts()).unwrap();

    let imports = find_section(&sections, "imports").unwrap();
    assert!(!imports.entries.is_empty());

    let types = find_section(&sections, "types").unwrap();
    assert!(
        types
            .entries
            .iter()
            .any(|e| e.label.contains("Config struct"))
    );
    assert!(
        types
            .entries
            .iter()
            .any(|e| e.label.contains("Runner interface"))
    );

    let functions = find_section(&sections, "functions").unwrap();
    assert!(
        functions
            .entries
            .iter()
            .any(|e| e.label.contains("NewConfig"))
    );

    let tests = find_section(&sections, "tests").unwrap();
    assert!(tests.entries.iter().any(|e| e.label.contains("TestConfig")));
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

#[test]
fn java_basic_outline() {
    let source = r#"
package com.example;

import java.util.List;
import java.util.Map;

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
"#;

    let sections = index_source(source, "java", &default_opts()).unwrap();

    let pkg = find_section(&sections, "package").unwrap();
    assert!(!pkg.entries.is_empty());

    let imports = find_section(&sections, "imports").unwrap();
    assert_eq!(imports.entries.len(), 2);

    let classes = find_section(&sections, "classes").unwrap();
    assert!(classes.entries[0].label.contains("class Config"));
    assert!(classes.entries[0].children.len() >= 2); // constructor + getName at minimum

    let interfaces = find_section(&sections, "interfaces").unwrap();
    assert!(interfaces.entries[0].label.contains("Validator"));
}

// ---------------------------------------------------------------------------
// C
// ---------------------------------------------------------------------------

#[test]
fn c_basic_outline() {
    let source = r#"
#include <stdio.h>
#include <stdlib.h>

#define MAX_SIZE 1024

struct Config {
    char *name;
    int retries;
};

enum Status {
    ACTIVE,
    INACTIVE
};

void run(int argc, char *argv[]) {
    printf("hello\n");
}
"#;

    let sections = index_source(source, "c", &default_opts()).unwrap();

    let includes = find_section(&sections, "includes").unwrap();
    assert_eq!(includes.entries.len(), 2);

    let types = find_section(&sections, "types").unwrap();
    assert!(
        types
            .entries
            .iter()
            .any(|e| e.label.contains("struct Config"))
    );
    assert!(
        types
            .entries
            .iter()
            .any(|e| e.label.contains("enum Status"))
    );

    let functions = find_section(&sections, "functions").unwrap();
    assert!(functions.entries.iter().any(|e| e.label.contains("run")));
}

// ---------------------------------------------------------------------------
// C#
// ---------------------------------------------------------------------------

#[test]
fn csharp_basic_outline() {
    let source = r#"
using System;
using System.Collections.Generic;

namespace MyApp
{
    public class Config
    {
        public string Name { get; set; }

        public Config(string name)
        {
            Name = name;
        }

        public bool Validate()
        {
            return true;
        }
    }

    public interface IValidator
    {
        bool Validate();
    }
}
"#;

    let sections = index_source(source, "csharp", &default_opts()).unwrap();

    let usings = find_section(&sections, "usings").unwrap();
    assert_eq!(usings.entries.len(), 2);

    let classes = find_section(&sections, "classes").unwrap();
    assert!(
        classes
            .entries
            .iter()
            .any(|e| e.label.contains("class Config"))
    );

    let interfaces = find_section(&sections, "interfaces").unwrap();
    assert!(
        interfaces
            .entries
            .iter()
            .any(|e| e.label.contains("IValidator"))
    );
}

// ---------------------------------------------------------------------------
// Ruby
// ---------------------------------------------------------------------------

#[test]
fn ruby_basic_outline() {
    let source = r#"
require 'json'
require_relative 'helper'

class Config
  def initialize(name)
    @name = name
  end

  def validate
    true
  end
end

def run(args)
  puts args
end
"#;

    let sections = index_source(source, "ruby", &default_opts()).unwrap();

    let requires = find_section(&sections, "requires").unwrap();
    assert_eq!(requires.entries.len(), 2);

    let classes = find_section(&sections, "classes").unwrap();
    assert!(classes.entries[0].label.contains("class Config"));
    assert_eq!(classes.entries[0].children.len(), 2);

    let functions = find_section(&sections, "functions").unwrap();
    assert!(
        functions
            .entries
            .iter()
            .any(|e| e.label.contains("def run"))
    );
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

#[test]
fn format_output_structure() {
    let source = r#"
use std::io;

pub fn hello() -> String {
    "hi".to_string()
}
"#;

    let sections = index_source(source, "rust", &default_opts()).unwrap();
    let output = format_outline("src/lib.rs", "rust", &sections);

    assert!(output.starts_with("path: src/lib.rs\n"));
    assert!(output.contains("language: rust\n"));
    assert!(output.contains("imports:\n"));
    assert!(output.contains("functions:\n"));
    assert!(output.contains("["));
    assert!(output.contains("-"));
}

// ---------------------------------------------------------------------------
// Error cases
// ---------------------------------------------------------------------------

#[test]
fn unsupported_language() {
    let result = index_source("stuff", "brainfuck", &default_opts());
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("Unsupported") || err.contains("brainfuck"));
}

// ---------------------------------------------------------------------------
// Line range correctness
// ---------------------------------------------------------------------------

#[test]
fn line_ranges_are_correct() {
    let source = "use std::io;\n\npub fn hello() {\n    println!(\"hi\");\n}\n";
    // Lines:
    // 1: use std::io;
    // 2: (empty)
    // 3: pub fn hello() {
    // 4:     println!("hi");
    // 5: }

    let sections = index_source(source, "rust", &default_opts()).unwrap();

    let imports = find_section(&sections, "imports").unwrap();
    assert_eq!(imports.entries[0].start_line, 1);
    assert_eq!(imports.entries[0].end_line, 1);

    let functions = find_section(&sections, "functions").unwrap();
    assert_eq!(functions.entries[0].start_line, 3);
    assert_eq!(functions.entries[0].end_line, 5);
}

// ---------------------------------------------------------------------------
// safe_slice helper
// ---------------------------------------------------------------------------

#[test]
fn safe_slice_on_ascii() {
    use super::extractors::helpers::safe_slice;
    let s = "hello world";
    assert_eq!(safe_slice(s, 0, 5), "hello");
    assert_eq!(safe_slice(s, 6, 11), "world");
    assert_eq!(safe_slice(s, 0, 11), "hello world");
}

#[test]
fn safe_slice_snaps_to_char_boundaries() {
    use super::extractors::helpers::safe_slice;
    // '─' is U+2500, encoded as 3 bytes: 0xE2 0x94 0x80
    let s = "ab─cd";
    // byte layout: a(0) b(1) 0xE2(2) 0x94(3) 0x80(4) c(5) d(6)

    // Slicing from the middle of '─' (byte 3) should snap down to byte 2
    let result = safe_slice(s, 3, 6);
    assert_eq!(result, "─c");

    // Slicing to the middle of '─' (byte 3) should snap up to byte 5
    let result = safe_slice(s, 0, 3);
    assert_eq!(result, "ab─");

    // Both from and to inside the multi-byte char should give the full char
    let result = safe_slice(s, 3, 4);
    assert_eq!(result, "─");
}

#[test]
fn safe_slice_out_of_bounds_clamped() {
    use super::extractors::helpers::safe_slice;
    let s = "abc";
    // `to` past end should be clamped
    assert_eq!(safe_slice(s, 0, 100), "abc");
    // `from` past end should return empty
    assert_eq!(safe_slice(s, 100, 200), "");
}

// ---------------------------------------------------------------------------
// Multi-byte UTF-8 in Rust source (regression test for the panic)
// ---------------------------------------------------------------------------

#[test]
fn rust_multibyte_utf8_does_not_panic() {
    // This source has a box-drawing character in a comment, followed by a
    // #[test] function. The lookback in is_test_function could previously
    // land mid-character and panic.
    let source = r#"
// ──────────────────────────────────────────────────────────────────────────
// Some section header with box-drawing chars above
// ──────────────────────────────────────────────────────────────────────────

use std::io;

#[test]
fn my_test() {
    assert!(true);
}

fn regular_func() -> bool {
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn inner_test() {}
}
"#;
    // Must not panic; the multi-byte '─' chars could cause lookback byte
    // offsets to land inside a character.
    let opts = default_opts();
    let sections = index_source(source, "rust", &opts).unwrap();

    // Verify test detection still works correctly despite multi-byte chars
    let tests = find_section(&sections, "tests");
    assert!(tests.is_some(), "should detect test items");
    let test_entries = &tests.unwrap().entries;
    let labels: Vec<&str> = test_entries.iter().map(|e| e.label.as_str()).collect();
    assert!(
        labels.iter().any(|l| l.contains("my_test")),
        "should find my_test in test section, got: {labels:?}"
    );
}

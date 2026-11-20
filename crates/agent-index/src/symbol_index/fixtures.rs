//! Multi-language source fixtures shared by tests that need realistic symbol inputs.

/// `(language, source)` pairs covering several extractor families.
pub(crate) fn multi_language_fixtures() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "rust",
            r#"pub struct Config {
    pub name: String,
}

impl Config {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

pub fn run() {}
"#,
        ),
        (
            "typescript",
            r#"import axios from 'axios';

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
"#,
        ),
        (
            "python",
            r#"import os
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
"#,
        ),
        (
            "csharp",
            r#"using System;

namespace MyApp {
    public class Config {
        public string Name { get; set; }
        public bool Validate() { return true; }
    }

    public interface IValidator {
        bool Validate();
    }
}
"#,
        ),
        (
            "ruby",
            r#"require 'json'
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
"#,
        ),
        (
            "json",
            r#"{
    "name": "demo",
    "retries": 3,
    "server": {
        "host": "localhost",
        "port": 8080
    },
    "tags": ["a", "b"]
}
"#,
        ),
    ]
}

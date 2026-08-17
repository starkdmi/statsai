//! Decides which files count as code, and which are noise.

use super::*;
use std::path::Path;

#[must_use]
pub fn classify_code_path(path: &Path) -> Option<CodeCategory> {
    if is_excluded_code_path(path) {
        return None;
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())?
        .to_ascii_lowercase();
    if !matches!(
        extension.as_str(),
        "asm"
            | "astro"
            | "bash"
            | "c"
            | "cc"
            | "clj"
            | "cljc"
            | "cljs"
            | "cpp"
            | "cs"
            | "css"
            | "cts"
            | "cxx"
            | "dart"
            | "erl"
            | "ex"
            | "exs"
            | "fish"
            | "fs"
            | "fsi"
            | "fsx"
            | "go"
            | "gql"
            | "graphql"
            | "groovy"
            | "h"
            | "hh"
            | "hpp"
            | "hrl"
            | "hs"
            | "htm"
            | "html"
            | "hxx"
            | "java"
            | "jl"
            | "js"
            | "jsx"
            | "kt"
            | "kts"
            | "less"
            | "lhs"
            | "lua"
            | "m"
            | "ml"
            | "mli"
            | "mm"
            | "mjs"
            | "mts"
            | "nim"
            | "php"
            | "pl"
            | "pm"
            | "proto"
            | "ps1"
            | "psm1"
            | "py"
            | "pyi"
            | "pyw"
            | "r"
            | "raku"
            | "rb"
            | "rs"
            | "s"
            | "sass"
            | "scala"
            | "sc"
            | "scss"
            | "sh"
            | "sol"
            | "sql"
            | "svelte"
            | "swift"
            | "ts"
            | "tsx"
            | "vb"
            | "vue"
            | "wat"
            | "zig"
            | "zsh"
    ) {
        return None;
    }
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_test = normalized.split('/').any(|part| {
        matches!(
            part,
            "test" | "tests" | "spec" | "specs" | "__tests__" | "fixtures"
        )
    }) || file_name.contains(".test.")
        || file_name.contains(".spec.")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with("_tests.rs")
        || file_name.ends_with("_test.go")
        || file_name.ends_with("tests.swift")
        || file_name.starts_with("test_")
        || file_name.ends_with("_test.py");
    Some(if is_test {
        CodeCategory::Test
    } else {
        CodeCategory::Source
    })
}

#[must_use]
pub fn is_excluded_code_path(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let excluded_directory = normalized.split('/').any(|part| {
        matches!(
            part,
            ".git"
                | "node_modules"
                | "vendor"
                | "vendors"
                | "dist"
                | "build"
                | "target"
                | ".next"
                | ".cache"
                | "coverage"
                | "generated"
                | "__generated__"
                | "gen"
                | "out"
                | ".turbo"
                | ".gradle"
                | ".dart_tool"
                | "pods"
                | "deriveddata"
        )
    });
    let lockfile = matches!(
        file_name.as_str(),
        "cargo.lock"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "poetry.lock"
            | "uv.lock"
            | "pipfile.lock"
            | "composer.lock"
            | "gemfile.lock"
            | "go.sum"
            | "go.work.sum"
            | "bun.lock"
            | "bun.lockb"
            | "podfile.lock"
            | "package.resolved"
            | "gradle.lockfile"
    );
    excluded_directory
        || lockfile
        || file_name.ends_with(".min.js")
        || file_name.ends_with(".min.css")
        || file_name.ends_with(".map")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_lockfiles_generated_directories_and_minified_files() {
        assert!(classify_code_path(Path::new("Cargo.lock")).is_none());
        assert!(classify_code_path(Path::new("node_modules/pkg/index.js")).is_none());
        assert!(classify_code_path(Path::new("src/app.min.js")).is_none());
        assert_eq!(
            classify_code_path(Path::new("src/app.rs")),
            Some(CodeCategory::Source)
        );
    }

    #[test]
    fn ignores_documentation_configuration_manifests_and_unknown_text() {
        for path in [
            "README.md",
            "docs/guide.txt",
            "Cargo.toml",
            "package.json",
            ".github/workflows/ci.yml",
            "tests/fixtures/example.md",
        ] {
            assert_eq!(classify_code_path(Path::new(path)), None, "{path}");
        }
        assert_eq!(
            classify_code_path(Path::new("src/component.tsx")),
            Some(CodeCategory::Source)
        );
        assert_eq!(
            classify_code_path(Path::new("tests/component_test.py")),
            Some(CodeCategory::Test)
        );
    }
}

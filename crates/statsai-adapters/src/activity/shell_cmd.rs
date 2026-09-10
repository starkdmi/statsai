//! Transient POSIX command classification for activity rollups.
//!
//! The raw command string is never stored. Only a vocabulary basename or
//! `other` is returned. Path-qualified invocations collapse to `other`.

/// Vocabulary revision for the public-registry binary head list.
#[allow(dead_code)]
pub const ACTIVITY_SHELL_BIN_REVISION: &str = "activity-shell-bins.v1";
pub const OTHER_COMMAND: &str = "other";

const RESERVED_WORDS: &[&str] = &[
    "!", "[[", "]]", "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if",
    "in", "select", "then", "time", "until", "while", "{", "}",
];

const BUILTINS: &[&str] = &[
    ".",
    ":",
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "cd",
    "command",
    "compgen",
    "complete",
    "continue",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "help",
    "history",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "newgrp",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
];

const WRAPPERS: &[&str] = &[
    "command", "env", "nice", "nohup", "stdbuf", "time", "timeout",
];

const SHELLS: &[&str] = &["bash", "dash", "fish", "ksh", "sh", "zsh"];

/// Public-registry head binaries (Debian Contents, crates.io, npm, PyPI,
/// RubyGems). Path-qualified names never match. Sorted for binary search.
const VOCABULARY: &[&str] = &[
    "7z",
    "ag",
    "alacritty",
    "apt",
    "apt-get",
    "ar",
    "aria2c",
    "asciinema",
    "autoconf",
    "automake",
    "awk",
    "aws",
    "az",
    "bat",
    "bazel",
    "black",
    "brotli",
    "btop",
    "buck",
    "buck2",
    "bun",
    "bundle",
    "bundler",
    "c++",
    "cabal",
    "cargo",
    "cat",
    "cc",
    "ccache",
    "clang",
    "clang++",
    "clang-format",
    "clang-tidy",
    "cmake",
    "column",
    "comm",
    "composer",
    "corepack",
    "cp",
    "curl",
    "cut",
    "dart",
    "date",
    "dd",
    "delta",
    "diff",
    "dnf",
    "docker",
    "docker-compose",
    "dotnet",
    "dpkg",
    "dust",
    "elixir",
    "emacs",
    "erlang",
    "exa",
    "eza",
    "fd",
    "ffmpeg",
    "find",
    "fish",
    "flake8",
    "flex",
    "flutter",
    "fzf",
    "g++",
    "gcc",
    "gcloud",
    "gem",
    "gh",
    "ghostty",
    "git",
    "go",
    "gofmt",
    "golangci-lint",
    "gradle",
    "grep",
    "gzip",
    "head",
    "helix",
    "helm",
    "hg",
    "htop",
    "http",
    "httpie",
    "hugo",
    "hyperfine",
    "install",
    "java",
    "javac",
    "jj",
    "jq",
    "just",
    "kak",
    "kitty",
    "kotlin",
    "kubectl",
    "lazygit",
    "ld",
    "less",
    "llvm-ar",
    "ln",
    "ls",
    "lsof",
    "lua",
    "make",
    "man",
    "md5sum",
    "meson",
    "mkdir",
    "mv",
    "mypy",
    "nano",
    "nc",
    "ncdu",
    "ninja",
    "nix",
    "nix-env",
    "nix-shell",
    "node",
    "npm",
    "npx",
    "nvim",
    "objcopy",
    "objdump",
    "openssl",
    "oxlint",
    "pacman",
    "patch",
    "pdm",
    "perl",
    "php",
    "pigz",
    "ping",
    "pip",
    "pip3",
    "pipx",
    "pkg-config",
    "pnpm",
    "podman",
    "poetry",
    "prettier",
    "psql",
    "pulumi",
    "pv",
    "pyright",
    "pytest",
    "python",
    "python3",
    "rake",
    "rg",
    "ripgrep",
    "rm",
    "rspec",
    "rsync",
    "ruby",
    "ruff",
    "rustc",
    "rustfmt",
    "rustup",
    "sccache",
    "scp",
    "sed",
    "sha256sum",
    "shellcheck",
    "sort",
    "sqlite3",
    "ssh",
    "stack",
    "stat",
    "strip",
    "stylua",
    "sudo",
    "svn",
    "swift",
    "swiftc",
    "sync",
    "systemctl",
    "tar",
    "task",
    "tee",
    "terraform",
    "tldr",
    "tmux",
    "tokei",
    "touch",
    "tput",
    "tr",
    "tree",
    "tsc",
    "tsx",
    "uniq",
    "unzip",
    "uv",
    "uvx",
    "vim",
    "wasm-pack",
    "watch",
    "wc",
    "wget",
    "which",
    "xargs",
    "xz",
    "yarn",
    "yq",
    "yt-dlp",
    "zstd",
];

const WRITE_BINS: &[&str] = &[
    "cp", "dd", "install", "mv", "patch", "rsync", "sponge", "tee",
];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Operator(String),
}

#[must_use]
pub fn classify_shell_command(command: &str) -> String {
    classify_tokens(&posix_tokens(command))
}

#[must_use]
pub fn classify_shell_argv(argv: &[String]) -> String {
    if argv.is_empty() {
        return OTHER_COMMAND.to_string();
    }
    if let Some(script) = wrapper_script(argv) {
        return classify_shell_command(script);
    }
    let mut words = argv.iter().map(String::as_str).collect::<Vec<_>>();
    skip_wrappers(&mut words);
    if words.is_empty() {
        return OTHER_COMMAND.to_string();
    }
    classify_head(words[0])
}

pub fn shell_command_is_write(command: &str) -> bool {
    let tokens = posix_tokens(command);
    if tokens
        .iter()
        .any(|token| matches!(token, Token::Operator(op) if op == ">" || op == ">>"))
    {
        return true;
    }
    WRITE_BINS
        .binary_search(&classify_tokens(&tokens).as_str())
        .is_ok()
}

#[must_use]
pub fn shell_argv_is_write(argv: &[String]) -> bool {
    if argv.iter().any(|word| word == ">" || word == ">>") {
        return true;
    }
    if let Some(script) = wrapper_script(argv) {
        return shell_command_is_write(script);
    }
    WRITE_BINS
        .binary_search(&classify_shell_argv(argv).as_str())
        .is_ok()
}

fn classify_tokens(tokens: &[Token]) -> String {
    for simple in simple_commands(tokens) {
        let mut words = simple;
        skip_assignments(&mut words);
        skip_wrappers(&mut words);
        let Some(head) = words.first().copied() else {
            continue;
        };
        if is_path_qualified(head) {
            return OTHER_COMMAND.to_string();
        }
        let base = basename(head);
        if is_reserved_or_builtin(base) {
            continue;
        }
        return classify_head(head);
    }
    OTHER_COMMAND.to_string()
}

fn classify_head(head: &str) -> String {
    if is_path_qualified(head) {
        return OTHER_COMMAND.to_string();
    }
    let base = basename(head);
    if VOCABULARY.binary_search(&base).is_ok() {
        base.to_string()
    } else {
        OTHER_COMMAND.to_string()
    }
}

fn wrapper_script(argv: &[String]) -> Option<&str> {
    if argv.is_empty() {
        return None;
    }
    let bin = basename(argv[0].as_str());
    if !SHELLS.binary_search(&bin).is_ok() {
        return None;
    }
    let mut index = 1;
    while index < argv.len() {
        let flag = argv[index].as_str();
        if flag == "--" {
            return argv.get(index + 1).map(String::as_str);
        }
        if flag == "-c" || flag == "-lc" {
            return argv.get(index + 1).map(String::as_str);
        }
        if flag.starts_with('-') && !flag.starts_with("--") && flag.contains('c') {
            return argv.get(index + 1).map(String::as_str);
        }
        if flag.starts_with('-') {
            index += 1;
            continue;
        }
        break;
    }
    None
}

fn skip_wrappers(words: &mut Vec<&str>) {
    loop {
        let Some(head) = words.first().copied() else {
            return;
        };
        let base = basename(head);
        if WRAPPERS.binary_search(&base).is_ok() {
            words.remove(0);
            while let Some(word) = words.first().copied() {
                if word.starts_with('-') || (base == "env" && is_assignment(word)) {
                    words.remove(0);
                    continue;
                }
                break;
            }
            continue;
        }
        return;
    }
}

fn skip_assignments(words: &mut Vec<&str>) {
    while words.first().is_some_and(|word| is_assignment(word)) {
        words.remove(0);
    }
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_path_qualified(word: &str) -> bool {
    word.starts_with('.') || word.starts_with('~') || word.starts_with('/') || word.contains('/')
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_reserved_or_builtin(word: &str) -> bool {
    RESERVED_WORDS.binary_search(&word).is_ok() || BUILTINS.binary_search(&word).is_ok()
}

fn simple_commands(tokens: &[Token]) -> Vec<Vec<&str>> {
    let mut commands = Vec::new();
    let mut current = Vec::new();
    for token in tokens {
        match token {
            Token::Word(word) => current.push(word.as_str()),
            Token::Operator(op)
                if op == "|" || op == "||" || op == "&&" || op == ";" || op == "&" =>
            {
                if !current.is_empty() {
                    commands.push(std::mem::take(&mut current));
                }
            }
            Token::Operator(_) => {}
        }
    }
    if !current.is_empty() {
        commands.push(current);
    }
    commands
}

fn posix_tokens(input: &str) -> Vec<Token> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' {
            i += 1;
            continue;
        }
        if ch == '#' && (i == 0 || chars[i - 1].is_whitespace()) {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if let Some((op, len)) = operator_at(&chars, i) {
            tokens.push(Token::Operator(op.to_string()));
            i += len;
            continue;
        }
        let (word, next) = read_word(&chars, i);
        if !word.is_empty() {
            tokens.push(Token::Word(word));
        }
        i = next;
    }
    tokens
}

fn operator_at(chars: &[char], i: usize) -> Option<(&'static str, usize)> {
    let next = chars.get(i + 1).copied();
    match (chars[i], next) {
        ('&', Some('&')) => Some(("&&", 2)),
        ('|', Some('|')) => Some(("||", 2)),
        ('>', Some('>')) => Some((">>", 2)),
        ('>', Some('&')) => Some((">&", 2)),
        ('<', Some('<')) => Some(("<<", 2)),
        ('|', _) => Some(("|", 1)),
        ('&', _) => Some(("&", 1)),
        (';', _) => Some((";", 1)),
        ('>', _) => Some((">", 1)),
        ('<', _) => Some(("<", 1)),
        ('(', _) => Some(("(", 1)),
        (')', _) => Some((")", 1)),
        _ => None,
    }
}

fn read_word(chars: &[char], start: usize) -> (String, usize) {
    let mut out = String::new();
    let mut i = start;
    while i < chars.len() {
        let ch = chars[i];
        if ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r' {
            break;
        }
        if operator_at(chars, i).is_some() {
            break;
        }
        if ch == '\\' {
            i += 1;
            if i < chars.len() {
                if chars[i] != '\n' {
                    out.push(chars[i]);
                }
                i += 1;
            }
            continue;
        }
        if ch == '\'' {
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                out.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            continue;
        }
        if ch == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    let next = chars[i + 1];
                    if next == '"' || next == '\\' || next == '$' || next == '`' || next == '\n' {
                        if next != '\n' {
                            out.push(next);
                        }
                        i += 2;
                        continue;
                    }
                }
                out.push(chars[i]);
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            continue;
        }
        out.push(ch);
        i += 1;
    }
    (out, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_sorted(names: &[&str]) {
        let mut sorted = names.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted.as_slice());
    }

    #[test]
    fn tables_are_sorted_for_binary_search() {
        assert_sorted(RESERVED_WORDS);
        assert_sorted(BUILTINS);
        assert_sorted(WRAPPERS);
        assert_sorted(SHELLS);
        assert_sorted(VOCABULARY);
        assert_sorted(WRITE_BINS);
    }

    #[test]
    fn posix_tokenizer_keeps_quoted_regex_and_scripts_intact() {
        assert_eq!(classify_shell_command("rg 'foo|bar' src"), "rg");
        assert_eq!(
            classify_shell_command("python -c \"import os; print(1 | 2)\""),
            "python"
        );
        assert_eq!(classify_shell_command("git log | head -n 20"), "git");
        assert_eq!(classify_shell_command("cd crates && cargo test"), "cargo");
        assert_eq!(classify_shell_command("./hack.sh && cargo test"), "other");
        assert_eq!(classify_shell_command("~/bin/foo"), "other");
        assert_eq!(classify_shell_command("/opt/acme/bin/x"), "other");
        assert_eq!(classify_shell_command("cd src"), "other");
        assert_eq!(
            classify_shell_argv(&["bash".into(), "-lc".into(), "cargo test".into()]),
            "cargo"
        );
        assert_eq!(classify_shell_argv(&["git".into(), "status".into()]), "git");
        assert_eq!(ACTIVITY_SHELL_BIN_REVISION, "activity-shell-bins.v1");
        assert!(shell_command_is_write(
            "cat <<'EOF' > src/main.rs\nfn main() {}\nEOF"
        ));
        assert!(shell_command_is_write("tee README.md"));
        assert!(!shell_command_is_write("cargo test"));
    }
}

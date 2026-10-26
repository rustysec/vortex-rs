//! Language registry: detection, tree-sitter grammars, comment tokens.

use std::path::Path;

use tree_sitter::Language;

/// Everything vortex knows about one language.
#[derive(Debug, Clone, Copy)]
pub struct LanguageInfo {
    /// Stable identifier used in config (`[format.command]`, `[lsp.servers]`).
    pub id: &'static str,
    /// Name shown in the status area.
    pub name: &'static str,
    /// File extensions, space separated (no dots).
    pub extensions: &'static str,
    /// Exact file names, space separated.
    pub filenames: &'static str,
    /// Interpreter names found in `#!` lines, space separated.
    pub shebangs: &'static str,
    /// Line comment token, empty if the language has none.
    pub line_comment: &'static str,
    /// `(/* */)` style comment delimiters.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// Default indentation width for this language.
    pub indent_width: u16,
    /// tree-sitter grammar, if bundled.
    grammar: Option<fn() -> Language>,
}

impl LanguageInfo {
    pub fn grammar(&self) -> Option<Language> {
        self.grammar.map(|grammar| grammar())
    }

    pub fn has_tree_sitter(&self) -> bool {
        self.grammar.is_some()
    }

    /// Comment prefix used by `gc` and by the spell checker.
    pub fn comment_token(&self) -> &'static str {
        self.line_comment
    }

    pub fn covers_extension(&self, ext: &str) -> bool {
        self.extensions.split_whitespace().any(|e| e == ext)
    }

    pub fn covers_filename(&self, name: &str) -> bool {
        self.filenames.split_whitespace().any(|f| f == name)
    }

    pub fn covers_shebang(&self, name: &str) -> bool {
        self.shebangs.split_whitespace().any(|s| name.ends_with(s))
    }
}

const fn none() -> Option<fn() -> Language> {
    None
}

/// Registered languages, in lookup order.
pub const LANGUAGES: &[LanguageInfo] = &[
    LanguageInfo {
        id: "rust",
        name: "Rust",
        extensions: "rs",
        filenames: "",
        shebangs: "",
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
        indent_width: 4,
        grammar: Some(|| tree_sitter_rust::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "python",
        name: "Python",
        extensions: "py pyi pyw",
        filenames: "pyproject.toml",
        shebangs: "python python3",
        line_comment: "#",
        block_comment: None,
        indent_width: 4,
        grammar: Some(|| tree_sitter_python::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "javascript",
        name: "JavaScript",
        extensions: "js jsx mjs cjs",
        filenames: "",
        shebangs: "node nodejs deno bun",
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        grammar: Some(|| tree_sitter_javascript::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "typescript",
        name: "TypeScript",
        extensions: "ts mts cts",
        filenames: "",
        shebangs: "ts-node tsx",
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        grammar: Some(|| tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
    },
    LanguageInfo {
        id: "tsx",
        name: "TSX",
        extensions: "tsx",
        filenames: "",
        shebangs: "",
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        grammar: Some(|| tree_sitter_typescript::LANGUAGE_TSX.into()),
    },
    LanguageInfo {
        id: "json",
        name: "JSON",
        extensions: "json jsonc jsonl",
        filenames: ".babelrc .eslintrc",
        shebangs: "",
        line_comment: "",
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        grammar: Some(|| tree_sitter_json::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "toml",
        name: "TOML",
        extensions: "toml",
        filenames: "Cargo.toml config.toml",
        shebangs: "",
        line_comment: "#",
        block_comment: None,
        indent_width: 2,
        grammar: Some(|| tree_sitter_toml_ng::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "yaml",
        name: "YAML",
        extensions: "yaml yml",
        filenames: "",
        shebangs: "",
        line_comment: "#",
        block_comment: None,
        indent_width: 2,
        grammar: Some(|| tree_sitter_yaml::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "bash",
        name: "Shell",
        extensions: "sh bash zsh ksh",
        filenames: ".bashrc .zshrc .profile",
        shebangs: "sh bash zsh ksh",
        line_comment: "#",
        block_comment: None,
        indent_width: 2,
        grammar: Some(|| tree_sitter_bash::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "go",
        name: "Go",
        extensions: "go",
        filenames: "",
        shebangs: "",
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
        indent_width: 4,
        grammar: Some(|| tree_sitter_go::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "c",
        name: "C",
        extensions: "c h",
        filenames: "",
        shebangs: "",
        line_comment: "//",
        block_comment: Some(("/*", "*/")),
        indent_width: 4,
        grammar: Some(|| tree_sitter_c::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "css",
        name: "CSS",
        extensions: "css scss",
        filenames: "",
        shebangs: "",
        line_comment: "",
        block_comment: Some(("/*", "*/")),
        indent_width: 2,
        grammar: Some(|| tree_sitter_css::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "html",
        name: "HTML",
        extensions: "html htm",
        filenames: "",
        shebangs: "",
        line_comment: "",
        block_comment: Some(("<!--", "-->")),
        indent_width: 2,
        grammar: Some(|| tree_sitter_html::LANGUAGE.into()),
    },
    LanguageInfo {
        id: "markdown",
        name: "Markdown",
        extensions: "md markdown",
        filenames: "README LICENSE.md",
        shebangs: "",
        line_comment: "",
        block_comment: Some(("<!--", "-->")),
        indent_width: 2,
        grammar: none(),
    },
    LanguageInfo {
        id: "text",
        name: "Text",
        extensions: "",
        filenames: "",
        shebangs: "",
        line_comment: "",
        block_comment: None,
        indent_width: 4,
        grammar: none(),
    },
];

/// Look up by config id.
pub fn by_id(id: &str) -> Option<&'static LanguageInfo> {
    LANGUAGES.iter().find(|l| l.id == id)
}

/// The fallback language for unknown files.
pub fn plain_text() -> &'static LanguageInfo {
    LANGUAGES
        .iter()
        .find(|l| l.id == "text")
        .expect("text language is always registered")
}

/// Detect a language from a path, then from the first line (shebang).
pub fn detect(path: Option<&Path>, first_line: Option<&str>) -> &'static LanguageInfo {
    if let Some(path) = path {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(lang) = LANGUAGES.iter().find(|l| l.covers_filename(name)) {
                return lang;
            }
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(lang) = LANGUAGES.iter().find(|l| l.covers_extension(ext)) {
                return lang;
            }
        }
    }
    if let Some(line) = first_line {
        if let Some(lang) = detect_shebang(line) {
            return lang;
        }
    }
    plain_text()
}

fn detect_shebang(line: &str) -> Option<&'static LanguageInfo> {
    // Callers pass the head of the file; only the first line is a shebang.
    let line = line.lines().next().unwrap_or(line);
    let line = line.strip_prefix("#!")?.trim();
    let mut words = line.split_whitespace();
    let mut program = words.next()?;
    // `#!/usr/bin/env python3` — the interpreter is the argument to env.
    if program.ends_with("env") {
        program = words.next()?;
    }
    program = program.rsplit('/').next().unwrap_or(program);
    LANGUAGES
        .iter()
        .find(|l| l.shebangs.split_whitespace().any(|s| program == s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_extension_and_name() {
        assert_eq!(detect(Some(Path::new("a/b/main.rs")), None).id, "rust");
        assert_eq!(detect(Some(Path::new("Cargo.toml")), None).id, "toml");
        assert_eq!(detect(Some(Path::new("weird.xyz")), None).id, "text");
    }

    #[test]
    fn detects_shebang() {
        assert_eq!(
            detect(None, Some("#!/usr/bin/env python3\nprint(1)")).id,
            "python"
        );
        assert_eq!(detect(None, Some("#!/bin/bash\n")).id, "bash");
        assert_eq!(detect(None, Some("# just a comment")).id, "text");
    }

    #[test]
    fn every_grammar_loads() {
        for lang in LANGUAGES {
            if let Some(grammar) = lang.grammar() {
                let mut parser = tree_sitter::Parser::new();
                parser
                    .set_language(&grammar)
                    .unwrap_or_else(|e| panic!("{} grammar: {e}", lang.id));
            }
        }
    }
}

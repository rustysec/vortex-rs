//! Spell checking data files: keys are code, values and comments are prose.

use std::path::PathBuf;

use vortex::buffer::{Buffer, BufferId};
use vortex::config::{EditorConfig, SpellConfig};
use vortex::selection::Span;

fn flagged(name: &str, text: &str) -> Vec<String> {
    let mut buffer = Buffer::new(
        BufferId(1),
        Some(PathBuf::from(name)),
        text.to_string(),
        &EditorConfig::default(),
    );
    assert!(buffer.reparse_now(), "{name} should parse");
    let spans = vortex::syntax::comment_and_string_spans(&buffer, Span::new(0, buffer.len_bytes()));
    vortex::spell::check_scoped(text, &SpellConfig::default(), Some(&spans))
        .into_iter()
        .map(|diagnostic| text[diagnostic.range.start..diagnostic.range.end].to_string())
        .collect()
}

#[test]
fn json_keys_are_not_spell_checked() {
    let found = flagged("sample.json", "{\"coment\": \"mispell\"}\n");
    assert_eq!(found, ["mispell"], "only the string value is prose");
}

#[test]
fn yaml_keys_are_not_spell_checked() {
    let found = flagged(
        "sample.yaml",
        "coment: mispell\nflow: {coment: mispell}\n# anoter coment\n",
    );
    assert_eq!(
        found,
        ["mispell", "mispell", "anoter", "coment"],
        "values and comments are prose, keys are not"
    );
}

#[test]
fn toml_quoted_keys_are_not_spell_checked() {
    let found = flagged("sample.toml", "\"coment\" = \"mispell\"\n# anoter\n");
    assert_eq!(found, ["mispell", "anoter"]);
}

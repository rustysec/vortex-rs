//! Open real files from the repository and push them through the paths that a
//! session would: parse, render, spell-check.
//!
//! These are the checks that catch byte/char mistakes on real text — unit tests
//! with tidy ASCII fixtures never do.

use std::path::{Path, PathBuf};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use vortex::buffer::{Buffer, BufferId, Change, ChangeGroup};
use vortex::config::{Config, EditorConfig, SpellConfig};
use vortex::editor::Editor;
use vortex::selection::Selection;
use vortex::status::Snapshot;
use vortex::theme::Theme;
use vortex::ui::{self, Screen};

fn repo_files() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![
        root.join("README.md"),
        root.join("config.example.toml"),
        root.join("plan/knits.md"),
        root.join("plan/init.md"),
    ];
    for dir in ["src", "src/input", "src/lsp", "scripts"] {
        let entries = std::fs::read_dir(root.join(dir)).expect("source dir");
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}

/// The full checker (dictionary, suggestions) is slower, so it runs on the
/// files with the most prose plus one piece of code.
#[test]
fn spell_checking_survives_real_text() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in [
        "README.md",
        "plan/init.md",
        "src/editor.rs",
        "config.example.toml",
    ] {
        let text = std::fs::read_to_string(root.join(name)).expect("readable");
        let found = vortex::spell::check(&text, &SpellConfig::default());
        for diagnostic in &found {
            // A diagnostic that does not point at real text is worse than none.
            let span = diagnostic.range;
            assert!(
                span.end <= text.len() && span.start <= span.end,
                "{name}: {:?} is outside the file",
                span
            );
            assert!(
                text.is_char_boundary(span.start) && text.is_char_boundary(span.end),
                "{name}: {:?} is not on a char boundary",
                span
            );
        }
    }
}

#[test]
fn every_repo_file_parses_renders_and_spells() {
    let files = repo_files();
    assert!(files.len() > 20, "expected the repo's own files");
    for path in files {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let text = std::fs::read_to_string(&path).expect("readable");

        // Tokenising every byte of every file: this is where byte/char mistakes
        // show up as panics, and it is fast enough to do for all of them.
        let tokens = vortex::spell::words(&text);
        assert!(
            tokens
                .iter()
                .all(|(word, byte, length)| text[*byte..*byte + *length] == *word),
            "spell token offsets are not byte-exact in {}",
            path.display()
        );

        // Parse and render, the way a frame would be built.
        let config = Config::default();
        let theme = Theme::default();
        let mut editor = Editor::new(config.clone());
        let id = editor.open_file(&path);
        if let Some(buffer) = editor.buffer_mut(id) {
            vortex::syntax::parse_in_place(buffer);
        }
        editor.show_in_pane(id, None);

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                let snapshot = Snapshot::collect(&editor, None, Path::new("."));
                let screen = Screen {
                    editor: &editor,
                    config: &config,
                    theme: &theme,
                    status: snapshot,
                    command_line: None,
                    picker: None,
                    completion: None,
                    diagnostic_float: None,
                };
                ui::draw(frame, &screen);
            })
            .unwrap_or_else(|error| panic!("rendering {name} failed: {error}"));
    }
}

/// Edit every file the way a session would — insert, delete, undo — and require
/// that undo returns the text byte for byte.  Real files are where byte offsets
/// and character counts disagree.
#[test]
fn editing_real_files_round_trips() {
    for path in repo_files() {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let original = std::fs::read_to_string(&path).expect("readable");
        if original.is_empty() {
            continue;
        }
        let mut buffer = Buffer::open(BufferId(1), &path, &EditorConfig::default()).expect("open");
        let mut selection = Selection::point(0);

        // Step through the file in small increments so that offsets land inside
        // multi-byte characters as often as not.
        let step = original.len().div_ceil(24).max(1);
        let mut typed = 0;
        for offset in (0..original.len()).step_by(step) {
            // `Command` rather than `Insert`: typing coalesces consecutive
            // inserts into a single undo step, which would hide per-offset bugs.
            buffer.apply(
                vec![Change::insert(offset, "øx")],
                ChangeGroup::Command,
                &mut selection,
            );
            typed += 1;
            assert!(
                buffer.text().contains("øx"),
                "{name}: insert at byte {offset} disappeared"
            );
        }
        for _ in 0..typed {
            buffer.undo().expect("undo one of the inserts");
        }
        assert_eq!(
            buffer.text(),
            original,
            "{name}: undo did not restore the file"
        );

        buffer.redo().expect("redo");
        assert_ne!(buffer.text(), original, "{name}: redo did nothing");
        buffer.undo().expect("undo the redo");
        assert_eq!(buffer.text(), original, "{name}: final undo missed");
    }
}

//! A `:` command must be bindable as a keymap action, with or without the
//! leading colon, and the binding must run the command rather than merely
//! pre-filling the command line.

use vortex::action::{Action, Cmd, parse_action};
use vortex::config::Config;
use vortex::input::Keymaps;

#[test]
fn command_names_are_bindable_actions() {
    // The colon form always worked at parse time.
    assert_eq!(
        parse_action(":bnext"),
        Some(Action::Cmd(Cmd::Ex("bnext".into())))
    );
    // The bare command name and its abbreviation now resolve as well.
    assert_eq!(
        parse_action("bnext"),
        Some(Action::Cmd(Cmd::Ex("bnext".into())))
    );
    assert_eq!(parse_action("bn"), Some(Action::Cmd(Cmd::Ex("bn".into()))));
    // Action names win on a clash with a command name.
    assert_eq!(parse_action("quit"), Some(Action::Cmd(Cmd::Quit)));
    // Nonsense is still nonsense.
    assert_eq!(parse_action("frobnicate"), None);
}

#[test]
fn a_bnext_keybind_builds_without_warnings() {
    let text = r#"
[keymap.normal]
"gb" = "bnext"
"<ctrl-n>" = ":bnext"
"#;
    let config: Config = toml::from_str(text).unwrap();
    assert!(config.validate().is_empty(), "{:?}", config.validate());
    let (maps, warnings) = Keymaps::build(&config);
    assert!(warnings.is_empty(), "{warnings:?}");
    use vortex::input::keymap::Lookup;
    let keys = vortex::input::parse_key_sequence("g b").unwrap();
    assert_eq!(
        maps.normal.lookup(&keys),
        Lookup::Action(Action::Cmd(Cmd::Ex("bnext".into())))
    );
}

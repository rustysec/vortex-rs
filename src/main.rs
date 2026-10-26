//! vortex — a modal, async, grammar-aware terminal code editor.
//!
//! The binary parses the command line, loads configuration, enters the terminal
//! and hands control to [`vortex::app::App`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::LazyLock;

use clap::Parser;
use vortex::action::action_names;
use vortex::app::App;
use vortex::config::Config;
use vortex::error::{Error, Result};
use vortex::term;

static VERSION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    )
});

const AFTER_HELP: &str = "\
The line number may also be written vim-style, before or after the file:
    vortex +42 src/main.rs
    vortex src/main.rs +42

Keys live under [keymap.*] in the config; `:map` inside vortex shows the active
bindings and `:help` lists the commands.  `vortex --default-config` prints the
annotated reference config (also shipped as config.example.toml).
";

/// A modal, async, grammar-aware code editor for the terminal.
#[derive(Debug, Parser)]
#[command(
    name = "vortex",
    version = VERSION.as_str(),
    about = "a modal, async, grammar-aware code editor for the terminal",
    after_help = AFTER_HELP,
    override_usage = "vortex [OPTIONS] [+LINE] [FILE]…"
)]
struct Cli {
    /// Files to open; missing files start empty so you can create them.
    ///
    /// A name starting with `+` is only a line number when all digits, so a
    /// file called `+notes` still works.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,

    /// Line to put the cursor on in the first file (vim's `+LINE`).
    #[arg(short = 'L', long, value_name = "LINE")]
    line: Option<usize>,

    /// Use this configuration file instead of ~/.config/vortex/config.toml.
    #[arg(short = 'c', long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Write the documented starter config and exit (PATH defaults to the config dir).
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
    write_config: Option<String>,

    /// Print the documented default config and exit.
    #[arg(long)]
    default_config: bool,

    /// Validate the configuration and exit non-zero if it cannot be applied.
    #[arg(long)]
    check_config: bool,

    /// Print every bindable action name and exit.
    #[arg(long)]
    list_actions: bool,

    /// Do not capture the mouse (scroll wheel and clicks stay with the terminal).
    #[arg(long)]
    no_mouse: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("vortex: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse_from(expand_line_arguments(std::env::args()));

    if cli.list_actions {
        let mut names = action_names();
        names.sort_unstable();
        for name in names {
            println!("{name}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    if cli.default_config {
        print!("{}", Config::default_config_text());
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(target) = cli.write_config.as_deref() {
        let path = if target.is_empty() {
            Config::default_path().ok_or_else(|| {
                Error::Config("no configuration directory found (set $XDG_CONFIG_HOME)".into())
            })?
        } else {
            PathBuf::from(target)
        };
        let path = write_starter_config(path)?;
        println!("wrote {}", path.display());
        return Ok(ExitCode::SUCCESS);
    }
    if cli.check_config {
        return check_config(cli.config.as_deref());
    }

    let (mut config, warnings, config_path) = load_config(cli.config.as_deref())?;
    if cli.no_mouse {
        config.editor.mouse = false;
    }

    // Every file argument becomes a buffer; `+123` positions the first one.
    term::install_panic_hook();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("vortex-async")
        .build()
        .map_err(|error| Error::Terminal(format!("cannot start async runtime: {error}")))?;

    let exit = runtime.block_on(async move {
        let mut app = match App::start(config, config_path, &cli.files, cli.line) {
            Ok(app) => app,
            Err(error) => {
                eprintln!("vortex: {error}");
                return ExitCode::FAILURE;
            }
        };
        for warning in &warnings {
            app.set_startup_warning(warning.clone());
        }
        let result = app.run().await;
        app.shutdown();
        match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("vortex: {error}");
                ExitCode::FAILURE
            }
        }
    });
    Ok(exit)
}

/// Report the configuration that would be used, and whether it is usable.
fn check_config(explicit: Option<&Path>) -> Result<ExitCode> {
    let (config, warnings) = Config::load(explicit)?;
    let path = explicit
        .map(Path::to_path_buf)
        .or_else(Config::default_path)
        .map(|path| {
            let display = path.display().to_string();
            if path.exists() {
                display
            } else {
                format!("{display} (not present, defaults in use)")
            }
        })
        .unwrap_or_else(|| "<defaults>".to_string());
    println!("config: {path}");
    for warning in &warnings {
        println!("warning: {warning}");
    }
    // An option that cannot be applied through `:set` is broken even if it
    // deserialised, so check a few of them end to end.
    let mut probe = config.clone();
    let mut problems = Vec::new();
    for (key, value) in [
        ("editor.tab-width", "4"),
        ("status.height", "1"),
        ("layout.split-mode", "auto"),
    ] {
        if let Err(error) = probe.set_value(key, value) {
            problems.push(format!("{key}={value}: {error}"));
        }
    }
    for problem in &problems {
        println!("error: {problem}");
    }
    if warnings.is_empty() && problems.is_empty() {
        println!("all options look fine");
    }
    Ok(if problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// Rewrite vim's `+42` into `--line 42`, which is what clap can express.
///
/// Only an all-digit `+` argument is a line directive: `+notes` stays a file
/// name, and nothing after `--` is touched.
fn expand_line_arguments(argv: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut expanded = Vec::new();
    let mut literal = false;
    for argument in argv {
        if literal {
            expanded.push(argument);
            continue;
        }
        match argument.as_str() {
            "--" => {
                literal = true;
                expanded.push(argument);
            }
            other
                if other.len() > 1
                    && other.starts_with('+')
                    && other[1..].bytes().all(|b| b.is_ascii_digit()) =>
            {
                expanded.push("--line".to_string());
                expanded.push(other[1..].to_string());
            }
            other => expanded.push(other.to_string()),
        }
    }
    expanded
}

/// Load configuration, reporting unknown keys as warnings instead of failing.
fn load_config(explicit: Option<&Path>) -> Result<(Config, Vec<String>, Option<PathBuf>)> {
    let path = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => Config::default_path().filter(|path| path.exists()),
    };
    let (config, warnings) = Config::load(path.as_deref())?;
    Ok((config, warnings, path))
}

fn write_starter_config(path: PathBuf) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.exists() {
        return Err(Error::Config(format!(
            "{} already exists (nothing was overwritten)",
            path.display()
        )));
    }
    let mut file = std::fs::File::create(&path)?;
    file.write_all(Config::default_config_text().as_bytes())?;
    file.flush()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Cli {
        let owned: Vec<String> = std::iter::once("vortex".to_string())
            .chain(argv.iter().map(|arg| arg.to_string()))
            .collect();
        Cli::try_parse_from(expand_line_arguments(owned)).expect("should parse")
    }

    #[test]
    fn vim_line_syntax_becomes_an_option() {
        let cli = parse(&["+42", "src/main.rs"]);
        assert_eq!(cli.line, Some(42));
        assert_eq!(cli.files, [PathBuf::from("src/main.rs")]);

        // Position does not matter, and a count is just a line number.
        let cli = parse(&["src/lib.rs", "+7"]);
        assert_eq!(cli.line, Some(7));
        assert_eq!(cli.files, [PathBuf::from("src/lib.rs")]);
    }

    #[test]
    fn a_plus_file_name_is_still_a_file() {
        let cli = parse(&["+notes"]);
        assert_eq!(cli.line, None);
        assert_eq!(cli.files, [PathBuf::from("+notes")]);

        // After `--` nothing is a flag, exactly as people expect from `--`.
        let cli = parse(&["--", "+9"]);
        assert_eq!(cli.line, None);
        assert_eq!(cli.files, [PathBuf::from("+9")]);
    }

    #[test]
    fn flags_carry_their_values() {
        let cli = parse(&["-c", "/tmp/vortex.toml", "--no-mouse", "a.rs", "b.rs"]);
        assert_eq!(cli.config.as_deref(), Some(Path::new("/tmp/vortex.toml")));
        assert!(cli.no_mouse);
        assert_eq!(cli.files.len(), 2);

        let cli = parse(&["--check-config"]);
        assert!(cli.check_config);

        // `--write-config` takes its path only when one is given.
        let cli = parse(&["--write-config"]);
        assert_eq!(cli.write_config.as_deref(), Some(""));
        let cli = parse(&["--write-config", "/tmp/v.toml"]);
        assert_eq!(cli.write_config.as_deref(), Some("/tmp/v.toml"));
    }

    #[test]
    fn unknown_options_are_rejected_with_usage() {
        let owned: Vec<String> = ["vortex", "--nonsense"]
            .into_iter()
            .map(String::from)
            .collect();
        let error =
            Cli::try_parse_from(expand_line_arguments(owned)).expect_err("should not parse");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}

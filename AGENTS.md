# AGENTS.md

Notes for agents (and humans) working in this repo. The user-facing story is in
`README.md`; the architecture map lives under "Architecture" there.

## Always pass `cargo fmt --all` before committing

Formatting is checked in CI (`cargo fmt --all --check`, see
`.github/workflows/ci.yml`), so a commit that is not formatted breaks the build
for everyone who pushes after it.

Before every commit:

```sh
cargo fmt --all && cargo fmt --all --check
```

Rules of thumb:

* Run `cargo fmt --all`, not `cargo fmt` — `--all` covers the lib, the binary,
  `tests/` and any example, so a stray test file cannot ride in unformatted.
* Never commit hand-formatted code on the theory that it "reads better this
  way". `rustfmt.toml` is the style: edition 2024, `max_width = 100`, Unix
  newlines. If a line has to be reshaped by hand (a long match arm, a deeply
  nested call), restructure it until rustfmt agrees instead of fighting it.
* Do not add unstable rustfmt options to `rustfmt.toml` to get the layout you
  want. The toolchain is pinned in `rust-toolchain.toml`, an unstable option is
  ignored by a stable build, and the check then passes on one machine and fails
  on another.
* Keep formatting-only churn out of a behavioural commit when you can; if a
  rustfmt fix lands in unrelated lines, say so in the message rather than
  leaving it as an unexplained diff.

The rest of what CI runs, in the same order — run all of it before committing:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo build --release` plus the pty smoke tests (`scripts/smoke*.py`) are for
changes that touch rendering, input or the terminal; see "Development" in
`README.md`.

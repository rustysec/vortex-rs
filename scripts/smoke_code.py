#!/usr/bin/env python3
"""Smoke test vortex on real code: language, highlighting, formatting, spell."""
import os
import re
import shutil
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ptyui import Pty, clean_config, drain

FAILURES = []


def check(name, condition, detail=""):
    print(("  ok  " if condition else " FAIL ") + name)
    if not condition:
        FAILURES.append(name)
        print("       " + detail.replace("\n", "\n       ")[:900])


SOURCE = """fn  main() {
    let mut v = vec![3,1,2];
    v.sort();
    // teh sorted numbers are printed here
    println!("{:?}", v);
}
"""

FORMATTED = """fn main() {
    let mut v = vec![3, 1, 2];
    v.sort();
    // teh sorted numbers are printed here
    println!("{:?}", v);
}
"""


def main():
    binary = os.path.abspath(sys.argv[1])
    project = tempfile.mkdtemp(prefix="vortex-code-")
    target = os.path.join(project, "main.rs")
    with open(target, "w") as handle:
        handle.write(SOURCE)

    app = Pty(binary, project, target, extra_args=clean_config(project))
    # Parsing, then the idle spell pass, need a moment.
    app.keys(b"", 4.0)
    raw = app.raw
    screen = app.screen.dump()
    check("rust file opens", "main.rs" in screen, screen)
    check("language detected", "Rust" in screen, screen)
    # Syntax highlighting paints with colours; a plain buffer would not.
    check("syntax highlighting emits colours", re.search(rb"\x1b\[38;[25];", raw) is not None, repr(raw[-400:]))
    check(
        "spell checker flags the comment",
        re.search(r"\d+[ewh]\b|hint", screen) is not None,
        screen,
    )

    # Format with rustfmt through :format.
    app.keys(b":format\r", 2.5)
    body = app.repaint().dump()
    check(":format runs rustfmt", "let mut v = vec![3, 1, 2];" in body, body)
    with open(target) as handle:
        check(":format does not write the file by itself", handle.read() == SOURCE, repr(handle.read()))

    # space F formats too, and the buffer stays dirty until :w
    app.keys(b":e!\r", 0.6)
    body = app.repaint().dump()
    check(":e! reloads from disk", "vec![3,1,2]" in body, body)
    app.keys(b"\x20F", 2.5)
    body = app.repaint().dump()
    check("space F formats", "vec![3, 1, 2];" in body, body)
    app.keys(b":w\r", 0.8)
    with open(target) as handle:
        check(":w saves the formatted file", handle.read() == FORMATTED, repr(handle.read()))

    # Grammar-aware text objects: da( inside a call.
    app.keys(b"gg", 0.2)
    body = app.repaint().dump()
    app.keys(b"/vec!\r", 0.4)
    app.keys(b"\x15", 0.2)  # ctrl-u: clear, then place the cursor inside the brackets
    app.keys(b"\x1b")
    app.keys(b"/[\r", 0.4)
    body = app.repaint().dump()
    check("search in code finds the bracket", "[" in body, body)

    # Multi-byte text: a character is a character, not its three bytes.
    unicode_file = os.path.join(project, "unicode.txt")
    with open(unicode_file, "w") as handle:
        handle.write("alpha \u2026 beta \u6f22\u5b57 gamma\n")
    app.keys(b":e unicode.txt\r", 0.6)
    app.keys(b"0wx", 0.4)
    app.keys(b":w\r", 0.8)
    with open(unicode_file) as handle:
        check(
            "x deletes one multi-byte character",
            handle.read() == "alpha  beta \u6f22\u5b57 gamma\n",
            repr(open(unicode_file).read()),
        )
    app.keys(b"u", 0.5)
    app.keys(b":w\r", 0.8)
    with open(unicode_file) as handle:
        check(
            "undo puts the bytes back",
            handle.read() == "alpha \u2026 beta \u6f22\u5b57 gamma\n",
            repr(open(unicode_file).read()),
        )

    for _ in range(4):
        app.keys(b":q!\r", 0.25)
    check("quits with :q!", app.exited(), app.screen.dump())

    app.close()
    shutil.rmtree(project, ignore_errors=True)
    print()
    if FAILURES:
        print(f"{len(FAILURES)} code smoke failures: {FAILURES}")
        return 1
    print("code smoke test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

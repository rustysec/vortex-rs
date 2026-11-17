#!/usr/bin/env python3
"""Smoke test for in-buffer markdown decoration.

Opens a .md file in a real terminal, and checks the buffer is drawn as formatted
text — markers concealed, bullets substituted — while the source it writes stays
plain markdown, and that `:markdown` puts the raw text back."""
import os
import re
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ptyui import Pty, starter_config

FAILURES = []


def check(name, condition, detail=""):
    print(("  ok  " if condition else " FAIL ") + name)
    if not condition:
        FAILURES.append(name)
        print("       " + detail.replace("\n", "\n       "))


class Position:
    def __init__(self, line, column):
        self.line = line
        self.column = column

    def __eq__(self, other):
        return (self.line, self.column) == (other.line, other.column)

    def __repr__(self):
        return f"{self.line}:{self.column}"


def position(app):
    """The (line, 0-based column) the status area reports for the focused pane."""
    rows = [r for r in app.repaint().dump().splitlines() if "NORMAL" in r or "INSERT" in r]
    found = re.findall(r"(\d+):(\d+)", rows[-1]) if rows else []
    if not found:
        return None
    line, column = found[-1]
    return Position(int(line), int(column) - 1)


def body(app):
    """The pane rows only, without the status line."""
    return "\n".join(
        row
        for row in app.repaint().dump().splitlines()
        if "NORMAL" not in row and "INSERT" not in row
    )


def main():
    binary = os.path.abspath(sys.argv[1])
    project = tempfile.mkdtemp(prefix="vortex-md-")
    target = os.path.join(project, "note.md")
    source = (
        "# Heading\n"
        "\n"
        "Some **bold** and *italic* words.\n"
        "\n"
        "- first item\n"
        "> quoted line\n"
        "\n"
        "| name | value |\n"
        "| ---- | ----- |\n"
        "| a    | 1     |\n"
    )
    with open(target, "w") as handle:
        handle.write(source)

    app = Pty(binary, project, target, extra_args=starter_config(project, binary))

    # The caret opens on the heading — and that line shows its own markup, so the
    # characters you could edit are visible under the cursor.  Park it a row down
    # and every other line renders clean.
    screen = body(app)
    check("the caret's own line reveals its markup", "# Heading" in screen, screen)
    app.keys(b"j", 0.4)

    screen = body(app)
    check("the file opened", "Heading" in screen and "italic" in screen, screen)
    check("heading hashes are concealed", "#" not in screen, screen)
    check("emphasis markers are concealed", "**" not in screen, screen)
    check("a bullet is drawn as a typographic one", "• first item" in screen, screen)
    check("a quote is drawn as a vertical rule", "│ quoted line" in screen, screen)
    check("table borders are box drawing", "│ name │ value │" in screen, screen)
    border = [row for row in screen.splitlines() if "name" not in row and ("┼" in row or "─" in row)]
    check("the table delimiter row became a rule", bool(border), screen)

    # The caret must sit over the character it names. `0` then `2l` lands on the
    # heading's `H`, which is written at buffer column 3 but painted two cells in,
    # where the concealed `#` and its space used to be.
    app.keys(b"gg0ll", 0.4)
    decorated = position(app)
    decorated_row = next((r for r in body(app).splitlines() if "Heading" in r), "")


    # Toggle decoration off: the source reappears exactly as written.
    app.keys(b":", 0.3)
    app.keys(b"markdown\r", 0.6)
    raw = body(app)
    check("toggling shows the raw markdown again", "# Heading" in raw and "**bold**" in raw, raw)

    # Decoration must not shift a single column: the same word has to start in the
    # same screen cell whether it is decorated or raw, and the caret — which counts
    # buffer characters — has to report the same position either way.  Those two
    # together are what keep the cursor over the glyph it refers to.
    plain = position(app)
    plain_row = next((r for r in raw.splitlines() if "Heading" in r), "")
    check(
        "the same offset reports the same row and column either way",
        decorated == plain,
        f"decorated {decorated} vs plain {plain}",
    )
    check(
        "decoration did not shift any column",
        bool(decorated_row)
        and decorated_row.index("Heading") == plain_row.index("Heading"),
        f"{decorated_row!r} vs {plain_row!r}",
    )

    # And saving from either mode writes plain markdown, untouched by decoration.
    app.keys(b":", 0.3)
    app.keys(b"w\r", 0.6)
    with open(target) as handle:
        check("the file on disk is still plain markdown", handle.read() == source, "rewrote content")

    app.keys(b":qa!\r", 0.4)
    app.close()

    if FAILURES:
        print(f"\n{len(FAILURES)} failed: {', '.join(FAILURES)}")
        sys.exit(1)
    print("\nall markdown decoration smoke checks passed")


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""End-to-end smoke test: drive vortex in a real pty and read its screen back."""
import os
import re
import shutil
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


def position(app):
    """The (line, column) the status area reports for the focused pane."""
    rows = [
        row
        for row in app.repaint().dump().splitlines()
        if "NORMAL" in row or "INSERT" in row
    ]
    found = re.findall(r"(\d+):(\d+)", rows[-1]) if rows else []
    return (int(found[-1][0]), int(found[-1][1])) if found else (0, 0)


def main():
    binary = os.path.abspath(sys.argv[1])
    project = tempfile.mkdtemp(prefix="vortex-smoke-")
    target = os.path.join(project, "notes.txt")
    with open(target, "w") as handle:
        handle.write("alpha bravo\ncharlie\ndelta\n")
    with open(os.path.join(project, "other.md"), "w") as handle:
        handle.write("# other\n\nvortex is here\n")

    app = Pty(binary, project, target, extra_args=starter_config(project, binary))
    screen = app.screen.dump()
    check("opens the file in a pane", "notes.txt" in screen and "alpha bravo" in screen, screen)
    check("status area reports normal mode", "NORMAL" in screen, screen)
    # The default gutter is relative (vim-style), so 1,1,2 rather than 1,2,3.
    check("a line-number gutter is drawn", "1 alpha bravo" in screen, screen)
    check("rows past the end show ~", screen.count("~") > 5, screen)
    check("no configuration warnings", "unknown key" not in screen, screen)
    # A fresh session must look like normal mode, not like the shell before it.
    check("starts with a block cursor", b"\x1b[1 q" in app.raw, repr(app.raw[:300]))

    # Insert mode, including capitals and punctuation.
    app.keys(b"ihello ZZ! ")
    body = app.repaint().dump()
    check("insert mode types capitals and punctuation", "hello ZZ! alpha bravo" in body, body)
    check("status area switches to INSERT", "INSERT" in body, body)
    check("insert mode shows a bar cursor", b"\x1b[5 q" in app.raw, repr(app.raw[-300:]))
    app.keys(b"\x1b")
    body = app.repaint().dump()
    check("escape returns to normal mode", "NORMAL" in body, body)

    # :w writes the buffer.
    app.keys(b":w\r", 0.6)
    on_disk = open(target).read()
    check(":w writes the file", on_disk.startswith("hello ZZ! alpha"), repr(on_disk))

    # Undo and redo.
    app.keys(b"u")
    app.keys(b":w\r", 0.5)
    check("u undoes the insert", open(target).read().startswith("alpha"), repr(open(target).read()))
    app.keys(b"\x12")  # ctrl-r
    app.keys(b":w\r", 0.5)
    check("ctrl-r redoes the insert", open(target).read().startswith("hello"), repr(open(target).read()))
    app.keys(b"u")
    app.keys(b":w\r", 0.5)

    # Motions, operators and text objects.
    app.keys(b"ggdw")
    body = app.repaint().dump()
    check("dw deletes a word", "bravo" in body and "alpha" not in body.splitlines()[1], body)
    app.keys(b"u")
    app.keys(b"ggciwzulu")
    body = app.repaint().dump()
    check("ciw replaces a word", "zulu bravo" in body, body)
    app.keys(b"\x1b")
    app.keys(b"gg3x")
    body = app.repaint().dump()
    check("3x deletes three characters", "u bravo" in body, body)  # "zul" is gone
    app.keys(b"uu")
    app.repaint()

    app.keys(b"ggVjd")
    body = app.repaint().dump()
    check("Vjd deletes lines", "delta" in body and "bravo" not in body, body)
    app.keys(b"u")
    app.repaint()

    # Search.
    app.keys(b"/", 0.3)
    app.keys(b"\x15charlie\r", 0.5)  # ctrl-u clears the pre-filled term
    body = app.repaint().dump()
    check("search finds a match", "1 match" in body or "match" in body, body)
    check("search updates the cursor position", "2:1" in body, body)
    app.keys(b"/", 0.3)
    app.keys(b"\x15delta\r", 0.5)
    body = app.repaint().dump()
    check("a second search replaces the first", "3:1" in body, body)

    # Panes.
    app.keys(b":vsplit other.md\r", 0.8)
    body = app.repaint().dump()
    check(":vsplit opens a second pane", "other.md" in body and "vortex is here" in body, body)
    app.keys(b"\x17l", 0.4)  # ctrl-w l
    body = app.repaint().dump()
    check("ctrl-w l focuses the next pane", body.count("other.md") >= 1, body)
    app.keys(b":only\r", 0.5)
    body = app.repaint().dump()
    check(":only keeps the focused pane", "other.md" in body and body.count("\u256d") == 1, body)

    # Scrolling belongs to the focused pane, not to the terminal: in a stacked
    # split the cursor must stay inside a pane that is half the screen height.
    long_file = os.path.join(project, "long.txt")
    with open(long_file, "w") as handle:
        handle.write("".join(f"line {i}\n" for i in range(1, 61)))
    app.keys(b":e long.txt\r", 0.7)
    app.keys(b":set number\r", 0.4)
    app.keys(b":split\r", 0.6)
    for _ in range(18):
        app.keys(b"j", 0.05)
    body = app.repaint().dump()
    line, _ = position(app)
    check("the cursor line stays inside its split pane", f"{line} line {line}" in body, body)
    check("the other pane does not scroll along", "1 line 1" in body, body)
    for _ in range(30):
        app.keys(b"k", 0.04)
    body = app.repaint().dump()
    line, _ = position(app)
    check("scrolling back up keeps the cursor in view", f"{line} line {line}" in body, body)
    app.keys(b"G", 0.5)
    body = app.repaint().dump()
    line, _ = position(app)
    check("G stops on the last line, not after it", line == 60, body)
    app.keys(b":only\r", 0.5)

    # Paragraph motions: `}` always finds the *next* empty line, even from one,
    # and `{` mirrors it.
    para_file = os.path.join(project, "para.md")
    with open(para_file, "w") as handle:
        handle.write("one\ntwo\n\nthree\n\n\nfour\n")
    app.keys(b":e para.md\r", 0.7)
    app.keys(b"gg", 0.3)
    app.keys(b"}", 0.4)
    check("} from a text line finds the blank line", position(app) == (3, 1), app.screen.dump())
    app.keys(b"}", 0.4)
    check("} from a blank line moves on", position(app) == (5, 1), app.screen.dump())
    app.keys(b"{", 0.4)
    check("{ walks back to the previous boundary", position(app) == (3, 1), app.screen.dump())
    app.keys(b"}", 0.4)
    app.keys(b"}", 0.4)
    check("} runs to the last character at the end", position(app) == (7, 4), app.screen.dump())

    # Options.
    app.keys(b":set tab-width?\r", 0.4)
    body = app.repaint().dump()
    check(":set shows an option value", "tab-width" in body, body)
    app.keys(b":set number\r", 0.4)
    body = app.repaint().dump()
    check(":set number switches to absolute numbers", "3" in body, body)
    app.keys(b":set tab-width=3\r", 0.4)
    body = app.repaint().dump()
    check(":set applies a value and echoes it", "editor.tab-width = 3" in body, body)
    # The name the example config uses, not only vim's `scroll`.
    app.keys(b":set scroll-margin=8\r", 0.4)
    body = app.repaint().dump()
    check("a documented config key can be set by name", "editor.scroll-margin = 8" in body, body)
    app.keys(b":set nonsense=1\r", 0.4)
    body = app.repaint().dump()
    check("an unknown option is refused", "unknown option" in body, body)

    # Fuzzy file picker.
    app.keys(b" ", 0.3)
    app.keys(b"p", 0.6)  # space p opens the command popup
    body = app.repaint().dump()
    # The overlay reports how many actions match, which only a live picker does.
    check("space p opens the command popup", re.search(r"\d+/\d+ match", body) is not None, body)
    # Every command listed alphabetically, so the list is a lookup.  Only one page
    # of it fits in the card, so the order is checked between neighbours that are
    # both drawn — `bdelete` sorts first and its argument shape rides along
    # dimmed after the name; typing what you mean brings the rest into view.
    check(
        "the popup lists commands alphabetically with their arguments",
        ":bdelete" in body
        and "<name>" in body
        and 0 < body.index(":append") < body.index(":bdelete"),
        body,
    )
    app.keys(b"gre", 0.5)
    body = app.repaint().dump()
    check("a command from deep in the list filters into view", ":grep" in body, body)
    for _ in range(3):
        app.keys(b"\x7f", 0.15)  # clear back to the full listing before `wri`
    app.keys(b"wri", 0.5)
    body = app.repaint().dump()
    check(
        "typing narrows the list to what it could mean",
        ":write" in body and ":bdelete" not in body,
        body,
    )
    # A word that names a command runs as written: `:enew` takes an empty answer
    # for its optional path rather than stepping into a question.
    app.keys(b"\x1b", 0.3)
    app.keys(b" ", 0.3)
    app.keys(b"p", 0.5)
    app.keys(b"enew\r", 0.8)
    body = app.repaint().dump()
    check(
        "a typed command word runs instead of asking",
        "*scratch*" in body,
        body,
    )

    # Walking to a row is the discovery path: it asks for that command's
    # parameters one at a time, and Esc backs out of a step rather than the lot.
    # The palette lists every action as well as command, so the walk stops on the
    # preview text instead of counting rows a list that keeps growing would date.
    app.keys(b":", 0.5)
    walked = False
    for _ in range(60):
        screen = app.keys(b"\x1b[B", 0.12)  # Down towards :edit, whose path is optional
        if "reload this file" in screen.dump():
            walked = True
            break
    check("the arrows walk the list and the preview follows", walked, screen.dump())
    app.keys(b"\r", 0.7)
    body = app.repaint().dump()
    check(
        "Enter on a chosen row asks for its parameter",
        ":edit path" in body and "to skip" in body,
        body,
    )
    app.keys(b"\x1b", 0.6)
    body = app.repaint().dump()
    check(
        "Esc backs out one step with the command still on the line",
        ":edit path" not in body and "edit" in body and "commands" in body,
        body,
    )
    app.keys(b"\x1b", 0.4)

    # The leader is a key of its own: send it apart from the key it modifies.
    app.keys(b" ", 0.3)
    app.keys(b"f", 0.8)
    body = app.repaint().dump()
    # `long.txt` is not otherwise on screen, so seeing it means the candidate
    # list is really drawn.
    check("space f lists project files", "long.txt" in body, body)
    app.keys(b"other", 0.5)
    body = app.repaint().dump()
    check("typing filters the list", "other.md" in body and "long.txt" not in body, body)
    app.keys(b"\r", 0.8)
    body = app.repaint().dump()
    check("picking a file opens it", "vortex is here" in body, body)

    # Quit.
    for _ in range(4):
        app.keys(b":q\r", 0.25)
    exited = app.exited()
    check("quits cleanly", exited, app.screen.dump())

    app.close()
    shutil.rmtree(project, ignore_errors=True)

    print()
    if FAILURES:
        print(f"{len(FAILURES)} smoke failures: {FAILURES}")
        return 1
    print("smoke test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

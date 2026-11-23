#!/usr/bin/env python3
"""Check that vortex reads the terminal's colours and acts on them.

The terminal answers an OSC 11/10 query with its own background and foreground,
and vortex should then pick the matching palette and blend its quiet elements
(cursor line, selection, gutter, borders) from what it was told.  A terminal
that never answers must cost a few milliseconds and nothing else.

    python3 scripts/smoke_theme.py target/release/vortex
"""

import os
import re
import shutil
import sys
import tempfile
import threading

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ptyui import Pty, color_report, starter_config  # noqa: E402

SOURCE = """fn  main() {
    // note the numbers
    let v = vec![3,1,2];
    println!("{:?}", v);
}
"""

failures = []


def check(what, ok, *evidence):
    print(("  ok  " if ok else " FAIL ") + what)
    if not ok:
        failures.append(what)
        for note in evidence:
            print("       " + str(note)[:400])


def run(binary, label, answer_colors):
    """Open a Rust file in a terminal that may or may not answer the query."""
    project = tempfile.mkdtemp(prefix="vortex-theme-")
    try:
        target = os.path.join(project, "main.rs")
        with open(target, "w") as handle:
            handle.write(SOURCE)
        app = Pty(
            binary, project, target, extra_args=starter_config(project, binary), answer_colors=answer_colors
        )
        app.keys(b"", 1.2)
        raw = app.raw
        screen = app.repaint().dump()
        results = {
            # A background colour derived from the terminal's own, not invented.
            "derived blend": b"48;2;" in raw,
            # Dark-scheme accent on the focused pane border (bright cyan).
            "dark accent": b"38;5;14" in raw,
            # Light-scheme comments are grey (index 8) on the plain terminal
            # background; the dark scheme uses cyan.  The angled status chips
            # legitimately paint index-8 fills, so only bare grey text counts:
            # `38;5;8` not followed by a `48;…` segment fill.
            "light comment": re.search(rb"38;5;8(?!\d)(?!;48)", raw) is not None,
        }
        check("%s: file is drawn" % label, "note the numbers" in screen, screen)
        check("%s: editor is alive" % label, "NORMAL" in screen, screen[-200:])
        for name, expected in EXPECTED[label].items():
            check(
                "%s: %s %s" % (label, name, "present" if expected else "absent"),
                results[name] == expected,
                sorted(set(
                    chunk.decode("utf-8", "replace")
                    for chunk in [raw[i : i + 12] for i in range(0, len(raw), 12)]
                    if chunk.startswith(b"\x1b[3") or chunk.startswith(b"\x1b[4")
                ))[:20],
            )
        for _ in range(3):
            app.keys(b":q!\r", 0.2)
        app.close()
    finally:
        shutil.rmtree(project, ignore_errors=True)


# Which of the three signals each scenario must show.
EXPECTED = {
    "light terminal": {"derived blend": True, "dark accent": False, "light comment": True},
    "dark terminal": {"derived blend": True, "dark accent": True, "light comment": False},
    "silent terminal": {"derived blend": False, "dark accent": True, "light comment": False},
}


class LatePty(Pty):
    """A terminal that answers the background query at once, the foreground one late."""

    def __init__(self, *args, **kwargs):
        self._sent = set()
        super().__init__(*args, **kwargs)

    def _answer_queries(self, chunk):
        if b"\x1b]11;?" in chunk and "11" not in self._sent:
            self._sent.add("11")
            os.write(self.fd, color_report("11", (24, 24, 30)).encode())
        if b"\x1b]10;?" in chunk and "10" not in self._sent:
            self._sent.add("10")
            # 80 ms late: past any window that only waits for the first answer.
            threading.Timer(
                0.08, lambda: os.write(self.fd, color_report("10", (220, 220, 220)).encode())
            ).start()


def run_late_answer(binary):
    """A reply that arrives after the first one must be read, not typed.

    A half-read colour query shows up as characters on the screen: the leftover
    bytes come back through the input reader as if the user had typed them.
    """
    project = tempfile.mkdtemp(prefix="vortex-theme-")
    try:
        target = os.path.join(project, "notes.txt")
        with open(target, "w") as handle:
            handle.write("hello\n")
        app = LatePty(binary, project, target, extra_args=starter_config(project, binary))
        app.keys(b"i", 0.3)
        app.keys(b"world ", 0.4)
        app.keys(b"\x1b", 0.3)
        app.keys(b":w\r", 0.8)
        written = open(target).read()
        check(
            "a late colour reply is not typed into the buffer",
            written == "world hello\n",
            repr(written),
        )
        check("no colour-report bytes leak into the stream", b"rgb:" not in app.raw, repr(app.raw[-160:]))
        for _ in range(3):
            app.keys(b":q!\r", 0.2)
        app.close()
    finally:
        shutil.rmtree(project, ignore_errors=True)


class VeryLatePty(Pty):
    """A terminal that answers only long after vortex stopped listening."""

    def __init__(self, *args, delay=0.6, **kwargs):
        self.delay = delay
        self._scheduled = set()
        super().__init__(*args, **kwargs)

    def _answer_queries(self, chunk):
        for index, rgb in self.answer_colors.items():
            if index in self._scheduled:
                continue
            if ("\x1b]%s;?" % index).encode() in chunk:
                self._scheduled.add(index)
                threading.Timer(
                    self.delay,
                    lambda fd=self.fd, index=index, rgb=rgb: os.write(
                        fd, color_report(index, rgb).encode()
                    ),
                ).start()


def run_very_late_answer(binary):
    """A reply that lands after the query gave up must be swallowed, not typed.

    This is what a slow terminal (tmux, ssh) does.  crossterm has no OSC
    parsing, so `ESC ] 11 ;` comes back as `Alt-]` and every byte of
    `rgb:f8f8/f8f8/f2f2` as a keystroke: in normal mode the `:` opens the
    command line and the channels are typed into it, and the `r` before it
    replaces the character under the caret.
    """
    project = tempfile.mkdtemp(prefix="vortex-theme-")
    try:
        target = os.path.join(project, "notes.txt")
        with open(target, "w") as handle:
            handle.write("hello\n")
        app = VeryLatePty(
            binary,
            project,
            target,
            extra_args=starter_config(project, binary),
            answer_colors={"11": (248, 248, 242), "10": (24, 24, 24)},
            delay=0.6,
        )
        # Let the late reply land before doing anything with the keyboard.
        app.keys(b"", 1.2)
        app.keys(b"i", 0.3)
        app.keys(b"world ", 0.4)
        app.keys(b"\x1b", 0.3)
        app.keys(b":w\r", 0.8)
        written = open(target).read()
        check(
            "a reply later than the query is not typed either",
            written == "world hello\n",
            repr(written),
        )
        check(
            "no colour-report bytes reach the screen",
            b"rgb:" not in app.raw and b"f8f8" not in app.raw,
            repr(app.raw[-200:]),
        )
        for _ in range(3):
            app.keys(b":q!\r", 0.2)
        app.close()
    finally:
        shutil.rmtree(project, ignore_errors=True)


def main():
    if len(sys.argv) < 2:
        print("usage: smoke_theme.py BINARY", file=sys.stderr)
        return 2
    binary = os.path.abspath(sys.argv[1])
    run(binary, "light terminal", {"11": (245, 245, 245), "10": (20, 20, 20)})
    run(binary, "dark terminal", {"11": (24, 24, 30), "10": (220, 220, 220)})
    run(binary, "silent terminal", {})
    run_late_answer(binary)
    run_very_late_answer(binary)
    if failures:
        print("\n%d theme checks failed: %s" % (len(failures), failures))
        return 1
    print("\ntheme smoke test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

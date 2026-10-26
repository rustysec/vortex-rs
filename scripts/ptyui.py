#!/usr/bin/env python3
"""A tiny terminal emulator, enough to read back vortex's screen.

Handles the sequences ratatui emits: CUP (`ESC[row;colH`), EL, SGR (ignored),
alternate screen, and the mouse/cursor setup sequences.
"""
import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import termios
import time

CUP = re.compile(r"\x1b\[(\d+);(\d+)H")
OTHER_CSI = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]")
OSC = re.compile(r"\x1b\][^\x07]*(?:\x07|\x1b\\)")


class Screen:
    def __init__(self, rows, cols):
        self.rows = rows
        self.cols = cols
        self.grid = [[" "] * cols for _ in range(rows)]

    def write(self, data):
        text = OSC.sub("", data.decode("utf-8", "replace"))
        # Walk the stream, tracking the cursor for every CUP, and printing
        # everything else at the current position.
        row, col = 0, 0
        index = 0
        while index < len(text):
            match = CUP.match(text, index)
            if match:
                row = int(match.group(1)) - 1
                col = int(match.group(2)) - 1
                index = match.end()
                continue
            if text[index] == "\x1b":
                other = OTHER_CSI.match(text, index)
                if other:
                    index = other.end()
                    continue
                index += 1
                continue
            char = text[index]
            index += 1
            if char == "\n":
                row += 1
                col = 0
                continue
            if char == "\r":
                col = 0
                continue
            if char < " ":
                continue
            if 0 <= row < self.rows and 0 <= col < self.cols:
                self.grid[row][col] = char
                col += 1
        return self

    def dump(self):
        return "\n".join("".join(row).rstrip() for row in self.grid)


def clean_config(directory):
    """Args that pin an empty config, so a test never reads ~/.config/vortex.

    A developer's own options (`wd = close-pane`, `spell enable = false`, …)
    would otherwise change editor behaviour under the test's feet.
    """
    path = os.path.join(directory, "vortex-test-config.toml")
    with open(path, "w") as handle:
        handle.write("# intentionally empty: the built-in defaults are under test\n")
    return ["--config", path]


def spawn(binary, cwd, target, extra_args=()):
    argv = [binary, *extra_args]
    if target is not None:
        argv.append(target)
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.environ["TERM"] = "xterm-256color"
        os.execv(binary, argv)
        os._exit(127)
    return pid, fd


def set_size(fd, rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


def drain(fd, seconds=0.4, on_output=None):
    out = b""
    deadline = time.time() + seconds
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], 0.05)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
            if on_output is not None:
                on_output(chunk)
    return out


def color_report(index, rgb):
    """A terminal's answer to `ESC ] <index> ; ? BEL` (OSC 10/11)."""
    r, g, b = rgb
    # 8-bit values scale to 16-bit exactly by multiplying by 257.
    return "\x1b]%s;rgb:%04x/%04x/%04x\x07" % (index, r * 257, g * 257, b * 257)


class Pty:
    def __init__(self, binary, cwd, target, rows=24, cols=80, extra_args=(),
                 answer_colors=None):
        """answer_colors: {"11": (r,g,b), "10": (r,g,b)} to answer colour queries."""
        self.rows, self.cols = rows, cols
        self.answer_colors = dict(answer_colors or {})
        self.answered = set()
        self.pid, self.fd = spawn(binary, cwd, target, extra_args)
        set_size(self.fd, rows, cols)
        self.screen = Screen(rows, cols)
        self.raw = b""
        self._drain(1.0)

    def _drain(self, seconds):
        """Read output, replying to any colour query the editor sends."""
        chunk = drain(self.fd, seconds, self._answer_queries)
        self.raw += chunk
        self.screen.write(chunk)
        return chunk

    def _answer_queries(self, chunk):
        """Reply to OSC 10/11 queries as they appear in the output stream."""
        for index, rgb in self.answer_colors.items():
            if index in self.answered:
                continue
            if ("\x1b]%s;?" % index).encode() in chunk:
                self.answered.add(index)
                os.write(self.fd, color_report(index, rgb).encode())

    def keys(self, data, settle=0.35):
        os.write(self.fd, data)
        self._drain(settle)
        return self.screen

    def repaint(self):
        """Force a full redraw by resizing twice."""
        set_size(self.fd, self.rows, self.cols - 1)
        self._drain(0.25)
        set_size(self.fd, self.rows, self.cols)
        os.kill(self.pid, signal.SIGWINCH)
        # Keep the existing grid: ratatui repaints only the cells it thinks changed.
        self._drain(0.6)
        return self.screen

    def exited(self):
        """Has the editor process exited?"""
        deadline = time.time() + 2.0
        while time.time() < deadline:
            try:
                got, _status = os.waitpid(self.pid, os.WNOHANG)
            except ChildProcessError:
                return True
            if got:
                return True
            time.sleep(0.05)
        return False

    def close(self):
        try:
            os.kill(self.pid, signal.SIGKILL)
        except Exception:
            pass
        try:
            os.close(self.fd)
        except OSError:
            pass

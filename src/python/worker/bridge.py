import json, os, sys

_user_files = {}


def is_user_file(path):
    """main.py and the modules beside it or in folders, as opposed to the standard library.

    The standard library is zipped, and its code reports the paths it was built from,
    which don't exist here. The student's files are the real files outside the install.
    """
    known = _user_files.get(path)
    if known is None:
        known = _user_files[path] = (
            path.startswith('/')
            and path != '/_bridge.py'
            and not path.startswith(sys.prefix + '/')
            and os.path.isfile(path)
        )
    return known

# Resume commands from the main thread. Must match PYTHON_CMD_* in debug.rs.
CONTINUE   = 0
STEP_OVER  = 1
STEP_INTO  = 2
STEP_OUT   = 3

MAX_REPR = 120      # repr strings longer than this are truncated with an ellipsis
MAX_CHILDREN = 50   # children shown per container before a "… N more" placeholder
MAX_DEPTH = 8       # how deep we recurse into nested containers (TODO: change to lazy)
MAX_NODES = 5000    # total nodes per pause; guards against huge/deeply nested data


def truncate_repr(value):
    try:
        s = repr(value)
    except Exception:
        return f"<unreprable {type(value).__name__}>"
    if len(s) <= MAX_REPR:
        return s
    return s[:MAX_REPR - 1] + '…'


def is_dunder(name):
    return name.startswith('__') and name.endswith('__')


def object_attrs(value):
    """Instance attributes of a custom object, or None if it has none worth showing.

    Handles both ``__dict__`` and ``__slots__`` objects, and drops dunder
    attributes so expanding an instance doesn't surface interpreter internals.
    """
    try:
        attrs = dict(vars(value))
    except TypeError:
        attrs = None

    if attrs is None:
        slots = getattr(type(value), '__slots__', None)
        if slots is None:
            return None
        if isinstance(slots, str):
            slots = (slots,)
        attrs = {}
        for name in slots:
            try:
                attrs[name] = getattr(value, name)
            except AttributeError:
                pass

    visible = {k: v for k, v in attrs.items() if not is_dunder(k)}
    return visible or None


def container(value):
    """Describe an expandable value as ``(label, pairs, total, indexed)``.

    ``pairs`` is an iterator of ``(child_name, child_value)``; ``total`` is the
    real child count (which may exceed ``MAX_CHILDREN``). Returns None for
    non-containers (scalars), which are rendered with their repr instead.
    """
    if isinstance(value, list):
        return f"list[{len(value)}]", enumerate_pairs(value), len(value), True
    if isinstance(value, tuple):
        return f"tuple[{len(value)}]", enumerate_pairs(value), len(value), True
    if isinstance(value, dict):
        return f"dict[{len(value)}]", mapping_pairs(value), len(value), False

    attrs = object_attrs(value)
    if attrs is not None:
        label = f"{type(value).__name__}[{len(attrs)}]"
        return label, mapping_pairs(attrs), len(attrs), False

    return None


def enumerate_pairs(seq):
    for i, item in enumerate(seq):
        yield f'[{i}]', item


def mapping_pairs(mapping):
    for key, item in mapping.items():
        yield (key if isinstance(key, str) else repr(key)), item


class _Shared:
    """Variable formatting, the debug channel and the pause payload, for both backends."""

    def _open_channel(self):
        self._dbg = open('/__debug__', 'r+b', buffering=0)
        self._nodes = 0

    # ── variable formatting ──────────────────────────────────────────────────
    #
    # On every pause we serialize each frame's locals into a fully-expanded tree
    # (down to MAX_DEPTH / MAX_NODES) and ship it inline with the pause payload.
    # The UI then walks that tree without ever calling back into the worker, so
    # expanding a variable never blocks the main thread.

    def _format(self, name, value, depth):
        self._nodes += 1
        described = container(value)
        if described is None:
            return {"name": name, "value": truncate_repr(value)}

        label, pairs, total, indexed = described
        node = {"name": name, "value": label}
        if indexed:
            node["indexed"] = True
        # A container past the depth/node budget becomes a leaf: its summary
        # (e.g. "list[5]") still tells the student what it holds.
        if depth < MAX_DEPTH and self._nodes < MAX_NODES:
            node["children"] = self._children(pairs, total, depth)
        return node

    def _children(self, pairs, total, depth):
        children = []
        for i, (name, value) in enumerate(pairs):
            if i >= MAX_CHILDREN:
                children.append({"name": "…", "value": f"{total - MAX_CHILDREN} more"})
                break
            if self._nodes >= MAX_NODES:
                children.append({"name": "…", "value": "…"})
                break
            children.append(self._format(name, value, depth + 1))
        return children

    # ── debug channel ────────────────────────────────────────────────────────

    def _read_response(self):
        data = b''
        while True:
            chunk = self._dbg.read(512)
            if not chunk:
                break
            data += chunk
        if not data:
            return None
        return json.loads(data.decode())

    def _send_pause(self, frame, reason):
        """Ships the stack and locals, then blocks until the main thread answers."""
        self._nodes = 0
        stack = []
        f = frame
        while f is not None:
            name = f.f_code.co_name
            path = f.f_code.co_filename
            if name == '<module>' and path == '/main.py':
                # Module scope — not the same as a function named `main`.
                display = '__main__'
            else:
                display = name
            stack.append({
                "file": path,
                "line": f.f_lineno,
                "function": display,
                "user": is_user_file(path),
                "locals": [
                    self._format(n, v, 0)
                    for n, v in f.f_locals.items()
                    if not is_dunder(n)
                ],
            })
            f = f.f_back

        pause = json.dumps({
            "reason": reason,
            "frames": stack,
        }, separators=(',', ':')).encode()
        self._dbg.write(pause)

        resp = self._read_response()
        if resp is None:
            raise RuntimeError("debugger channel closed while paused")
        return resp




def _bdb_bridge():
    """The settrace backend, built on first use so 3.12+ never imports bdb (and inspect)."""
    import bdb

    class BdbBridge(_Shared, bdb.Bdb):
        """sys.settrace backend, for Pythons before 3.12. Every line of a traced frame calls
        back into Python, so programs run several times slower under the debugger."""

        def __init__(self):
            bdb.Bdb.__init__(self)
            self._open_channel()
            self._stepping = False

        def _apply_config(self, resp):
            self._apply_breakpoints(resp.get("breakpoints", {}))
            cmd = resp["cmd"]
            if cmd == CONTINUE:
                self._stepping = False
            elif cmd in (STEP_OVER, STEP_INTO, STEP_OUT):
                self._stepping = True

        def _dispatch(self, resp, frame):
            self._apply_config(resp)
            cmd = resp["cmd"]
            if cmd == CONTINUE:
                self.set_continue()
            elif cmd == STEP_OVER:
                self.set_next(frame)
            elif cmd == STEP_INTO:
                self.set_step()
            elif cmd == STEP_OUT:
                self.set_return(frame)

        def consume_initial_config(self):
            resp = self._read_response()
            if resp is None:
                return
            self._apply_config(resp)

        def _apply_breakpoints(self, breakpoints):
            self.clear_all_breaks()
            for path, lines in breakpoints.items():
                for line in lines:
                    self.set_break(path, line)

        def user_line(self, frame):
            if not self._stepping and not self.break_here(frame):
                return
            self._pause(frame)

        def user_exception(self, frame, exc_info):
            # No pause: there are no exception breakpoints, and the standard library raises
            # and catches exceptions all the time (imports probing for optional modules, for
            # one). An uncaught exception still ends the run with its traceback.
            pass

        def _pause(self, frame):
            self._nodes = 0
            resp = self._send_pause(frame, "breakpoint" if self.break_here(frame) else "step")
            self._dispatch(resp, frame)

        def start(self, code, globals):
            self.consume_initial_config()
            self.run(code, globals)

    return BdbBridge


class MonitoringBridge(_Shared):
    """sys.monitoring backend (3.12+). While running freely, only functions that hold a
    breakpoint report line events, and every other line in them is switched off after its
    first run, so a program runs at nearly full speed until it reaches a breakpoint.
    Stepping turns line events on everywhere until the next pause."""

    def __init__(self):
        self._open_channel()
        self._mon = sys.monitoring
        self._tool = self._mon.DEBUGGER_ID
        self._breaks = {}          # path -> set of lines
        self._mode = CONTINUE
        self._depth = 0            # stack depth the step started at (over/out)
        self._target = None        # the frame a step over started in
        self._target_done = False  # it has returned (a generator may only have yielded)
        self._seen = set()         # code objects reported by PY_START
        self._user_codes = set()   # the student's code objects, armed per breakpoint
        self._lines = {}           # code -> its (start, end, line) offset ranges

    # ── arming ───────────────────────────────────────────────────────────────

    def _remember(self, code):
        """Registers a code object and the functions nested in it; arms the student's."""
        if code in self._seen:
            return
        self._seen.add(code)
        if is_user_file(code.co_filename):
            self._user_codes.add(code)
            self._arm(code)
        for const in code.co_consts:
            if isinstance(const, type(code)):
                self._remember(const)

    def _has_break(self, code):
        lines = self._breaks.get(code.co_filename)
        return bool(lines) and any(line in lines for _, _, line in code.co_lines() if line)

    def _arm(self, code):
        E = self._mon.events
        events = E.LINE | E.JUMP if self._has_break(code) else 0
        self._mon.set_local_events(self._tool, code, events)

    def _line_at(self, code, offset):
        lines = self._lines.get(code)
        if lines is None:
            lines = self._lines[code] = [(start, end, line) for start, end, line in code.co_lines()]
        for start, end, line in lines:
            if start <= offset < end:
                return line
        return None

    def _set_mode(self, mode):
        self._mode = mode
        E = self._mon.events
        stepping = E.LINE | E.JUMP | E.PY_RETURN if mode != CONTINUE else 0
        self._mon.set_events(self._tool, E.PY_START | stepping)
        for code in self._user_codes:
            self._arm(code)
        # Re-enable every event a callback switched off, so lines silenced while running
        # freely report again (and the reverse).
        self._mon.restart_events()

    # ── events ───────────────────────────────────────────────────────────────

    def _on_start(self, code, offset):
        self._remember(code)
        return self._mon.DISABLE

    def _on_jump(self, code, source, destination):
        # Like sys.settrace: jumping back to the start of the same line (a one-line loop or
        # comprehension) runs that line again, so it counts as reaching it.
        if destination > source:
            return self._mon.DISABLE
        line = self._line_at(code, destination)
        if line is None or line != self._line_at(code, source):
            return self._mon.DISABLE
        return self._on_line(code, line, depth_offset=1)

    def _on_line(self, code, line, depth_offset=0):
        on_break = line in self._breaks.get(code.co_filename, ())
        if self._mode == CONTINUE:
            if on_break:
                self._pause(sys._getframe(1 + depth_offset), "breakpoint")
                return None
            return self._mon.DISABLE
        if code not in self._user_codes:
            return self._mon.DISABLE
        frame = sys._getframe(1 + depth_offset)
        if on_break or self._mode == STEP_INTO:
            self._pause(frame, "breakpoint" if on_break else "step")
        elif self._mode == STEP_OVER and self._steps_over_to(frame):
            self._pause(frame, "step")
        elif self._mode == STEP_OUT and _depth(frame) < self._depth:
            self._pause(frame, "step")
        return None

    def _steps_over_to(self, frame):
        if frame is self._target:
            return True
        # Like bdb, stepping over a yield stays with the generator: its consumer runs
        # until the generator resumes. Once it has returned, stop in whoever called it.
        if self._target is not None and not self._target_done and \
                self._target.f_code.co_flags & _SUSPENDABLE:
            return False
        return _depth(frame) <= self._depth

    def _on_return(self, code, offset, value):
        if sys._getframe(1) is self._target:
            self._target_done = True

    # ── control ──────────────────────────────────────────────────────────────

    def _apply(self, resp, frame=None):
        # Same canonical form as bdb and code objects: absolute, from the working directory.
        self._breaks = {os.path.abspath(path): set(lines) for path, lines in resp.get("breakpoints", {}).items()}
        cmd = resp["cmd"]
        self._depth = _depth(frame) if frame is not None else 0
        self._target, self._target_done = frame, False
        self._set_mode(cmd if cmd in (STEP_OVER, STEP_INTO, STEP_OUT) else CONTINUE)

    def _pause(self, frame, reason):
        self._nodes = 0
        self._apply(self._send_pause(frame, reason), frame)

    def start(self, code, globals):
        mon, E = self._mon, self._mon.events
        mon.use_tool_id(self._tool, 'glassjar')
        mon.register_callback(self._tool, E.PY_START, self._on_start)
        mon.register_callback(self._tool, E.LINE, self._on_line)
        mon.register_callback(self._tool, E.JUMP, self._on_jump)
        mon.register_callback(self._tool, E.PY_RETURN, self._on_return)
        self._remember(code)
        resp = self._read_response()
        self._apply(resp if resp is not None else {"cmd": CONTINUE})
        try:
            exec(code, globals)
        finally:
            mon.set_events(self._tool, 0)
            mon.free_tool_id(self._tool)


# Generators and coroutines, whose frames can suspend and resume (co_flags).
_SUSPENDABLE = 0x20 | 0x80 | 0x200


def _depth(frame):
    depth = 0
    while frame is not None:
        depth += 1
        frame = frame.f_back
    return depth


if __name__ == '__main__':
    debugger = MonitoringBridge() if sys.version_info >= (3, 12) else _bdb_bridge()()
    _main = open('/main.py').read()
    debugger.start(
        compile(_main, '/main.py', 'exec'),
        {'__file__': '/main.py', '__name__': '__main__'}
    )

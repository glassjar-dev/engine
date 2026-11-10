import bdb, json, os, sys

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


class DapBridge(bdb.Bdb):
    def __init__(self):
        super().__init__()
        self._dbg = open('/__debug__', 'r+b', buffering=0)
        self._stepping = False
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
            "reason": "breakpoint" if self.break_here(frame) else "step",
            "frames": stack,
        }, separators=(',', ':')).encode()
        self._dbg.write(pause)

        resp = self._read_response()
        if resp is None:
            raise RuntimeError("debugger channel closed while paused")
        self._dispatch(resp, frame)


debugger = DapBridge()
debugger.consume_initial_config()
_main = open('/main.py').read()
debugger.run(
    compile(_main, '/main.py', 'exec'),
    {'__file__': '/main.py', '__name__': '__main__'}
)

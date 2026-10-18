"""Run local Linux checks and validate private, content-bound push evidence."""

import ctypes
import hashlib
import json
import os
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
from collections.abc import Mapping
from pathlib import Path
from types import FrameType
from typing import NoReturn, TypedDict, overload

__all__ = ["main", "run", "verify"]


class _GateError(ValueError):
    """Local acceptance or receipt validation failed."""


class _Identity(TypedDict):
    branch: str
    head: str
    tree: str
    base: str
    range: str
    outgoing_oid: str
    clean_before: bool
    clean_after: bool


ARTIFACT_ROOT = Path("/mnt/ssd/mirror-rootfs/home/obj/tmp/hymt-stop-ai-20261012")
RANGE = "main...HEAD"
GATE_COMMAND = "just pre-push-gate head"
REVIEW_COMMAND = "native review main...HEAD"
PROTECTED = {"main", "master", "dev"}


def _git(*args: str) -> str:
    return subprocess.check_output(["git", *args]).decode().strip()


def _identity() -> _Identity:
    if _git("status", "--porcelain=v1", "--untracked-files=all"):
        raise _GateError("a clean index and working tree are required")
    branch = _git("symbolic-ref", "--quiet", "--short", "HEAD")
    if branch in PROTECTED:
        raise _GateError("a checked-out feature branch is required")
    head = _git("rev-parse", "HEAD")
    return {
        "branch": branch,
        "head": head,
        "tree": _git("rev-parse", "HEAD^{tree}"),
        "base": _git("merge-base", "main", "HEAD"),
        "range": RANGE,
        "outgoing_oid": head,
        "clean_before": True,
        "clean_after": True,
    }


def _private_path(path: str | Path, directory: bool = False) -> Path:
    root = ARTIFACT_ROOT.resolve(strict=True)
    if root.stat().st_dev != Path("/mnt/ssd").stat().st_dev:
        raise _GateError("artifact root is not on the verified SSD")
    # A designated directory alias may resolve for validation. No path/link is
    # rewritten, and both its logical and resolved ancestor chains must be safe.
    for ancestor in {*ARTIFACT_ROOT.parents, *root.parents}:
        link = ancestor.lstat()
        if stat.S_ISLNK(link.st_mode) and link.st_uid not in (0, os.getuid()):
            raise _GateError(f"foreign artifact-root directory alias: {ancestor}")
        info = ancestor.stat()
        if not stat.S_ISDIR(info.st_mode) or (
            stat.S_IMODE(info.st_mode) & 0o022 and not info.st_mode & stat.S_ISVTX
        ):
            raise _GateError(f"unsafe artifact-root ancestor: {ancestor}")
    path = Path(path)
    if not path.is_absolute() or path != Path(os.path.normpath(path)):
        raise _GateError("artifact paths must be absolute and normalized")
    try:
        relative = path.relative_to(ARTIFACT_ROOT)
    except ValueError:
        relative = path.relative_to(root)
    current = root
    for index, part in enumerate((None, *relative.parts)):
        if part is not None:
            current = current / part
        info = current.lstat()
        is_dir = index < len(relative.parts) or directory
        kind = stat.S_ISDIR if is_dir else stat.S_ISREG
        if (
            not kind(info.st_mode)
            or info.st_uid != os.getuid()
            or info.st_dev != root.stat().st_dev
        ):
            raise _GateError(f"artifact is not a private owned regular path: {current}")
        if stat.S_IMODE(info.st_mode) != (0o700 if is_dir else 0o600):
            raise _GateError(f"artifact permissions are not private: {current}")
    return current


@overload
def _read_private(path: str | Path, limit: int) -> bytes: ...


@overload
def _read_private(path: str | Path, limit: None = None) -> str: ...


def _read_private(path: str | Path, limit: int | None = None) -> bytes | str:
    path = _private_path(path)
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), "rb") as source:
        info = os.fstat(source.fileno())
        if (
            not stat.S_ISREG(info.st_mode)
            or info.st_uid != os.getuid()
            or info.st_dev != Path("/mnt/ssd").stat().st_dev
            or stat.S_IMODE(info.st_mode) != 0o600
        ):
            raise _GateError("artifact identity changed while opening")
        if limit is not None:
            data = source.read(limit + 1)
            if len(data) > limit:
                raise _GateError("receipt/report exceeds its size limit")
            return data
        digest = hashlib.sha256()
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
        return digest.hexdigest()


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for key, value in pairs:
        if key in result:
            raise _GateError(f"duplicate receipt field: {key}")
        result[key] = value
    return result


def _receipt(
    variable: str, expected: Mapping[str, object], artifact: str
) -> tuple[str, str]:
    path = os.environ.get(variable)
    if not path:
        raise _GateError(
            f"missing {variable}; run the clean-head gate and obtain independent whole-branch review"
        )
    data: object = json.loads(
        _read_private(path, 16 * 1024), object_pairs_hook=_unique_object
    )
    fixed: dict[str, object] = {"version": 1, "status": "PASS", **expected}
    if not isinstance(data, dict) or set(data) != set(fixed) | {
        artifact,
        artifact + "_sha256",
    }:
        raise _GateError("receipt schema is invalid")
    for key, value in fixed.items():
        if type(data[key]) is not type(value) or data[key] != value:
            raise _GateError(f"receipt has stale or invalid {key}")
    artifact_path = data[artifact]
    digest = data[artifact + "_sha256"]
    if (
        not isinstance(artifact_path, str)
        or not isinstance(digest, str)
        or not re.fullmatch(r"[0-9a-f]{64}", digest)
    ):
        raise _GateError("receipt artifact identity is invalid")
    if _read_private(artifact_path) != digest:
        raise _GateError(f"{artifact} digest does not match")
    return artifact_path, digest


def _review_report(path: str, digest: str, expected: _Identity) -> None:
    content = _read_private(path, 1024 * 1024)
    if hashlib.sha256(content).hexdigest() != digest:
        raise _GateError("native report changed during verification")
    text = content.decode("utf-8")
    fields: dict[str, list[str]] = {}
    for line in text.splitlines():
        match = re.fullmatch(
            r"(HEAD|TREE|BASE|RANGE|VERDICT|ACTIONABLE_COUNT):\s*(\S+)", line
        )
        if match:
            fields.setdefault(match[1], []).append(match[2])
        elif re.match(r"(?:HEAD|TREE|BASE|RANGE|VERDICT|ACTIONABLE_COUNT)\s*:", line):
            raise _GateError("malformed native report metadata")
    for key, value in (
        ("head", expected["head"]),
        ("tree", expected["tree"]),
        ("base", expected["base"]),
        ("range", expected["range"]),
    ):
        if fields.get(key.upper()) != [value]:
            raise _GateError(
                f"native report does not bind the exact whole-branch {key}"
            )
    for key, value in (("VERDICT", "PASS"), ("ACTIONABLE_COUNT", "0")):
        values = fields.get(key, [])
        if len(values) < 2 or any(item != value for item in values):
            raise _GateError(
                "native report must consistently open and close with PASS and zero actionable findings"
            )
    lines = [line for line in text.splitlines() if line.strip()]
    if lines[:2] != ["VERDICT: PASS", "ACTIONABLE_COUNT: 0"] or lines[-2:] != lines[:2]:
        raise _GateError("native report lacks matching initial and final verdicts")


def verify() -> None:
    before = _identity()
    records = sys.stdin.read(4097)
    if len(records) > 4096 or len(records.splitlines()) != 1:
        raise _GateError("pre-push requires exactly one outgoing update")
    fields = records.split()
    ref = "refs/heads/" + before["branch"]
    if (
        len(fields) != 4
        or fields[0] != ref
        or fields[2] != ref
        or fields[1] != before["head"]
    ):
        raise _GateError("outgoing refs/OID must match the checked-out feature HEAD")
    if not re.fullmatch(r"[0-9a-f]{40}", fields[3]) or fields[1] == "0" * 40:
        raise _GateError("invalid or deleted outgoing update")
    _receipt("HYMT_LOCAL_GATE_RECEIPT", {**before, "command": GATE_COMMAND}, "log")
    report, digest = _receipt(
        "HYMT_NATIVE_REVIEW_RECEIPT", {**before, "command": REVIEW_COMMAND}, "report"
    )
    _review_report(report, digest, before)
    if _identity() != before:
        raise _GateError("Git identity changed during push verification")
    print(
        "pre-push: exact-HEAD gate and whole-branch native review verified; no checks rerun"
    )


def _subreaper(enabled: int | None = None) -> int:
    # Linux adoption lets the owner reap orphaned gate descendants, not PID 1.
    libc = ctypes.CDLL(None, use_errno=True)
    value = ctypes.c_int()
    if libc.prctl(37, ctypes.byref(value), 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "cannot read child-subreaper state")
    if enabled is not None and libc.prctl(36, enabled, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "cannot own gate descendants")
    return value.value


def _interrupt(signum: int, _frame: FrameType | None) -> NoReturn:
    if signum == signal.SIGALRM:
        raise TimeoutError("full local gate exceeded its 3005-second deadline")
    raise InterruptedError(f"local gate interrupted by signal {signum}")


def _nonblocking_write(fd: int, data: bytes) -> int:
    was_blocking = os.get_blocking(fd)
    os.set_blocking(fd, False)
    try:
        return os.write(fd, data)
    finally:
        os.set_blocking(fd, was_blocking)


def _diagnostic(message: str) -> None:
    """Report a failure without blocking a completed cancellation on stderr."""
    try:
        if sys.stderr is not None:
            _nonblocking_write(
                sys.stderr.fileno(),
                (message + "\n").encode("utf-8", errors="backslashreplace"),
            )
    except (OSError, ValueError):
        pass  # Full or closed diagnostic sinks cannot mask the primary failure.


class _ChildIdentity:
    """A kernel handle validated against this dedicated owner's child custody."""

    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.fd: int | None = os.pidfd_open(pid)
        try:
            self.exited()
        except BaseException as original:
            try:
                self.close()
            except OSError as error:
                original.add_note(f"child identity cleanup: {error}")
            raise

    def exited(self) -> bool:
        if self.fd is None:
            raise _GateError("closed child identity")
        try:
            return (
                os.waitid(os.P_PIDFD, self.fd, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                is not None
            )
        except ChildProcessError as error:
            raise _GateError("stale or foreign child identity") from error

    def signal(self, signum: int) -> None:
        if not self.exited():
            assert self.fd is not None
            # The pidfd cannot retarget a reused numeric PID.
            try:
                signal.pidfd_send_signal(self.fd, signum)
            except ProcessLookupError:
                pass  # The retained handle exited; it cannot target a reused PID.

    def reap(self) -> None:
        if self.fd is None or not self.exited():
            raise _GateError("cannot reap a live or closed child identity")
        os.waitid(os.P_PIDFD, self.fd, os.WEXITED | os.WNOHANG)
        self.close()

    def close(self) -> None:
        fd, self.fd = self.fd, None
        if fd is not None:
            os.close(fd)


def _settle(child: subprocess.Popen[bytes], root: _ChildIdentity | None = None) -> int:
    # This single-threaded producer owns only its direct/adopted gate children.
    # Hold the root unreaped until every final signal has been sent.
    began = time.monotonic()
    members: dict[int, _ChildIdentity] = {}
    errors: list[str] = []
    if root is not None:
        members[child.pid] = root
    status: int | None = None
    try:
        while time.monotonic() - began < 4:
            try:
                children = Path("/proc/thread-self/children").read_text().split()
                for text in children:
                    pid = int(text)
                    if pid not in members:
                        members[pid] = _ChildIdentity(pid)
            except (OSError, _GateError) as error:
                errors.append(str(error))
            for pid, identity in list(members.items()):
                try:
                    if identity.exited():
                        if pid != child.pid:
                            identity.reap()
                            del members[pid]
                    else:
                        identity.signal(
                            signal.SIGTERM
                            if time.monotonic() - began < 1
                            else signal.SIGKILL
                        )
                except (OSError, _GateError) as error:
                    # One failing syscall must not prevent other owned settlement.
                    errors.append(str(error))
            root = members.get(child.pid)
            if root is not None and len(members) == 1:
                try:
                    if root.exited() and Path(
                        "/proc/thread-self/children"
                    ).read_text().split() == [str(child.pid)]:
                        status = child.wait(timeout=0.05)
                        break
                except (OSError, subprocess.SubprocessError, _GateError) as error:
                    errors.append(str(error))
            time.sleep(0.02)
        if status is None:
            errors.append("gate process tree did not settle within four seconds")
    finally:
        for identity in members.values():
            try:
                identity.close()
            except OSError as error:
                errors.append(str(error))
    if errors:
        raise _GateError("; ".join(dict.fromkeys(errors)))
    assert status is not None
    return status


def run(target: str) -> None:
    if target != "head":
        raise _GateError("the gate target must be head")
    before = _identity()
    _private_path(ARTIFACT_ROOT, directory=True)
    parent = ARTIFACT_ROOT
    requested = os.environ.get("TMPDIR")
    if requested:
        candidate = Path(requested)
        if candidate == ARTIFACT_ROOT or ARTIFACT_ROOT in candidate.parents:
            parent = _private_path(candidate, directory=True)
    leaf = Path(tempfile.mkdtemp(prefix="hymt-local-gate-", dir=parent))
    log = leaf / "gate.log"
    try:
        if sys.stdout is None:
            raise BrokenPipeError("gate output is unavailable")
        announcement = f"local-gates: log: {log}\n".encode("utf-8")
        if _nonblocking_write(sys.stdout.fileno(), announcement) != len(announcement):
            raise BrokenPipeError("incomplete gate log announcement")
    except (OSError, ValueError) as error:
        try:
            leaf.rmdir()  # No gate was launched; retain nonempty leaves.
        except OSError as cleanup_error:
            error.add_note(f"unannounced leaf cleanup: {cleanup_error}")
        raise
    env = os.environ.copy()
    env.update(TMPDIR=str(parent), TMP=str(parent), TEMP=str(parent))
    # Runner options may not skip commands or select a different justfile.
    for key in list(env):
        if key.startswith("JUST_"):
            del env[key]
    previous_subreaper = _subreaper(1)
    pending: list[int] = []
    armed = False

    def record_interrupt(signum: int, _frame: FrameType | None) -> None:
        nonlocal armed
        if not pending:
            pending.append(signum)
        if armed:
            armed = False
            _interrupt(pending[0], _frame)

    previous_handlers = {
        sig: signal.signal(sig, record_interrupt)
        for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGALRM)
    }
    # Bound the read loop even if a descendant retains its output pipe.
    previous_timer = signal.setitimer(signal.ITIMER_REAL, 3005)
    try:
        with os.fdopen(
            os.open(log, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb"
        ) as log_output:
            child: subprocess.Popen[bytes] | None = None
            root = None
            try:
                child = subprocess.Popen(
                    [
                        "just",
                        "--no-dotenv",
                        "--justfile",
                        str(Path.cwd() / "justfile"),
                        "_full-gate",
                    ],
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    env=env,
                    start_new_session=True,
                )
                root = _ChildIdentity(child.pid)
                stream = child.stdout
                if stream is None:
                    raise _GateError("gate output pipe is unavailable")
                armed = True
                try:
                    if pending:
                        _interrupt(pending[0], None)
                    for chunk in iter(lambda: os.read(stream.fileno(), 65536), b""):
                        log_output.write(chunk)
                        log_output.flush()
                        offset = 0
                        while offset < len(chunk):
                            offset += os.write(sys.stdout.fileno(), chunk[offset:])
                    # Observe exit without reaping: cleanup retains root identity.
                    assert root.fd is not None
                    os.waitid(os.P_PIDFD, root.fd, os.WEXITED | os.WNOWAIT)
                finally:
                    armed = False
            finally:
                original_error = sys.exception()
                if child is not None:
                    try:
                        status = _settle(child, root)
                    except (
                        OSError,
                        subprocess.SubprocessError,
                        _GateError,
                    ) as cleanup_error:
                        if original_error is None:
                            raise
                        _diagnostic(f"local-gates cleanup: {cleanup_error}")
                    finally:
                        if child.stdout is not None:
                            current_error = sys.exception() or original_error
                            try:
                                child.stdout.close()
                            except OSError as close_error:
                                if current_error is None:
                                    raise
                                _diagnostic(f"local-gates cleanup: {close_error}")
    finally:
        original_error = sys.exception()
        restore_errors: list[str] = []
        try:
            signal.setitimer(signal.ITIMER_REAL, *previous_timer)
        except OSError as error:
            restore_errors.append(str(error))
        for sig, handler in previous_handlers.items():
            try:
                signal.signal(sig, handler)
            except OSError as error:
                restore_errors.append(str(error))
        try:
            _subreaper(previous_subreaper)
        except OSError as error:
            restore_errors.append(str(error))
        if restore_errors:
            detail = "; ".join(restore_errors)
            if original_error is None:
                raise _GateError(detail)
            _diagnostic(f"local-gates cleanup: {detail}")
    if pending:
        _interrupt(pending[0], None)
    if status:
        raise _GateError(f"full local gate failed with exit {status}; log: {log}")
    if _identity() != before:
        raise _GateError(
            f"Git identity changed during full gate; no receipt; log: {log}"
        )
    data: dict[str, object] = {
        "version": 1,
        "status": "PASS",
        **before,
        "command": GATE_COMMAND,
        "log": str(log),
        "log_sha256": _read_private(log),
    }
    path = leaf / "gate.receipt.json"
    with os.fdopen(
        os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w"
    ) as receipt_output:
        json.dump(data, receipt_output, indent=2)
        receipt_output.write("\n")
    print(f"HYMT_LOCAL_GATE_RECEIPT={path}")


def _tracked(suffix: bytes, *roots: str) -> list[str]:
    paths = [
        os.fsdecode(path)
        for path in subprocess.check_output(
            ["git", "ls-files", "-z", "--", *roots]
        ).split(b"\0")
        if path and path.endswith(suffix)
    ]
    if not paths:
        raise _GateError("no tracked inputs found")
    return paths


def _service_verify() -> None:
    allowed = {
        "hy-mt2-quality.service: Command /home/obj/.local/share/llama-cpp-cuda/llama-server is not executable: No such file or directory",
        "hy-mt2-throughput.service: Command /home/obj/.local/share/llama-cpp-cuda/llama-server is not executable: No such file or directory",
    }
    result = subprocess.run(
        [
            "systemd-analyze",
            "verify",
            "services/hy-mt2-quality.service",
            "services/hy-mt2-throughput.service",
            "services/hy-mt2-cache.service",
        ],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    print(result.stdout, end="")
    lines = result.stdout.splitlines()
    if (
        result.returncode not in (0, 1)
        or any(line not in allowed for line in lines)
        or (result.returncode and not lines)
    ):
        raise _GateError(f"unexpected systemd verify result: exit {result.returncode}")
    if lines:
        print(
            "Only the two established host-specific missing ExecStart diagnostics are allowed."
        )


def main() -> None:
    action = sys.argv[1:]
    if action == ["verify"]:
        verify()
    elif len(action) == 2 and action[0] == "run":
        run(action[1])
    elif action == ["shell-syntax"]:
        for path in _tracked(b".sh", "scripts", "services"):
            print(f"bash -n: {path}", flush=True)
            subprocess.run(["bash", "-n", path], check=True)
    elif action == ["shellcheck"]:
        subprocess.run(
            ["shellcheck", *_tracked(b".sh", "scripts", "services")], check=True
        )
    elif action == ["service-verify"]:
        _service_verify()
    elif action == ["test-inference-cache"]:
        parent = _private_path(ARTIFACT_ROOT, directory=True)
        env = os.environ.copy()
        env.update(TMPDIR=str(parent), TMP=str(parent), TEMP=str(parent))
        subprocess.run(
            ["python3", "-B", "-m", "unittest", "discover", "-s", "scripts/tests", "-p", "test_inference_cache*.py"],
            check=True,
            env=env,
        )
    elif action == ["toml-parse"]:
        for path in _tracked(b".toml"):
            with open(path, "rb") as source:
                tomllib.load(source)
            print(f"Parsed {path}")
    else:
        raise _GateError(
            "usage: local-gates.py run head|verify|shell-syntax|shellcheck|service-verify|test-inference-cache|toml-parse"
        )


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        _diagnostic(f"local-gates: {error}")
        sys.exit(1)

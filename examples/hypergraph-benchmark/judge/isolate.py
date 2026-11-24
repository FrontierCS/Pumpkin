"""The launcher where the judge runs a built program as a confined process.

`run` starts the command in a fresh instance with:

  - read access only to the system directories (SYSTEM_READ), the judge's own interpreter and
    the `read_paths` the judge names: the judge, its data, the reference and every other run
    are unreadable;
  - writes only to its own private directories (work, out, home, tmp; HOME and TMPDIR point
    there), deleted after the run, so nothing persists from one run to the next (no /tmp,
    /dev/shm, SysV IPC, POSIX message queues or keyrings);
  - an environment built from nothing (PATH, HOME, TMPDIR, LANG, LC_ALL plus `env`);
  - no network sockets, no ptrace or /proc/<pid>/{environ,mem} of the judge, no signals to it,
    no mount, namespace, BPF, perf, io_uring or keyring calls, and no new processes, though
    threads still work;
  - limits on wall time, private disk, output bytes, processes, open files and file size.

Landlock (ABI 6 or later) and seccomp build that boundary, which needs no privilege. A boundary
that can't be built is an IsolateError: a verification failure, never a candidate's score and
never a run without it. A setup failure reaches the launcher only through a close-on-exec pipe,
as in CPython's subprocess, so nothing the command does is ever an IsolateError.

The launcher half of this file runs as `python3 -I -S isolate.py <control dir>`.
"""

from __future__ import annotations

import ctypes
import errno
import json
import os
import resource
import select
import shutil
import signal
import stat
import struct
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

LAUNCHER = os.path.abspath(__file__)
# Read-only (and executable) roots every instance may use. Judge state never lives here.
SYSTEM_READ = ("/usr", "/lib", "/lib32", "/lib64", "/libx32", "/bin", "/sbin", "/etc", "/proc",
               "/sys")
DEVICES_RW = ("/dev/null", "/dev/zero", "/dev/full")
DEVICES_RO = ("/dev/random", "/dev/urandom")
# Where judge state lives: a read path may lie below these but never contain one of them.
PROTECTED_ROOTS = ("/opt/team", "/scratch", "/tmp", "/var/tmp", "/dev/shm", "/workspace",
                   "/root", "/home", "/run")
PATH = "/usr/local/bin:/usr/bin:/bin"
DISK_BYTES = 8 << 30
PROCESSES = 1 << 16  # RLIMIT_NPROC counts the user's threads, the judge's among them.
OPEN_FILES = 4096
MIN_LANDLOCK_ABI = 6  # scoped signals and abstract sockets (Linux 6.12)
NET_BIND_TCP, NET_CONNECT_TCP = 1 << 0, 1 << 1
POLL_SECONDS = 0.05
DISK_POLL_SECONDS = 1.0
# An instance lives in <tempdir>/ts-XXXXXXXX/r/{work,in,out,home,tmp}. The names are short so
# that an AF_UNIX socket bound below TMPDIR still fits sun_path (107 bytes).
INSTANCE_PREFIX = "ts-"


class IsolateError(RuntimeError):
    """The sandbox could not be built or was used wrongly: a judge or infrastructure fault."""


@dataclass(frozen=True)
class Result:
    """One instance's outcome. `limit` names the limit that ended it, if any."""
    returncode: int            # the command's exit status; -N when killed by signal N
    stderr: bytes
    timed_out: bool = False
    limit: str | None = None   # "time" | "disk" | "output" | None
    command_seconds: float = 0.0  # the command alone: exec to exit


# ------------------------------------------------------------------ judge-side API
def run(argv, *, timeout_seconds: float, read_paths=(), env=None, file_bytes: int,
        output_bytes: int) -> Result:
    """Run `argv` in a fresh instance with no standard input and return its Result.

    read_paths  existing files or directories the command may read in place: absolute,
                symlink-free, never / or a directory holding judge state
    file_bytes  sets RLIMIT_FSIZE; output_bytes bounds each of stdout and stderr
    """
    abi = landlock_abi()
    if sys.platform != "linux" or os.uname().machine != "x86_64" or abi < MIN_LANDLOCK_ABI:
        raise IsolateError(f"isolate needs Linux x86_64 with Landlock ABI >= {MIN_LANDLOCK_ABI}"
                           f" (found {abi})")
    options = {"timeout_seconds": float(timeout_seconds), "file_bytes": file_bytes,
               "output_bytes": output_bytes, "read_paths": _check_read_paths(read_paths)}
    base = Path(tempfile.mkdtemp(prefix=INSTANCE_PREFIX))
    try:
        control = base / "control"
        control.mkdir(mode=0o700)
        root = base / "r"
        root.mkdir(mode=0o700)
        paths = {name: str(root / name) for name in ("work", "in", "out", "home", "tmp")}
        for path in paths.values():
            os.mkdir(path)
        for name in ("stdin", "stdout", "stderr"):
            (control / name).write_bytes(b"")
        spec = {"argv": list(argv), "cwd": paths["work"],
                "env": {"PATH": PATH, "HOME": paths["home"], "TMPDIR": paths["tmp"],
                        "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", **(env or {})},
                "root": str(root), "paths": paths, "options": options,
                "interpreter": _interpreter_roots()}
        (control / "spec.json").write_text(json.dumps(spec))
        process = subprocess.run([sys.executable, "-I", "-S", LAUNCHER, str(control)],
                                 stdin=subprocess.DEVNULL, capture_output=True, env={},
                                 timeout=timeout_seconds + 120)
        try:
            status = json.loads((control / "status.json").read_text())
        except (OSError, ValueError) as error:
            raise IsolateError(f"isolate launcher failed (exit {process.returncode}): "
                               f"{process.stderr[-2000:]!r}") from error
        if status["error"]:
            raise IsolateError(f"isolate launcher: {status['error']}")
        return Result(returncode=status["returncode"],
                      stderr=(control / "stderr").read_bytes()[:output_bytes],
                      timed_out=status["limit"] == "time", limit=status["limit"],
                      command_seconds=status["command_ns"] / 1e9)
    except subprocess.TimeoutExpired as error:
        raise IsolateError("isolate launcher did not finish") from error
    finally:
        _remove(base)


def _check_read_paths(paths) -> list[str]:
    """The judge-named read paths, validated: each exists, is absolute, normalized and free
    of symlinks, and holds no judge root."""
    checked = []
    for value in paths:
        text = os.fspath(value)
        normal = os.path.normpath(text)
        if (not text.startswith("/") or normal == "/" or os.path.realpath(normal) != normal
                or not os.path.exists(normal)):
            raise IsolateError(f"read_paths: {text!r} must be an existing absolute path other "
                               "than / with no symlink in it")
        for root in (*PROTECTED_ROOTS, tempfile.gettempdir()):
            if _within(root, normal):
                raise IsolateError(f"read_paths: {text!r} contains judge state under {root}")
        checked.append(normal)
    return checked


def _within(path: str, root: str) -> bool:
    return path == root or path.startswith(root.rstrip("/") + "/")


def _interpreter_roots() -> list[str]:
    """The judge's Python, outside every directory that holds judge state."""
    roots = []
    for prefix in {sys.prefix, sys.base_prefix, sys.exec_prefix}:
        prefix = os.path.realpath(prefix)
        if not any(_within(root, prefix) or _within(prefix, root)
                   for root in (*PROTECTED_ROOTS, tempfile.gettempdir())):
            roots.append(prefix)
    return sorted(roots)


def _remove(base: Path) -> None:
    """Delete a run's directories even where the command removed its own permissions.

    Symlinks are unlinked, never followed or chmodded. Every process of the run is dead by now,
    so a path checked as no symlink stays one for the plain chmod.
    """
    for directory, subdirs, _ in os.walk(base, topdown=True):
        for name in list(subdirs):
            path = os.path.join(directory, name)
            try:
                if os.path.islink(path):
                    subdirs.remove(name)
                    os.unlink(path)
                else:
                    os.chmod(path, 0o700)
            except OSError:
                pass
    shutil.rmtree(base, ignore_errors=True)


# ------------------------------------------------------------------ kernel interfaces
_LIBC = ctypes.CDLL(None, use_errno=True)
_LIBC.syscall.restype = ctypes.c_long
SYS_LANDLOCK_CREATE_RULESET, SYS_LANDLOCK_ADD_RULE, SYS_LANDLOCK_RESTRICT_SELF = 444, 445, 446
SYS_PIDFD_OPEN = 434
PR_SET_NO_NEW_PRIVS, PR_SET_SECCOMP = 38, 22
SECCOMP_MODE_FILTER = 2


def landlock_abi() -> int:
    value = _LIBC.syscall(SYS_LANDLOCK_CREATE_RULESET, None, ctypes.c_size_t(0), ctypes.c_uint32(1))
    return int(value) if value > 0 else 0


class _RulesetAttr(ctypes.Structure):
    _fields_ = [("fs", ctypes.c_uint64), ("net", ctypes.c_uint64), ("scoped", ctypes.c_uint64)]


class _PathBeneath(ctypes.Structure):
    _pack_ = 1
    _fields_ = [("allowed", ctypes.c_uint64), ("parent_fd", ctypes.c_int32)]


FS_EXECUTE, FS_WRITE_FILE, FS_READ_FILE, FS_READ_DIR = 1 << 0, 1 << 1, 1 << 2, 1 << 3
FS_TRUNCATE, FS_IOCTL_DEV = 1 << 14, 1 << 15
FS_FILE_ONLY = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV
LANDLOCK_RULE_PATH_BENEATH = 1


def _landlock_ruleset(read: list[str], write: list[str]) -> int:
    """A ruleset allowing only these trees, denying every TCP bind and connect, and scoping
    signals and abstract sockets to the instance. Built before fork; the child restricts
    itself."""
    handled = (1 << 16) - 1                 # ABI 5: execute .. ioctl_dev
    attr = _RulesetAttr(handled, NET_BIND_TCP | NET_CONNECT_TCP, 0b11)
    ruleset = _LIBC.syscall(SYS_LANDLOCK_CREATE_RULESET, ctypes.byref(attr),
                            ctypes.c_size_t(ctypes.sizeof(attr)), ctypes.c_uint32(0))
    if ruleset < 0:
        raise OSError(ctypes.get_errno(), "landlock_create_ruleset")
    read_access = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR
    grants = ([(p, read_access) for p in read] + [(p, handled) for p in write]
              + [(p, FS_READ_FILE | FS_WRITE_FILE | FS_TRUNCATE | FS_IOCTL_DEV)
                 for p in DEVICES_RW]
              + [(p, FS_READ_FILE) for p in DEVICES_RO])
    for path, access in grants:
        try:
            descriptor = os.open(path, os.O_PATH | os.O_CLOEXEC)
        except FileNotFoundError:
            continue  # an absent optional root (e.g. /lib32)
        try:
            if not stat.S_ISDIR(os.fstat(descriptor).st_mode):
                access &= FS_FILE_ONLY
            rule = _PathBeneath(access & handled, descriptor)
            if _LIBC.syscall(SYS_LANDLOCK_ADD_RULE, ruleset, LANDLOCK_RULE_PATH_BENEATH,
                             ctypes.byref(rule), 0) != 0:
                raise OSError(ctypes.get_errno(), f"landlock_add_rule {path}")
        finally:
            os.close(descriptor)
    return ruleset


# x86_64 syscall numbers. Denied: debugging/inspection of other processes, mounts and
# namespaces, kernel extension points, io_uring (bypasses seccomp for network and file ops),
# keyrings, the IPC objects that outlive a process (SysV, POSIX mqueue), and new processes.
SYSCALLS_DENIED = {
    "ptrace": 101, "process_vm_readv": 310, "process_vm_writev": 311, "kcmp": 312,
    "pidfd_getfd": 438, "mount": 165, "umount2": 166, "pivot_root": 155, "chroot": 161,
    "unshare": 272, "setns": 308, "open_by_handle_at": 304, "name_to_handle_at": 303,
    "fsopen": 430, "fsconfig": 431, "fsmount": 432, "fspick": 433, "move_mount": 429,
    "open_tree": 428, "mount_setattr": 442, "bpf": 321, "perf_event_open": 298,
    "userfaultfd": 323, "io_uring_setup": 425, "io_uring_enter": 426, "io_uring_register": 427,
    "keyctl": 250, "add_key": 248, "request_key": 249, "reboot": 169, "kexec_load": 246,
    "kexec_file_load": 320, "init_module": 175, "finit_module": 313, "delete_module": 176,
    "swapon": 167, "swapoff": 168, "acct": 163, "quotactl": 179, "syslog": 103, "shmget": 29,
    "semget": 64, "msgget": 68, "mq_open": 240, "fork": 57, "vfork": 58,
}
SYS_SOCKET, SYS_CLONE, SYS_CLONE3 = 41, 56, 435
AUDIT_ARCH_X86_64 = 0xC000003E
CLONE_NAMESPACES = 0x7E020080  # NEWNS|NEWCGROUP|NEWUTS|NEWIPC|NEWUSER|NEWPID|NEWNET|NEWTIME
CLONE_THREAD = 0x00010000
RET_KILL_PROCESS, RET_ALLOW, RET_ERRNO = 0x80000000, 0x7FFF0000, 0x00050000


def seccomp_program() -> list[tuple]:
    """The classic-BPF filter as (code, jt, jf, k) tuples, with its labels resolved."""
    LD, JEQ, JGE, JSET, RET = 0x20, 0x15, 0x35, 0x45, 0x06
    program: list = []
    labels: dict = {}

    def emit(code, k, jt=0, jf=0):
        program.append([code, jt, jf, k])

    emit(LD, 4)                                              # arch
    emit(JEQ, AUDIT_ARCH_X86_64, 0, "kill")
    emit(LD, 0)                                              # nr
    emit(JGE, 0x40000000, "enosys", 0)                       # x32 ABI
    emit(JEQ, SYS_CLONE3, "enosys", 0)                       # libc falls back to clone
    for number in sorted(SYSCALLS_DENIED.values()):
        emit(JEQ, number, "eperm", 0)
    emit(JEQ, SYS_SOCKET, "eafnosupport", 0)
    emit(JEQ, SYS_CLONE, "clone", 0)
    emit(RET, RET_ALLOW)
    labels["clone"] = len(program)
    emit(LD, 16)                                             # clone flags (arg 0, low word)
    emit(JSET, CLONE_NAMESPACES, "eperm", 0)
    emit(JSET, CLONE_THREAD, "allow", "eperm")
    for name, value in (("allow", RET_ALLOW), ("eperm", RET_ERRNO | errno.EPERM),
                        ("eafnosupport", RET_ERRNO | errno.EAFNOSUPPORT),
                        ("enosys", RET_ERRNO | errno.ENOSYS), ("kill", RET_KILL_PROCESS)):
        labels[name] = len(program)
        emit(RET, value)
    resolved = []
    for index, (code, jt, jf, k) in enumerate(program):
        jt = labels[jt] - index - 1 if isinstance(jt, str) else jt
        jf = labels[jf] - index - 1 if isinstance(jf, str) else jf
        if not (0 <= jt <= 255 and 0 <= jf <= 255):
            raise IsolateError("seccomp program jump out of range")
        resolved.append((code, jt, jf, k))
    return resolved


class _Fprog(ctypes.Structure):
    _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.c_void_p)]


def _confine(ruleset: int, fprog: _Fprog) -> None:
    """In the child: no_new_privs, the Landlock domain, then the seccomp filter. For good."""
    if _LIBC.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "no_new_privs")
    if _LIBC.syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset, 0) != 0:
        raise OSError(ctypes.get_errno(), "landlock_restrict_self")
    os.close(ruleset)
    if _LIBC.prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, ctypes.byref(fprog), 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "seccomp")


# ------------------------------------------------------------------ launcher (own process)
def _disk_bytes(root: str) -> int:
    total = 0
    for directory, _, files in os.walk(root):
        for name in files:
            try:
                total += os.lstat(os.path.join(directory, name)).st_blocks * 512
            except OSError:
                continue
    return total


def _rlimits(options: dict) -> None:
    limits = {resource.RLIMIT_CORE: (0, 0),
              resource.RLIMIT_NOFILE: (OPEN_FILES,) * 2,
              resource.RLIMIT_NPROC: (PROCESSES,) * 2,
              resource.RLIMIT_FSIZE: (options["file_bytes"],) * 2}
    for kind, (soft, hard) in limits.items():
        _, current = resource.getrlimit(kind)
        if current != resource.RLIM_INFINITY:
            soft, hard = min(soft, current), min(hard, current)
        resource.setrlimit(kind, (soft, hard))


def _prepare(launch: dict, control: Path) -> dict:
    """Everything the child needs, built before fork so the timed command starts promptly."""
    paths = launch["paths"]
    read = [*SYSTEM_READ, *launch["interpreter"], *launch["options"]["read_paths"], paths["in"]]
    write = [paths["work"], paths["out"], paths["home"], paths["tmp"]]
    program = seccomp_program()
    blob = b"".join(struct.pack("HBBI", *instruction) for instruction in program)
    buffer = ctypes.create_string_buffer(blob, len(blob))  # must outlive the prctl
    stdio = [os.open(control / "stdin", os.O_RDONLY),
             os.open(control / "stdout", os.O_WRONLY | os.O_TRUNC),
             os.open(control / "stderr", os.O_WRONLY | os.O_TRUNC)]
    return {"ruleset": _landlock_ruleset(read, write), "buffer": buffer,
            "fprog": _Fprog(len(program), ctypes.addressof(buffer)), "stdio": stdio,
            "executable": _which(launch["argv"][0], launch["env"]["PATH"])}  # None: exit 127


def _child(launch: dict, ready: dict) -> None:
    """In the forked child: enter the boundary, then exec. Never returns. A setup failure is
    written to the close-on-exec pipe ready["errpipe"]; a missing command is exit 127."""
    errpipe, executing = ready["errpipe"], False
    try:
        os.sched_setaffinity(0, ready["affinity"])
        os.setsid()
        for source, target in zip(ready["stdio"], (0, 1, 2)):
            os.dup2(source, target)
        low = 3
        for keep in sorted((ready["ruleset"], errpipe)):
            os.closerange(low, keep)
            low = keep + 1
        os.closerange(low, 65536)
        for name in ("SIGPIPE", "SIGXFZ", "SIGXFSZ"):  # Python ignores these; exec keeps that
            if hasattr(signal, name):
                signal.signal(getattr(signal, name), signal.SIG_DFL)
        _rlimits(launch["options"])
        os.chdir(launch["cwd"])
        _confine(ready["ruleset"], ready["fprog"])
        executing = True
        if ready["executable"] is None:
            raise FileNotFoundError(errno.ENOENT, "command not found")
        os.execve(ready["executable"], launch["argv"], launch["env"])
    except BaseException as error:  # noqa: BLE001 - report and die; never run unconfined
        try:
            if executing and isinstance(error, FileNotFoundError):
                os.write(2, f"{launch['argv'][0]}: {error.strerror}\n".encode())
                os._exit(127)
            os.write(errpipe, f"{type(error).__name__}: {error}".encode())
        finally:
            os._exit(1)


def _which(command: str, path: str) -> str | None:
    if "/" in command:
        return command
    for directory in path.split(":"):
        candidate = os.path.join(directory, command)
        if os.access(candidate, os.X_OK):
            return candidate
    return None


def _launcher(control: Path) -> int:
    launch = json.loads((control / "spec.json").read_text())
    options = launch["options"]
    status = {"returncode": None, "limit": None, "error": None, "command_ns": 0}
    try:
        if landlock_abi() < MIN_LANDLOCK_ABI:
            raise OSError(errno.ENOSYS, f"Landlock ABI {landlock_abi()} < {MIN_LANDLOCK_ABI}")
        deadline = time.monotonic() + options["timeout_seconds"]
        ready = _prepare(launch, control)
        # The CPUs the judge runs on are the command's (a timed judge pins itself to one). The
        # launcher leaves them, so its own work never shares the command's CPU.
        ready["affinity"] = os.sched_getaffinity(0)
        os.sched_setaffinity(0, range(os.cpu_count()))
        errpipe_read, ready["errpipe"] = os.pipe()   # close-on-exec: EOF once the command runs
        pid = os.fork()
        if pid == 0:
            _child(launch, ready)
        for descriptor in (ready["ruleset"], ready["errpipe"], *ready["stdio"]):
            os.close(descriptor)
        with os.fdopen(errpipe_read, "rb") as errpipe:
            failure = errpipe.read(50000)
            # EOF without a report: the command's execve succeeded. command_seconds starts here,
            # so the fork and the instance setup in the child are never inside it.
            started = time.perf_counter_ns()
        if failure:
            raise IsolateError(f"instance setup failed: {failure.decode(errors='replace')}")
        exited = _LIBC.syscall(SYS_PIDFD_OPEN, pid, 0)   # readable when the command exits
        if exited < 0:
            raise OSError(ctypes.get_errno(), "pidfd_open")
        next_disk = time.monotonic() + DISK_POLL_SECONDS
        while True:
            # Wait first: the exit is stamped the moment the pidfd turns readable.
            wait = min(POLL_SECONDS, max(0.0, deadline - time.monotonic()))
            done = exited in select.select([exited], [], [], wait)[0]
            ended = time.perf_counter_ns()
            if done:
                status["returncode"] = os.waitstatus_to_exitcode(os.waitpid(pid, 0)[1])
                break
            now = time.monotonic()
            if now >= deadline:
                status["limit"] = "time"
            elif any((control / name).stat().st_size > options["output_bytes"]
                     for name in ("stdout", "stderr")):
                status["limit"] = "output"
            elif now >= next_disk:
                next_disk = now + DISK_POLL_SECONDS
                if _disk_bytes(launch["root"]) > DISK_BYTES:
                    status["limit"] = "disk"
            if status["limit"]:
                os.kill(pid, signal.SIGKILL)
                wait_status = os.waitpid(pid, 0)[1]
                ended = time.perf_counter_ns()
                status["returncode"] = os.waitstatus_to_exitcode(wait_status)
                break
        status["command_ns"] = ended - started
        os.close(exited)
    except Exception as error:  # noqa: BLE001 - reported to the judge as an infra fault
        status["error"] = f"{type(error).__name__}: {error}"
    (control / "status.json").write_text(json.dumps(status))
    return 0


if __name__ == "__main__":
    raise SystemExit(_launcher(Path(sys.argv[1])))

#!/usr/bin/env python3
"""Persistent, single-user development service supervisor. See docs/runall.md."""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time

if sys.version_info < (3, 11):
    raise SystemExit("runall requires Python 3.11 or newer")

import tomllib


ROOT = Path(__file__).resolve().parent.parent
START_TIMEOUT = 20
STOP_TIMEOUT = 15
MAX_MESSAGE = 65536


def secure_directory(path):
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    metadata = path.lstat()
    if (not stat.S_ISDIR(metadata.st_mode) or metadata.st_uid != os.getuid()
            or metadata.st_mode & 0o077):
        raise RuntimeError(f"directory must be owned by you and mode 0700: {path}")


@contextlib.contextmanager
def locked(path, timeout=0):
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        deadline = time.monotonic() + timeout
        while True:
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise RuntimeError(f"another runner owns {path}") from None
                time.sleep(0.05)
        yield
    finally:
        os.close(descriptor)


def receive(stream):
    with stream.makefile("rb") as reader:
        line = reader.readline(MAX_MESSAGE + 1)
    if len(line) > MAX_MESSAGE or not line.endswith(b"\n"):
        raise RuntimeError("invalid or oversized supervisor message")
    message = json.loads(line)
    if not isinstance(message, dict):
        raise RuntimeError("supervisor message must be an object")
    return message


def send(stream, message):
    stream.sendall(json.dumps(message).encode() + b"\n")


def control(directory, action):
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(STOP_TIMEOUT + 10)
        try:
            stream.connect(str(directory / "control.sock"))
        except (FileNotFoundError, ConnectionRefusedError):
            return None
        send(stream, {"action": action})
        return receive(stream)


def absolute_path(value, label):
    if not isinstance(value, str) or not Path(value).is_absolute():
        raise RuntimeError(f"{label} must be an absolute path")
    return Path(value)


def services_from_config(source):
    config = tomllib.loads(source.decode())
    audit = config.get("audit", {})
    session = config.get("session_fabric", {})
    for label, settings in (("audit", audit), ("session_fabric", session)):
        if not isinstance(settings, dict) or not isinstance(settings.get("enabled", False), bool):
            raise RuntimeError(f"{label}.enabled must be a boolean")
    services = []
    if audit.get("enabled", False):
        services.append({
            "name": "xshell-auditd",
            "socket": str(absolute_path(audit.get("socket"), "audit.socket")),
            "directory": str(absolute_path(audit.get("directory"), "audit.directory")),
        })
    if session.get("enabled", False):
        default = (Path(os.environ["XDG_STATE_HOME"]) / "xshell/sessions"
                   if "XDG_STATE_HOME" in os.environ else Path.home() / (
                       "Library/Application Support/xshell/sessions"
                       if sys.platform == "darwin" else ".local/state/xshell/sessions"))
        state = absolute_path(session.get("state_directory", str(default)),
                              "session_fabric.state_directory")
        services.append({
            "name": "xshelld",
            "socket": str(absolute_path(session.get("socket", str(state / "xshelld.sock")),
                                        "session_fabric.socket")),
            "directory": str(state),
        })
    if not services:
        raise RuntimeError("enable audit or session_fabric in the config; use cargo run for standalone mode")
    sockets = [service["socket"] for service in services]
    if len(set(sockets)) != len(sockets):
        raise RuntimeError("audit and session services must use different sockets")
    if any(len(os.fsencode(endpoint)) > 100 for endpoint in sockets):
        raise RuntimeError("service socket path is too long; use a shorter directory")
    return services


def build():
    command = ["cargo", "build", "--locked", "--message-format=json-render-diagnostics",
               "-p", "xshell-audit", "--bin", "xshell-auditd",
               "-p", "xshell-session", "--bin", "xshelld",
               "-p", "xshell-cli", "--bin", "xshell"]
    result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE, check=True, text=True)
    artifacts = {}
    for line in result.stdout.splitlines():
        record = json.loads(line)
        if record.get("reason") == "compiler-artifact" and record.get("executable"):
            artifacts[record["target"]["name"]] = Path(record["executable"])
    if not {"xshell-auditd", "xshelld", "xshell"} <= artifacts.keys():
        raise RuntimeError("Cargo did not report all three executable artifacts")
    return artifacts


def snapshot(directory, source, artifacts):
    bundle = Path(tempfile.mkdtemp(prefix="build-", dir=directory))
    try:
        (bundle / "config.toml").write_bytes(source)
        digest = hashlib.sha256(source)
        digest.update(json.dumps(services_from_config(source), sort_keys=True).encode())
        digest.update(Path(__file__).read_bytes())
        for name in sorted(artifacts):
            target = bundle / name
            shutil.copyfile(artifacts[name], target)
            target.chmod(0o700)
            digest.update(name.encode())
            with target.open("rb") as binary:
                digest.update(hashlib.file_digest(binary, "sha256").digest())
        return bundle, digest.hexdigest()
    except BaseException:
        shutil.rmtree(bundle)
        raise


def probe(bundle, service):
    command = [str(bundle / service["name"]), "--config", str(bundle / "config.toml")]
    command += ["probe"] if service["name"] == "xshelld" else ["--probe"]
    result = subprocess.run(command, capture_output=True, text=True, timeout=5)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or "service probe failed")
    if service["name"] == "xshelld":
        report = json.loads(result.stdout)
        if (report.get("daemon_status") != "ready"
                or report.get("protocol_version") != report.get("supported_protocol_version")):
            raise RuntimeError(f"session probe failed: {report}")


def stop_children(children):
    for child, service in reversed(children):
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=STOP_TIMEOUT)
            except subprocess.TimeoutExpired:
                print(f"{service['name']}: shutdown timed out; forcing exit", flush=True)
                child.kill()
                child.wait()


def supervise(directory, bundle, fingerprint):
    services = services_from_config((bundle / "config.toml").read_bytes())
    children = []
    owned_sockets = {}
    stopping = False

    def request_stop(_signal, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, request_stop)
    signal.signal(signal.SIGINT, request_stop)
    with locked(directory / "supervisor.lock"), contextlib.ExitStack() as resources:
        control_path = directory / "control.sock"
        control_path.unlink(missing_ok=True)
        try:
            directories = {Path(service[key]) if key == "directory" else Path(service[key]).parent
                           for service in services for key in ("directory", "socket")}
            for resource in sorted(directories):
                secure_directory(resource)
            for resource in sorted({resource.resolve() for resource in directories}):
                resources.enter_context(locked(resource / ".runall.lock"))
            for service in services:
                if os.path.lexists(service["socket"]):
                    raise RuntimeError(f"refusing to replace existing socket/path: {service['socket']}; "
                                       "stop its owner and remove a stale socket manually")
            for service in services:
                if stopping:
                    raise RuntimeError("startup interrupted")
                log = resources.enter_context((directory / f"{service['name']}.log").open("ab"))
                child = subprocess.Popen(
                    [str(bundle / service["name"]), "--config", str(bundle / "config.toml")],
                    stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True)
                children.append((child, service))
                deadline = time.monotonic() + START_TIMEOUT
                while True:
                    if stopping or child.poll() is not None:
                        raise RuntimeError(f"{service['name']} exited during startup; see its log")
                    try:
                        metadata = Path(service["socket"]).lstat()
                        owned_sockets[service["socket"]] = (metadata.st_dev, metadata.st_ino)
                        probe(bundle, service)
                        break
                    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
                        if time.monotonic() >= deadline:
                            raise RuntimeError(f"{service['name']} readiness failed: {error}") from error
                        time.sleep(0.1)
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(str(control_path))
                listener.listen(4)
                listener.settimeout(0.2)
                while not stopping:
                    for child, service in children:
                        if child.poll() is not None:
                            raise RuntimeError(f"{service['name']} exited unexpectedly ({child.returncode})")
                        metadata = Path(service["socket"]).lstat()
                        if (metadata.st_dev, metadata.st_ino) != owned_sockets[service["socket"]]:
                            raise RuntimeError(f"{service['name']} socket was replaced")
                    try:
                        connection, _address = listener.accept()
                    except socket.timeout:
                        continue
                    with connection:
                        connection.settimeout(2)
                        try:
                            request = receive(connection)
                            if request.get("action") == "stop":
                                stopping = True
                                send(connection, {"status": "stopping"})
                            elif request.get("action") == "status":
                                send(connection, {"status": "running", "fingerprint": fingerprint,
                                                  "bundle": str(bundle), "services": services})
                            else:
                                send(connection, {"status": "error", "message": "unknown action"})
                        except (OSError, ValueError, RuntimeError) as error:
                            print(f"control request failed: {error}", flush=True)
        finally:
            stop_children(children)
            for endpoint, identity in owned_sockets.items():
                try:
                    metadata = Path(endpoint).lstat()
                    if (metadata.st_dev, metadata.st_ino) == identity:
                        Path(endpoint).unlink()
                except FileNotFoundError:
                    pass
            control_path.unlink(missing_ok=True)


def stop(directory):
    response = control(directory, "stop")
    with locked(directory / "supervisor.lock", timeout=STOP_TIMEOUT * 2 + 10):
        pass
    return response


def ensure_running(directory, bundle, fingerprint):
    current = control(directory, "status")
    if current is not None:
        if current["fingerprint"] != fingerprint:
            raise RuntimeError("running binaries/config differ; use 'restart' explicitly (ends active work)")
        return current
    with locked(directory / "supervisor.lock"):
        pass
    with (directory / "supervisor.log").open("ab") as log:
        supervisor = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "_supervise", str(directory),
             str(bundle), fingerprint], stdin=subprocess.DEVNULL, stdout=log, stderr=log,
            start_new_session=True)
    try:
        deadline = time.monotonic() + START_TIMEOUT * 2 + 10
        while time.monotonic() < deadline:
            if supervisor.poll() is not None:
                raise RuntimeError(f"supervisor startup failed; see {directory / 'supervisor.log'}")
            current = control(directory, "status")
            if current is not None:
                return current
            time.sleep(0.1)
        raise RuntimeError(f"supervisor startup timed out; see {directory / 'supervisor.log'}")
    except BaseException:
        if supervisor.poll() is None:
            supervisor.terminate()
            supervisor.wait(timeout=STOP_TIMEOUT * 2 + 10)
        raise


def main():
    os.umask(0o077)
    if len(sys.argv) > 1 and sys.argv[1] == "_supervise":
        supervise(Path(sys.argv[2]), Path(sys.argv[3]), sys.argv[4])
        return
    arguments = sys.argv[1:]
    cli_args = []
    if "--" in arguments:
        separator = arguments.index("--")
        arguments, cli_args = arguments[:separator], arguments[separator + 1:]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["run", "start", "status", "stop", "restart"],
                        nargs="?", default="run")
    parser.add_argument("--config", type=Path,
                        default=Path(os.environ.get("XSHELL_CONFIG", Path.home() / ".config/xshell/config.toml")))
    parser.add_argument("--state-root", type=Path, default=Path.home() / ".local/state/xshell/runall",
                        help="private directory for per-checkout/config runner state")
    args = parser.parse_args(arguments)
    if any(value == "--config" or value.startswith("--config=") for value in cli_args):
        parser.error("pass --config to the runner so every service uses the same config")
    if cli_args and args.action != "run":
        parser.error("CLI arguments after -- require the run action")
    config_path = args.config.resolve()
    identity = hashlib.sha256(f"{ROOT}\0{config_path}".encode()).hexdigest()[:16]
    base = args.state_root.absolute()
    secure_directory(base)
    directory = base / identity
    secure_directory(directory)
    if len(os.fsencode(directory / "control.sock")) > 100:
        raise RuntimeError("runner control socket path is too long; choose a shorter --state-root")
    print(f"Runner state: {directory}", file=sys.stderr)
    with locked(directory / "command.lock", timeout=1):
        if args.action == "stop":
            print("stopped" if stop(directory) else "not running")
            return
        if args.action == "status":
            current = control(directory, "status")
            if current:
                for service in current["services"]:
                    probe(Path(current["bundle"]), service)
            print(json.dumps(current or {"status": "stopped"}, indent=2))
            return
        source = config_path.read_bytes()
        services_from_config(source)
        artifacts = build()
        bundle, fingerprint = snapshot(directory, source, artifacts)
        try:
            if args.action == "restart":
                stop(directory)
            current = ensure_running(directory, bundle, fingerprint)
            active_bundle = Path(current["bundle"])
            for service in current["services"]:
                probe(active_bundle, service)
        finally:
            active = control(directory, "status")
            if active is None or Path(active["bundle"]) != bundle:
                shutil.rmtree(bundle)
        if args.action != "run":
            print("services ready")
            return
    os.execv(str(active_bundle / "xshell"), ["xshell", "--config",
             str(active_bundle / "config.toml"), *cli_args])


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"runall: {error}", file=sys.stderr)
        sys.exit(1)

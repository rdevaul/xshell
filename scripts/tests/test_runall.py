import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "runall.py"
SPEC = importlib.util.spec_from_file_location("runall", SCRIPT)
runner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(runner)


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="xr-", dir="/tmp")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.state = self.root / "runner"
        self.state.mkdir(mode=0o700)
        self.events = self.root / "events"
        self.config = (
            f'[audit]\nenabled=true\nsocket="{self.root}/audit/sock"\n'
            f'directory="{self.root}/audit"\n'
            f'[session_fabric]\nenabled=true\nsocket="{self.root}/session/sock"\n'
            f'state_directory="{self.root}/session"\n'
            f'[testing]\nevents="{self.events}"\n'
        ).encode()
        self.artifacts = {}
        for name in ("xshell-auditd", "xshelld", "xshell"):
            target = self.root / name
            fixture = Path(__file__).with_name("fake_service.py").read_text()
            target.write_text(f"#!{sys.executable}\n" + fixture.split("\n", 1)[1])
            target.chmod(0o700)
            self.artifacts[name] = target
        self.bundle, self.fingerprint = runner.snapshot(self.state, self.config, self.artifacts)

    def launch(self, state=None, bundle=None, fingerprint=None):
        state = state or self.state
        log = (state / "test.log").open("wb")
        self.addCleanup(log.close)
        child = subprocess.Popen([sys.executable, str(SCRIPT), "_supervise", str(state),
                                  str(bundle or self.bundle), fingerprint or self.fingerprint],
                                 stdout=log, stderr=log)

        def cleanup():
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=20)

        self.addCleanup(cleanup)
        return child

    def wait_ready(self, child, state=None):
        state = state or self.state
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if child.poll() is not None:
                self.fail((state / "test.log").read_text())
            status = runner.control(state, "status")
            if status:
                return status
            time.sleep(0.02)
        self.fail("supervisor did not become ready")

    def records(self):
        return [json.loads(line) for line in self.events.read_text().splitlines()]

    def test_lifecycle_reuses_children_and_stops_in_dependency_order(self):
        child = self.launch()
        status = self.wait_ready(child)
        for service in status["services"]:
            runner.probe(self.bundle, service)
        reused = runner.ensure_running(self.state, self.bundle, self.fingerprint)
        self.assertEqual(reused, status)
        self.assertEqual([event["name"] for event in self.records()], ["xshell-auditd", "xshelld"])
        runner.stop(self.state)
        self.assertEqual(child.wait(timeout=3), 0)
        self.assertEqual([event["name"] for event in self.records() if event["action"] == "stop"],
                         ["xshelld", "xshell-auditd"])
        self.assertFalse((self.root / "session/sock").exists())
        self.assertFalse((self.root / "audit/sock").exists())

    def test_external_rebuild_and_config_change_require_explicit_restart(self):
        child = self.launch()
        self.wait_ready(child)
        with self.artifacts["xshelld"].open("a") as output:
            output.write("\n# rebuilt externally\n")
        changed, fingerprint = runner.snapshot(self.state, self.config, self.artifacts)
        self.assertNotEqual(fingerprint, self.fingerprint)
        with self.assertRaisesRegex(RuntimeError, "restart"):
            runner.ensure_running(self.state, changed, fingerprint)
        changed, fingerprint = runner.snapshot(self.state, self.config + b"\n# change\n", self.artifacts)
        with self.assertRaisesRegex(RuntimeError, "restart"):
            runner.ensure_running(self.state, changed, fingerprint)
        self.assertEqual(len(self.records()), 2)

    def test_existing_socket_is_not_replaced_or_signalled(self):
        audit = self.root / "audit"
        audit.mkdir(mode=0o700)
        with socket.socket(socket.AF_UNIX) as unrelated:
            unrelated.bind(str(audit / "sock"))
            identity = (audit / "sock").stat().st_ino
            child = self.launch()
            self.assertNotEqual(child.wait(timeout=5), 0)
            self.assertEqual((audit / "sock").stat().st_ino, identity)
            self.assertFalse(self.events.exists())

    def test_second_checkout_cannot_take_over_shared_resources(self):
        first = self.launch()
        self.wait_ready(first)
        other = self.root / "other-runner"
        other.mkdir(mode=0o700)
        second = self.launch(state=other)
        self.assertNotEqual(second.wait(timeout=5), 0)
        self.assertIsNone(first.poll())
        self.assertEqual(len(self.records()), 2)

    def test_failed_second_service_cleans_up_first(self):
        (self.bundle / "config.toml").write_bytes(self.config + b'fail="xshelld"\n')
        child = self.launch()
        self.assertNotEqual(child.wait(timeout=5), 0)
        self.assertIn({"name": "xshell-auditd", "action": "stop",
                       "pid": self.records()[0]["pid"]}, self.records())
        self.assertIsNone(runner.control(self.state, "status"))
        self.assertFalse((self.root / "audit/sock").exists())

    def test_child_crash_stops_sibling(self):
        child = self.launch()
        self.wait_ready(child)
        session = next(event for event in self.records() if event["name"] == "xshelld")
        os.kill(session["pid"], signal.SIGKILL)
        self.assertNotEqual(child.wait(timeout=5), 0)
        self.assertEqual(self.records()[-1]["name"], "xshell-auditd")
        self.assertEqual(self.records()[-1]["action"], "stop")

    def test_replaced_socket_is_preserved_during_cleanup(self):
        child = self.launch()
        self.wait_ready(child)
        endpoint = self.root / "session/sock"
        endpoint.unlink()
        endpoint.write_text("replacement")
        self.assertNotEqual(child.wait(timeout=5), 0)
        self.assertEqual(endpoint.read_text(), "replacement")

    def test_same_instance_lock_prevents_duplicate_supervisor(self):
        first = self.launch()
        status = self.wait_ready(first)
        second = self.launch()
        self.assertNotEqual(second.wait(timeout=5), 0)
        self.assertEqual(runner.control(self.state, "status"), status)
        self.assertEqual(len(self.records()), 2)

    def test_stalled_probe_is_bounded(self):
        (self.bundle / "config.toml").write_bytes(self.config + b'hang="xshelld"\n')
        child = self.launch()
        self.assertNotEqual(child.wait(timeout=30), 0)
        self.assertIsNone(runner.control(self.state, "status"))
        self.assertFalse((self.root / "session/sock").exists())
        self.assertFalse((self.root / "audit/sock").exists())

    def test_config_paths_and_disabled_services(self):
        services = runner.services_from_config(self.config.replace(b"enabled=true", b"enabled=false", 1))
        self.assertEqual([service["name"] for service in services], ["xshelld"])
        with self.assertRaisesRegex(RuntimeError, "absolute"):
            runner.services_from_config(b'[audit]\nenabled=true\nsocket="relative"')
        with self.assertRaisesRegex(RuntimeError, "enable"):
            runner.services_from_config(b"")

    def test_insecure_and_symlink_state_directories_are_rejected(self):
        self.state.chmod(0o777)
        with self.assertRaisesRegex(RuntimeError, "0700"):
            runner.secure_directory(self.state)
        self.state.chmod(0o700)
        link = self.root / "link"
        link.symlink_to(self.state)
        with self.assertRaisesRegex(RuntimeError, "0700"):
            runner.secure_directory(link)

    def test_commands_attach_cli_and_preserve_services_across_exit(self):
        config_path = self.root / "my config.toml"
        config_path.write_bytes(self.config)
        self.artifacts["xshell"].write_text(
            f"#!{sys.executable}\nimport json, sys\nprint(json.dumps(sys.argv[1:]))\n")
        cargo = self.root / "cargo"
        records = [{"reason": "compiler-artifact", "target": {"name": name},
                    "executable": str(path)} for name, path in self.artifacts.items()]
        cargo.write_text(f"#!{sys.executable}\nprint({chr(10).join(json.dumps(record) for record in records)!r})\n")
        cargo.chmod(0o700)
        environment = dict(os.environ, PATH=f"{self.root}{os.pathsep}{os.environ['PATH']}")
        command = [str(SCRIPT.with_suffix(".sh")), "--config", str(config_path),
                   "--state-root", str(self.root / "state")]

        def run(*args, success=True):
            result = subprocess.run([*command, *args], cwd="/tmp", env=environment,
                                    capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode == 0, success, result.stderr)
            return result

        self.addCleanup(lambda: run("stop"))
        run("start")
        initial = json.loads(run("status").stdout)
        attached = json.loads(run("run", "--", "--approval", "off").stdout)
        self.assertEqual(attached, ["--config", str(Path(initial["bundle"]) / "config.toml"),
                                    "--approval", "off"])
        self.assertEqual(json.loads(run("status").stdout), initial)
        self.assertEqual(len(self.records()), 2)
        cargo.write_text(f"#!{sys.executable}\nraise SystemExit(9)\n")
        run("restart", success=False)
        self.assertEqual(json.loads(run("status").stdout), initial)
        run("run", "--", "--config=other.toml", success=False)
        run("stop")
        self.assertEqual(json.loads(run("status").stdout), {"status": "stopped"})

    @unittest.skipUnless(os.environ.get("XSHELL_RUNALL_REAL_BIN_DIR"), "real binaries not requested")
    def test_real_daemons_and_cli_attach(self):
        binaries = Path(os.environ["XSHELL_RUNALL_REAL_BIN_DIR"]).resolve()
        artifacts = {name: binaries / name for name in self.artifacts}
        source = self.config.split(b"[testing]", 1)[0] + (
            '\n[models.test]\nprovider="ollama"\nmodel="test"\n'
            'base_url="http://127.0.0.1:1"\n'
        ).encode()
        bundle, fingerprint = runner.snapshot(self.state, source, artifacts)
        child = self.launch(bundle=bundle, fingerprint=fingerprint)
        status = self.wait_ready(child)
        for service in status["services"]:
            runner.probe(bundle, service)
        result = subprocess.run([str(bundle / "xshell"), "--config", str(bundle / "config.toml"),
                                 "--profile", "test", "--approval", "off"],
                                input="//quit\n", capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIsNone(child.poll())
        runner.stop(self.state)
        self.assertEqual(child.wait(timeout=5), 0)


if __name__ == "__main__":
    unittest.main()

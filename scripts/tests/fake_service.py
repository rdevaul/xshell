#!/usr/bin/env python3
"""Subprocess fixture for supervisor lifecycle tests."""

import json
import os
from pathlib import Path
import signal
import socket
import sys
import time
import tomllib

name = Path(sys.argv[0]).name
source = Path(sys.argv[sys.argv.index("--config") + 1])
config = tomllib.loads(source.read_text())
settings = config["audit" if name == "xshell-auditd" else "session_fabric"]
testing = config.get("testing", {})
endpoint = settings["socket"]

if "--probe" in sys.argv or "probe" in sys.argv:
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(0.2)
        client.connect(endpoint)
        client.sendall(b"probe")
        if client.recv(100) != b"ready":
            sys.exit(1)
    if name == "xshelld":
        print(json.dumps({"daemon_status": "ready", "protocol_version": 1,
                          "supported_protocol_version": 1}))
    sys.exit(0)


def event(action):
    with open(testing["events"], "a") as output:
        output.write(json.dumps({"name": name, "action": action, "pid": os.getpid()}) + "\n")


def terminate(_signal, _frame):
    event("stop")
    sys.exit(0)


signal.signal(signal.SIGTERM, terminate)
event("start")
if testing.get("fail") == name:
    sys.exit(4)
with socket.socket(socket.AF_UNIX) as listener:
    listener.bind(endpoint)
    listener.listen()
    while True:
        connection, _address = listener.accept()
        with connection:
            connection.recv(100)
            if testing.get("hang") == name:
                time.sleep(60)
            connection.sendall(b"ready")

#!/usr/bin/env python3
"""Connect to initrd sshd-unlock, answer passphrase prompts via PTY, and wait for EOF."""

import os
import pty
import re
import select
import signal
import subprocess
import sys
import time


def main() -> int:
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <passphrase>", file=sys.stderr)
        return 1

    passphrase = sys.argv[1]
    cmd = [
        "ssh", "-tt",
        "-i", "/tmp/ci_id_ed25519",
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "ConnectTimeout=3",
        "-o", "BatchMode=yes",
        "-p", "2222",
        "root@127.0.0.1",
    ]
    master, slave = pty.openpty()
    proc = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=slave, close_fds=True)
    os.close(slave)

    ansi_re = re.compile(rb"\x1b\[[0-9;?]*[ -/]*[@-~]")
    prompt_re = re.compile(rb"(?i)(?:enter new passphrase|confirm passphrase|enter passphrase)[^:\r\n]*:")
    buf = b""
    answered = 0
    eof = False
    deadline = time.monotonic() + 120

    try:
        while time.monotonic() < deadline:
            r, _, _ = select.select([master], [], [], 1.0)
            if not r:
                if proc.poll() is not None:
                    eof = True
                    break
                continue
            try:
                data = os.read(master, 4096)
            except OSError:
                eof = True
                break
            if not data:
                eof = True
                break
            sys.stdout.buffer.write(data)
            sys.stdout.buffer.flush()
            buf = ansi_re.sub(b"", buf + data)
            if prompt_re.search(buf):
                os.write(master, (passphrase + "\n").encode())
                answered += 1
                buf = b""
    finally:
        os.close(master)
        if proc.poll() is None:
            proc.send_signal(signal.SIGTERM)
        proc.wait(timeout=5)

    if answered > 0 and eof:
        return 0
    if answered > 0 and not eof:
        print("\nunlock_var_ssh: timed out waiting for switch-root EOF", file=sys.stderr)
        return 2
    return 1


if __name__ == "__main__":
    raise SystemExit(main())

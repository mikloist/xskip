#!/usr/bin/env python3
"""Map vCPU index to host thread id over QMP, for pinning.

query-cpus-fast is the only interface that reports the thread ids, and it
speaks JSON over a unix socket, so this is a program rather than a shell
pipeline.

usage: qmp.py <qmp-unix-socket>   -> "0:1234 1:1235 ..."
"""

import json
import socket
import sys


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__.strip().splitlines()[-1])
    s = socket.socket(socket.AF_UNIX)
    s.connect(sys.argv[1])
    f = s.makefile("rw")
    f.readline()  # greeting
    for cmd in ("qmp_capabilities", "query-cpus-fast"):
        f.write(json.dumps({"execute": cmd}) + "\n")
        f.flush()
        while True:
            reply = json.loads(f.readline())
            if "return" in reply or "error" in reply:
                break
    if "error" in reply:
        sys.exit(reply["error"]["desc"])
    print(" ".join("%d:%d" % (c["cpu-index"], c["thread-id"]) for c in reply["return"]))


if __name__ == "__main__":
    main()

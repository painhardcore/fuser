#!/usr/bin/env python3
"""Writes Docker's default seccomp profile with the io_uring syscalls allowed.

FUSE over io_uring cannot run under the default profile, which blocks io_uring. Running
unconfined instead would also unblock swapon, which acts on the host rather than the container.
"""

import json
import sys
import urllib.request

PROFILE = "https://raw.githubusercontent.com/moby/moby/v28.0.0/profiles/seccomp/default.json"

with urllib.request.urlopen(PROFILE) as response:
    profile = json.load(response)
profile["syscalls"].append(
    {
        "names": ["io_uring_setup", "io_uring_enter", "io_uring_register"],
        "action": "SCMP_ACT_ALLOW",
    }
)
json.dump(profile, sys.stdout, indent=2)

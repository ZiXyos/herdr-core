#!/usr/bin/env python3
"""Deterministic interactive stand-in for a harness: no network, no model."""
import os
import sys

print('FIXTURE_READY env=%s cwd=%s arg=%s' % (os.environ.get('FIXTURE_ENV'), os.getcwd(), sys.argv[1]), flush=True)
print('FIXTURE_LAUNCH %s' % os.environ.get('HYDRA_LAUNCH_ID'), flush=True)
for line in sys.stdin:
    print('FIXTURE_REPLY ' + line.strip(), flush=True)

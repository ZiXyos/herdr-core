#!/usr/bin/env python3
"""Records the exact argv/cwd it received as JSON; used for literal-delivery checks."""
import json, os, sys
with open(sys.argv[1], 'w') as f:
    json.dump(dict(argv=sys.argv[2:], cwd=os.getcwd(), env=os.environ.get('FIXTURE_ENV')), f)
print('ARGV_RECORDED', flush=True)

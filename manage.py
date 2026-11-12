#!/usr/bin/env python3
# Compatibility launcher: `python manage.py <command>` and `./manage.py <command>` run the Rust binary.
import os
import sys

os.execv("/usr/local/bin/shopcore", ["shopcore", "manage", *sys.argv[1:]])

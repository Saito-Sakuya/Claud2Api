"""Create a gateway key in a new protected file without printing the key."""
import os
from pathlib import Path
import secrets
import sys

path = Path(sys.argv[1]).resolve()
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w', encoding='utf-8') as output:
    output.write('claud2api_' + secrets.token_urlsafe(32) + '\n')
print(f'Created key file: {path}')

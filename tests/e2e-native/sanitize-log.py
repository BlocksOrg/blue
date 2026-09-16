"""Sanitize complete backend logs before they leave the ephemeral instance."""
import os
import re
import sys

text = sys.stdin.read()
for key in ("OPENROUTER_API_KEY", "LITELLM_MASTER_KEY"):
    secret = os.environ.get(key)
    if secret:
        text = text.replace(secret, "[REDACTED]")
text = re.sub(r"eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+", "[JWT]", text)
text = re.sub(r"(Bearer\s+|sk-)[A-Za-z0-9._-]+", "[TOKEN]", text, flags=re.I)
text = re.sub(r'X-Amz-[^\s"<>]+', "[SIGNED-URL]", text, flags=re.I)
sys.stdout.write(text)

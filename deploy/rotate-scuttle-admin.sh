#!/usr/bin/env bash
#
# Replace the Scuttlebutt moderator password with a real one.
#
#   bash deploy/rotate-scuttle-admin.sh
#
# WHY THIS EXISTS
#
# `vercel env pull` does not return the value of a variable marked Sensitive. It returns the
# literal string `[SENSITIVE]`, and `configure-dock.sh` wrote that into `[scuttle]` as three
# secrets on 2026-09-11. The moderator password on the live board was therefore the eleven
# characters `[SENSITIVE]`, which anyone who has ever run that command could guess.
#
# The script's own length check caught it - it refused an eleven-character trip salt - and the
# check was then relaxed, on the reasoning that inventing a minimum for a salt would be worse than
# useless. That reasoning was right about salts and wrong about this: the value was not a short
# secret, it was not a secret at all.
#
# WHAT THIS ROTATES, AND WHAT IT DELIBERATELY DOES NOT
#
# Only `admin_secret`, because only it is an authentication check.
#
# `trip_salt` and `ip_pepper` derive identifiers from history. Rotating them orphans every
# tripcode and every ban recorded since the bad write, and neither is something anyone logs in
# with. They are wrong, and they are not urgent in the way a guessable password is. Decide what to
# do about them separately, knowing the window.
#
# The new value is generated on the box and printed once, on your terminal. Save it before you
# close the window: nothing else has a copy, and this script will refuse to run a second time.
set -euo pipefail

HOST=${HOST:-root@167.235.29.213}
KEY=${KEY:-$HOME/.ssh/hetzner_drpc}

ssh -i "$KEY" "$HOST" 'bash -s' <<'BOX'
set -euo pipefail
export LC_ALL=C
CFG=/etc/kittiwake/config.toml
umask 077

[ -f "$CFG" ] || { echo "no $CFG" >&2; exit 1; }
grep -q '^\[scuttle\]' "$CFG" || { echo "no [scuttle] section to fix" >&2; exit 1; }

# Read the current value from inside [scuttle] only, so a same-named key in another section can
# never be what this decides on.
CURRENT=$(sed -n '/^\[scuttle\]/,/^\[/p' "$CFG" | grep -m1 '^admin_secret' | cut -d'"' -f2)
if [ "$CURRENT" != "[SENSITIVE]" ]; then
  echo "admin_secret is not the placeholder (it is ${#CURRENT} characters). Nothing was changed." >&2
  echo "If you meant to rotate a real secret, do that deliberately rather than with this script." >&2
  exit 1
fi

NEW=$(openssl rand -hex 32)
cp -p "$CFG" "$CFG.bak.$(date +%s)"

TMP=$(mktemp)
# One line, inside one section. A file-wide substitution would reach the same key name elsewhere
# the day another section grows one.
NEW="$NEW" python3 - "$CFG" > "$TMP" <<'PY'
import os, re, sys

src = open(sys.argv[1]).read()

def fix(section):
    return re.sub(r'(?m)^admin_secret\s*=\s*".*"$',
                  'admin_secret = "%s"' % os.environ['NEW'], section.group(0))

# `(?ms)` with a per-line lookahead: the section ends at the next `[`, not at the end of the file.
src = re.sub(r'(?ms)^\[scuttle\]\n(?:(?!^\[).*\n?)*', fix, src)
sys.stdout.write(src)
PY

grep -q '^\[nest\]' "$TMP" || { echo "the rewrite lost [nest]" >&2; rm -f "$TMP"; exit 1; }
if grep -q 'admin_secret = "\[SENSITIVE\]"' "$TMP"; then
  echo "the placeholder is still in the file; nothing was installed" >&2
  rm -f "$TMP"
  exit 1
fi

install -m 0640 -o root -g kittiwake "$TMP" "$CFG"
rm -f "$TMP"

systemctl restart kittiwake
sleep 6
systemctl is-active kittiwake

# Proof from the outside, not from the file: the old password has to stop working.
code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X POST \
  http://127.0.0.1:8080/api/scuttlebutt/admin/login \
  -H 'content-type: application/json' -d '{"password":"[SENSITIVE]"}')
echo "the placeholder password now returns http $code  (401 or 403 is what you want)"
echo
echo "the new moderator password, shown once:"
echo "  $NEW"
BOX

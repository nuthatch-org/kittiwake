# Shared checks for values copied out of Vercel. Sourced, not run.
#
#   . "$(dirname "$0")/secret-guard.sh"
#   refuse_placeholder SCUTTLEBUTT_TRIP_SALT "$value"
#
# WHY THIS IS SHARED RATHER THAN COPIED
#
# It was copied. `configure-tap.sh` stripped a `0x` prefix before checking a key was hexadecimal
# and `configure-receipts.sh` did not, so the same key was accepted by one script and reported as
# corrupt by the other. Three runs were spent chasing that.
#
# WHAT IT GUARDS
#
# `vercel env pull` does not return the value of a variable marked Sensitive in the dashboard. It
# writes the literal string `[SENSITIVE]` into the file instead, and says nothing about having done
# so. A script that copies a secret from Vercel therefore copies eleven printable characters that
# are the same on every project in the world.
#
# On 2026-09-11 that reached production: `[scuttle]` was configured with `[SENSITIVE]` as the
# trip salt, the IP pepper, and the moderator password. The board's admin login accepted a string
# anyone could guess for about a day.
#
# The shape of the bug is worth naming, because a length check alone did catch it and was then
# argued away: the value was not a short secret, it was not a secret at all. Check for the
# placeholder by name, not by length.

# Refuse a value that Vercel declined to give us. Never prints the value.
refuse_placeholder() {
  local name="$1" value="$2"
  case "$value" in
    '[SENSITIVE]'|'[sensitive]')
      echo "$name came back from Vercel as the literal placeholder \`[SENSITIVE]\`." >&2
      echo >&2
      echo "That variable is marked Sensitive, and \`vercel env pull\` does not return the values" >&2
      echo "of Sensitive variables. It is not a short secret; it is not the secret." >&2
      echo >&2
      echo "Read it from the dashboard instead: the Lodestar project, Settings, Environment" >&2
      echo "Variables, find $name, open Edit to reveal it, and paste it when asked." >&2
      return 1 ;;
  esac
  return 0
}

# A 32-byte hex key, `0x` optional. Echoes the bare value on success so callers use one spelling.
# Describes a rejected value by its length and its offending characters, never by printing it.
require_hex32() {
  local name="$1" raw="$2" bare="${2#0x}"
  refuse_placeholder "$name" "$raw" || return 1
  case "$bare" in
    *[!0-9a-fA-F]*)
      echo "$name is not hexadecimal." >&2
      echo "  length: ${#bare} characters" >&2
      echo "  it contains: $(printf '%s' "$bare" | tr -d '0-9a-fA-F' | fold -w1 | sort -u | tr -d '\n' | sed 's/./& /g')" >&2
      echo "  expected: 64 hex characters, with or without a 0x prefix" >&2
      return 1 ;;
  esac
  if [ "${#bare}" -ne 64 ]; then
    echo "$name is ${#bare} hex characters; kittiwake wants 64 (32 bytes)." >&2
    return 1
  fi
  printf '%s' "$bare"
}

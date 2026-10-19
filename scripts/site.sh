# Sourced by the harness scripts: loads the machine-specific values.
#
# Looks for, in order: $SV_SITE_ENV; scripts/site.env beside this file; the
# main checkout's scripts/site.env (so a worktree needs no copy of its own).
# Without one it falls back to site.env.example, whose placeholders make any
# attempt to reach a machine fail loudly instead of reaching the wrong one.

_site_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
_site_file=
if [ -n "${SV_SITE_ENV:-}" ]; then
  _site_file=$SV_SITE_ENV
elif [ -f "$_site_dir/site.env" ]; then
  _site_file=$_site_dir/site.env
else
  _site_common=$(git -C "$_site_dir" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
  if [ -n "$_site_common" ] && [ -f "$(dirname "$_site_common")/scripts/site.env" ]; then
    _site_file=$(dirname "$_site_common")/scripts/site.env
  fi
fi
if [ -z "$_site_file" ]; then
  echo "site.sh: no scripts/site.env; using the placeholders in site.env.example" >&2
  _site_file=$_site_dir/site.env.example
fi
# shellcheck disable=SC1090
. "$_site_file"
unset _site_dir _site_file _site_common

# Derived values.
SITE_HOST="$SITE_HOST_USER@$SITE_HOST_IP"
SITE_GUEST="$SITE_GUEST_USER@$SITE_GUEST_IP"
SITE_GUEST_MSYS_HOME="/c/Users/$SITE_GUEST_USER"
SITE_GUEST_WIN_HOME="C:\\Users\\$SITE_GUEST_USER"

# Quoted heredocs cannot expand variables, so the PowerShell and shell text
# they hold names the guest's account as @GUEST_USER@; this fills it in, in
# place, in each file given.
site_fill() {
  sed -i "s/@GUEST_USER@/$SITE_GUEST_USER/g" "$@"
}

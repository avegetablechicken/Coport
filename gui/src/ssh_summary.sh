# Fixed read-only discovery command. Never evaluate the recorded path as shell code.
# System tools come from fixed directories (merged or split /usr), never from PATH.
system_tool() {
  for dir in /usr/bin /bin; do
    if [ -x "$dir/$1" ]; then printf '%s' "$dir/$1"; return 0; fi
  done
  return 1
}
uname_bin=$(system_tool uname) && stat_bin=$(system_tool stat) && dd_bin=$(system_tool dd) || {
  printf '%s\n' 'Automatic SSH discovery requires uname, stat and dd in /usr/bin or /bin.' >&2; exit 126
}
case "$("$uname_bin" -s)" in
  Darwin) record="$HOME/Library/Application Support/io.github.coport.gui/summary-executable"; stat_mode='darwin' ;;
  Linux) record="${XDG_CONFIG_HOME:-$HOME/.config}/io.github.coport.gui/summary-executable"; stat_mode='linux' ;;
  *) printf '%s\n' 'Automatic SSH discovery supports Linux and macOS.' >&2; exit 126 ;;
esac
if [ -e "$record" ] || [ -L "$record" ]; then
  if [ ! -f "$record" ] || [ -L "$record" ] || [ ! -O "$record" ]; then
    printf '%s\n' 'Unsafe Coport discovery record; restart Coport on the destination.' >&2; exit 126
  fi
  if [ "$stat_mode" = darwin ]; then
    mode=$("$stat_bin" -f '%Lp' "$record" 2>/dev/null)
  else
    mode=$("$stat_bin" -c '%a' "$record" 2>/dev/null)
  fi
  case "$mode" in 600|400) ;; *) printf '%s\n' 'Coport discovery record must be private.' >&2; exit 126 ;; esac
  # Bound the path read even if the record has been replaced with a large file.
  binary=$("$dd_bin" if="$record" bs=4097 count=1 2>/dev/null)
  if [ "${#binary}" -gt 4096 ]; then
    printf '%s\n' 'Invalid Coport discovery record.' >&2; exit 126
  fi
  case "$binary" in /*) ;; *) printf '%s\n' 'Invalid Coport discovery record.' >&2; exit 126 ;; esac
  if [ -f "$binary" ] && [ -x "$binary" ]; then
    exec "$binary" --summary
  fi
  # Startup preserves a standalone helper on both Linux and macOS, independent
  # of the original build directory or application bundle. Never repair on read.
  helper="${record%/*}/summary-helper"
  if [ -e "$helper" ] || [ -L "$helper" ]; then
    if [ ! -f "$helper" ] || [ -L "$helper" ] || [ ! -O "$helper" ]; then
      printf '%s\n' 'Unsafe preserved Coport helper.' >&2; exit 126
    fi
    if [ "$stat_mode" = darwin ]; then
      mode=$("$stat_bin" -f '%Lp' "$helper" 2>/dev/null)
    else
      mode=$("$stat_bin" -c '%a' "$helper" 2>/dev/null)
    fi
    case "$mode" in 700|500) ;; *) printf '%s\n' 'Preserved Coport helper must be private and executable.' >&2; exit 126 ;; esac
    exec "$helper" --summary
  fi
  # Linux keeps an unlinked executable available while its process is alive.
  # Match the private registration exactly, and only inspect our own processes;
  # never select a process merely because its name happens to be coportd.
  if [ "$stat_mode" = linux ]; then
    readlink_bin=$(system_tool readlink) || readlink_bin=''
    if [ -n "$readlink_bin" ]; then
      for process in /proc/[0-9]*; do
        [ -O "$process" ] && [ -O "$process/exe" ] || continue
        executable=$("$readlink_bin" "$process/exe" 2>/dev/null) || continue
        if [ "$executable" = "$binary (deleted)" ] && [ -x "$process/exe" ]; then
          exec "$process/exe" --summary
        fi
      done
    fi
  fi
  # A registration is a location hint, not a reason to ignore a new install.
  # Keep discovery read-only: do not rewrite it or restart the running daemon.
  stale_registration=yes
fi
binary=$(command -v coportd 2>/dev/null) || binary=''
case "$binary" in /*) if [ -f "$binary" ] && [ -x "$binary" ]; then exec "$binary" --summary; fi ;; esac
for binary in "$HOME/.local/bin/coportd" /usr/local/bin/coportd /opt/homebrew/bin/coportd /Applications/Coport.app/Contents/MacOS/coportd "$HOME/Applications/Coport.app/Contents/MacOS/coportd"; do
  if [ -f "$binary" ] && [ -x "$binary" ]; then exec "$binary" --summary; fi
done
if [ "${stale_registration:-}" = yes ]; then
  printf '%s\n' 'Registered Coport executable is unavailable and no replacement was found; restore or reinstall coportd on the destination.' >&2
  exit 127
fi
printf '%s\n' 'Coport not found; update and start Coport on the destination, or set an explicit executable path.' >&2
exit 127

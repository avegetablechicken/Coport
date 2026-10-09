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
  if [ ! -f "$binary" ] || [ ! -x "$binary" ]; then
    printf '%s\n' 'Registered Coport executable is unavailable; restart Coport on the destination.' >&2; exit 127
  fi
  exec "$binary" --summary
fi
binary=$(command -v coportd 2>/dev/null) || binary=''
case "$binary" in /*) if [ -f "$binary" ] && [ -x "$binary" ]; then exec "$binary" --summary; fi ;; esac
for binary in "$HOME/.local/bin/coportd" /usr/local/bin/coportd /opt/homebrew/bin/coportd /Applications/Coport.app/Contents/MacOS/coportd "$HOME/Applications/Coport.app/Contents/MacOS/coportd"; do
  if [ -f "$binary" ] && [ -x "$binary" ]; then exec "$binary" --summary; fi
done
printf '%s\n' 'Coport not found; update and start Coport on the destination, or set an explicit executable path.' >&2
exit 127

H=cc93ea6b
prev="__init__"
while true; do
  out=$(timeout 60 fno agents mail unread -n cc93ea6b 2>&1); rc=$?
  if [ $rc -ne 0 ]; then echo "PROBE-FAIL mail: exit $rc :: $(printf '%s' "$out" | tail -1)"; sleep 60; continue; fi
  if [ -z "$out" ]; then echo "PROBE-FAIL mail: empty stdout (reader may be broken)"; sleep 60; continue; fi
  cur=$(printf '%s' "$out" | grep -v 'merge-gating opt-out' | md5)
  if [ "$cur" != "$prev" ] && [ "$prev" != "__init__" ]; then
    echo "MAIL-CHANGE:"; printf '%s\n' "$out" | grep -v 'merge-gating opt-out' | head -40
  fi
  prev="$cur"; sleep 60
done

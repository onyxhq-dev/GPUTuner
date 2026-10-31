#!/bin/bash
# Re-stamp the <SHA256> of every repo file the .plg downloads. Run after editing anything in source/ or assets/.
set -euo pipefail
cd "$(dirname "$0")"
for f in source/* assets/logo.png; do
  h=$(sha256sum "$f" | cut -d' ' -f1)
  sed -i "\|&raw;/$f</URL>|{n;s|<SHA256>.*</SHA256>|<SHA256>$h</SHA256>|}" gputuner.plg
done
if grep -q '<SHA256>0</SHA256>' gputuner.plg; then echo "unstamped files remain"; exit 1; fi
echo "hashes updated"

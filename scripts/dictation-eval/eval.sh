#!/usr/bin/env bash
# Dictation eval loop: gen -> serve (read phrases aloud in Chrome, fix mistakes) -> grade; trend across runs.
set -euo pipefail
cd "$(dirname "$0")"
case "${1:-}" in
  gen) python3 gen.py ;;
  serve) python3 server.py "${2:?usage: eval.sh serve plain|pp}" ;;
  grade) python3 grade.py "${2:-}" ;;
  replay) python3 server.py "replay" "${2:?usage: eval.sh replay runs/<run>.jsonl}" ;;
  cases) python3 grade.py --cases ;;
  asr) python3 grade.py --asr ;;
  trend) python3 grade.py --trend ;;
  *) echo "usage: eval.sh gen | serve plain|pp | grade [runs/file.jsonl] | replay runs/<run>.jsonl | cases | asr | trend"; exit 1 ;;
esac

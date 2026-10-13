#!/bin/zsh
# kevctl.sh start <run> [temperature] | stop — one Kev server at a time (KEV_DIR=<kev checkout>).
# Used by the docs/kev_migration_research.md reproduce steps.
cd "${KEV_DIR:?set KEV_DIR to your Kev checkout}"
case $1 in
  start)
    curl -s -m 2 localhost:8009/v1/models >/dev/null && { echo "port 8009 busy — stop first"; exit 1; }
    T=${3:-}; [[ -n $T ]] && export KEV_TEMPERATURE=$T
    setsid_cmd=(uv run --extra serve python -m kev.serve --run "$2" --port 8009)
    nohup "${setsid_cmd[@]}" > serve-eval.log 2>&1 &!
    for i in {1..120}; do sleep 1; curl -s -m 1 localhost:8009/v1/models >/dev/null && break; done
    curl -s localhost:8009/v1/models | python3 -c "import sys,json;m=json.load(sys.stdin)['models'][0];print('serving',m['run'],'T=%.3f'%m['temperature'])" ;;
  stop)
    pkill -f "kev.serve"; for i in {1..30}; do curl -s -m 1 localhost:8009/v1/models >/dev/null || { echo stopped; exit 0; }; sleep 1; done; echo "still up!"; exit 1 ;;
esac

#!/usr/bin/env bash
# Apollo(髱朿eaxt)縺ｮ繧ｵ繝悶げ繝ｩ繝・+ Reaxt 2縺､ 繧・open-cosmo 縺ｮ繧ｲ繝ｼ繝医え繧ｧ繧､縺ｮ蠕後ｍ縺ｫ鄂ｮ縺丞ｮ滓ｩ溘・隧ｦ鬨・set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
REAXT="${REAXT:-$(cd "$HERE/../.." && pwd)}"; COSMO="${COSMO:-$REAXT/../open-cosmo}"
node "$HERE/server.mjs" >"$HERE/apollo.log" 2>&1 & PA=$!
"$REAXT/target/debug/serve.exe" "$REAXT/examples/counter/pkg" 8792 >/dev/null 2>&1 & P1=$!
"$REAXT/target/debug/serve_greeter.exe" 8793 >/dev/null 2>&1 & P2=$!
OPEN_RUNO_SUBGRAPHS="counter=http://127.0.0.1:8792/_reaxt/graphql,greeter=http://127.0.0.1:8793/_reaxt/graphql,apollo=http://127.0.0.1:8794/" \
OPEN_RUNO_BIND_ADDR=127.0.0.1:8099 "$COSMO/target/debug/open-runo-gateway.exe" >"$HERE/gw.log" 2>&1 & P3=$!
trap 'kill $PA $P1 $P2 $P3 2>/dev/null' EXIT
sleep 6
q() { curl -s -XPOST localhost:8099/graphql -H 'content-type: application/json' -H 'x-reaxt: 1' -d "$1"; }
echo "--- apollo alone";      q '{"query":"{ apolloHello }"}'; echo
echo "--- apollo + reaxt split"; q '{"query":"{ apolloHello a: demo_v1_Greeter_GetVisits }"}'; echo
echo "--- @key join across 3 servers (apollo nickname + greeter visits + counter count)"
q '{"query":"{ v: demo_v1_Greeter_GetVisitor(input:{id:\"x\"}) { id visits count nickname } }"}'; echo
echo "--- gateway log tail"; tail -5 "$HERE/gw.log"

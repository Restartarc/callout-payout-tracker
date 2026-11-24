#!/bin/sh
# Saves your RPC endpoint to .env (git-ignored, readable only by you).
# Input is hidden: the key is not shown on screen or written to shell history.
cd "$(dirname "$0")" || exit 1
trap 'stty echo 2>/dev/null' EXIT INT TERM
printf 'Paste your RPC endpoint URL (https://...), then press Enter: '
stty -echo
IFS= read -r raw
stty echo
printf '\n'

# Terminals wrap pastes in invisible control codes; keep only the first
# https:// URL in what was pasted.
# The terminal may split a long paste into chunks, each wrapped in its own
# markers, so strip every control sequence anywhere, then join the pieces.
url=$(printf '%s' "$raw" | LC_ALL=C tr -d '\033\r\n\t ' | sed -E 's/\[[0-9;?]*[A-Za-z~]//g' | grep -Eo 'https://[A-Za-z0-9._:/?#=&%-]+' | head -n 1)
if [ -z "$url" ]; then
  # Just a Helius API key (a UUID) pasted on its own: build the endpoint.
  key=$(printf '%s' "$raw" | LC_ALL=C tr -d '\033\r\n\t ' | sed -E 's/\[[0-9;?]*[A-Za-z~]//g' \
    | grep -Eoi '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}' | head -n 1)
  if [ -n "$key" ]; then
    url="https://mainnet.helius-rpc.com/?api-key=$key"
    echo "Got a Helius API key on its own; using the Helius mainnet endpoint."
  else
    echo "Didn't find an https:// URL or a Helius API key in what was pasted - nothing saved. Run this again."
    exit 1
  fi
fi

umask 077
printf 'RPC_URL=%s\n' "$url" > .env
host=$(printf '%s' "$url" | sed -E 's#^https://([^/?]+).*#\1#')
klen=$(printf '%s' "$url" | sed -n 's/.*api-key=//p' | tr -d '\n' | wc -c | tr -d ' ')
echo "Saved an endpoint on $host to .env (key not shown, ${klen} characters long). Checking it works..."

code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 20 -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getSlot"}' "$url")
status=$?
if [ "$status" -ne 0 ]; then
  echo "Could not reach the endpoint (curl error $status) - check your connection and run this again."
elif [ "$code" = "200" ]; then
  echo "Endpoint answered (HTTP 200). You can close this tab."
else
  echo "Endpoint returned HTTP $code - check the key and run this again."
fi

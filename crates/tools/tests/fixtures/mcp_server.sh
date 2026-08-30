#!/bin/sh
mode="${1:-normal}"
[ "$mode" = "exit" ] && exit 7
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"fixture","version":"1"}}}\n' "$id" ;;
    *'"method":"notifications/initialized"'*) ;;
    *'"method":"tools/list"'*)
      [ "$mode" = "hang_discovery" ] && sleep 30
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","description":"echo text","inputSchema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}},{"name":"verbose","description":"long text","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"outputLimitBytes":12}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      if [ "$mode" = "exit_on_call" ]; then
        exit 9
      elif [ "$mode" = "malformed" ]; then
        printf '{bad json\n'
      elif printf '%s' "$line" | grep -q '"name":"echo"'; then
        text=$(printf '%s' "$line" | sed -n 's/.*"text":"\([^"]*\)".*/\1/p')
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$text"
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}]}}\n' "$id"
      fi ;;
  esac
done

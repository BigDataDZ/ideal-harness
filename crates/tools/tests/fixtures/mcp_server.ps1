param([string]$Mode = "normal")
$ErrorActionPreference = "Stop"
if ($Mode -eq "exit") { exit 7 }
while (($line = [Console]::In.ReadLine()) -ne $null) {
    $request = $line | ConvertFrom-Json
    if ($request.method -eq "initialize") {
        [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $request.id + ',"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"fixture","version":"1"}}}')
    } elseif ($request.method -eq "notifications/initialized") {
        continue
    } elseif ($request.method -eq "tools/list") {
        [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $request.id + ',"result":{"tools":[{"name":"echo","description":"echo text","inputSchema":{"type":"object","required":["text"],"properties":{"text":{"type":"string"}},"additionalProperties":false}},{"name":"verbose","description":"long text","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"outputLimitBytes":12}]}}')
    } elseif ($request.method -eq "tools/call") {
        if ($Mode -eq "exit_on_call") {
            exit 9
        } elseif ($Mode -eq "malformed") {
            [Console]::Out.WriteLine('{bad json')
        } elseif ($request.params.name -eq "echo") {
            $text = ($request.params.arguments.text | ConvertTo-Json -Compress)
            [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $request.id + ',"result":{"content":[{"type":"text","text":' + $text + '}]}}')
        } else {
            [Console]::Out.WriteLine('{"jsonrpc":"2.0","id":' + $request.id + ',"result":{"content":[{"type":"text","text":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}]}}')
        }
    }
}

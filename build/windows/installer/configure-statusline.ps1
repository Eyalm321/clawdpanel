# configure-statusline.ps1 — invoked by NSIS install / uninstall hooks.
#
# Usage:
#   configure-statusline.ps1 install
#   configure-statusline.ps1 uninstall
#
# Reads $env:USERPROFILE\.claude\settings.json (creating {} if missing),
# sets or removes the `statuslineCommand` key, and writes back atomically.
# Other keys in the file are preserved. If the file exists but isn't valid
# JSON, the script logs a warning and exits 0 without modifying it.

param(
    [Parameter(Position = 0, Mandatory = $true)]
    [ValidateSet('install', 'uninstall')]
    [string]$Mode
)

$ErrorActionPreference = 'Stop'

# The statusline command captures Claude Code's rate-limit payloads to
# ~/.claude/rate_limits.json on every prompt. Keep this in sync with the
# command documented in README.md.
$StatuslineCommand = "node -e ""const fs=require('fs');const p=require('path');const os=require('os');const d=fs.readFileSync(0,'utf-8');if(d){const parsed=JSON.parse(d);fs.writeFileSync(p.join(os.homedir(),'.claude','rate_limits.json'),JSON.stringify({...parsed,captured_at:Date.now()}))}"""

$ClaudeDir = Join-Path $env:USERPROFILE '.claude'
$SettingsPath = Join-Path $ClaudeDir 'settings.json'

if (-not (Test-Path $ClaudeDir)) {
    New-Item -ItemType Directory -Force -Path $ClaudeDir | Out-Null
}

$settings = $null
if (Test-Path $SettingsPath) {
    try {
        $raw = Get-Content -Raw -LiteralPath $SettingsPath
        if ([string]::IsNullOrWhiteSpace($raw)) {
            $settings = [pscustomobject]@{}
        } else {
            $settings = $raw | ConvertFrom-Json -ErrorAction Stop
        }
    } catch {
        Write-Warning "ClawdPanel: $SettingsPath is not valid JSON; skipping statusline configuration."
        exit 0
    }
} else {
    $settings = [pscustomobject]@{}
}

# Promote to a hashtable so we can add/remove keys without reflection gymnastics.
$bag = @{}
$settings.PSObject.Properties | ForEach-Object { $bag[$_.Name] = $_.Value }

if ($Mode -eq 'install') {
    # "statusLine" is the key Claude Code actually reads; older installs wrote
    # the nonexistent "statuslineCommand" (cleaned up on both paths).
    $bag['statusLine'] = @{ type = 'command'; command = $StatuslineCommand }
    $bag.Remove('statuslineCommand') | Out-Null
} else {
    if ($bag['statusLine'] -is [hashtable] -or $bag['statusLine'] -is [pscustomobject]) {
        if ("$($bag['statusLine'].command)" -like '*rate_limits.json*') { $bag.Remove('statusLine') | Out-Null }
    }
    $bag.Remove('statuslineCommand') | Out-Null
}

# Serialize. ConvertTo-Json default depth is 2 — bump to be safe for nested user keys.
$json = $bag | ConvertTo-Json -Depth 32

$tmp = "$SettingsPath.tmp"
Set-Content -LiteralPath $tmp -Value $json -Encoding UTF8
Move-Item -Force -LiteralPath $tmp -Destination $SettingsPath
Write-Output "ClawdPanel: statusline $Mode complete ($SettingsPath)"

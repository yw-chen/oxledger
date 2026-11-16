# SPDX-License-Identifier: GPL-2.0-or-later
# Adds a user to the running deployment (deploy/README.md):
#   .\deploy\add-user.ps1 -Login admin -Name "Admin" -Admin
# Asks for the password without echoing it and writes it to the command's
# stdin as UTF-8 bytes (not through a PowerShell pipeline, which adds a BOM).
# ASCII only: Windows PowerShell reads a script without a BOM in the ANSI code page.
param(
    [Parameter(Mandatory)] [string] $Login,
    [Parameter(Mandatory)] [string] $Name,
    [switch] $Admin
)
$ErrorActionPreference = 'Stop'
$secure = Read-Host -AsSecureString "Password for $Login (at least 12 characters)"
$bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
try { $plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }

# Windows command-line quoting: double the backslashes before a quote and at
# the end (they would escape the closing quote), then escape the quote.
function Quote([string] $s) { '"' + (($s -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"' }
$argv = @('compose', '-f', "$PSScriptRoot\compose.yaml", 'exec', '-T', 'app', 'oxledger', 'user', 'add', $Login, $Name)
if ($Admin) { $argv += '--admin' }

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = 'docker'
$psi.Arguments = ($argv | ForEach-Object { Quote $_ }) -join ' '
$psi.UseShellExecute = $false
$psi.RedirectStandardInput = $true
$p = [System.Diagnostics.Process]::Start($psi)
$bytes = (New-Object System.Text.UTF8Encoding $false).GetBytes($plain)
$p.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
$p.StandardInput.Close()
$p.WaitForExit()
if ($p.ExitCode -ne 0) { throw "oxledger user add failed (exit $($p.ExitCode))" }

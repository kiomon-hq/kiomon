# SPDX-License-Identifier: Apache-2.0
#
# Install `kiomon` on Windows:
#
#   irm https://raw.githubusercontent.com/kiomon-hq/kiomon/main/install.ps1 | iex
#
# Or, to see what it would do first — the right way to run a script from the internet:
#
#   irm https://raw.githubusercontent.com/kiomon-hq/kiomon/main/install.ps1 -OutFile install.ps1
#   notepad install.ps1
#   .\install.ps1 -DryRun
#
# What it does, in order: works out which archive this machine needs, downloads it and the
# `SHA256SUMS` file from the same release, verifies one against the other, extracts the
# binary into a directory of its own, puts that directory on the *user* PATH (never the
# machine's: that would need an administrator), and prints the version it installed.
#
# Windows PowerShell 5.1 is the floor, which is what a clean Windows 10/11 ships: no
# PowerShell 7 needed, no administrator, and nothing from the Microsoft Store.

[CmdletBinding()]
param(
    # Release tag to install, e.g. `v0.1.0`. The default resolves the newest release.
    [string]$Version = "latest",
    # Where the binary goes. Default mirrors `%LOCALAPPDATA%\Programs` style installs.
    [string]$Dir = "",
    # Install for this target instead of the machine's own. Only useful for staging.
    [string]$Target = "",
    # Release host: a mirror, an internal artifact store, or a local directory of files.
    [string]$ReleasesUrl = "",
    [switch]$NoVerify,
    # Do not touch PATH. The binary is still installed and still runs by full path.
    [switch]$NoPath,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

# Windows PowerShell 5.1 defaults to TLS 1.0 against github.com, which no longer speaks it,
# and the failure reads as a DNS problem. PowerShell 7 negotiates on its own and ignores
# this.
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {
    # A .NET that does not know the name is a .NET new enough not to need it.
}

function Write-Note([string]$Message) { Write-Host $Message }

function Get-HostTarget {
    $arch = ""
    try {
        $arch = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    } catch {
        $arch = $env:PROCESSOR_ARCHITECTURE
    }
    switch ($arch.ToUpperInvariant()) {
        "X64" { return "x86_64-pc-windows-msvc" }
        "AMD64" { return "x86_64-pc-windows-msvc" }
        "ARM64" {
            throw "no build for Windows on ARM64 yet. Build from source: cargo install --path crates/kiomon-cli"
        }
        "X86" {
            throw "no 32-bit Windows build. Build from source: cargo install --path crates/kiomon-cli"
        }
        default { throw "unrecognised processor architecture '$arch'. Build from source: cargo install --path crates/kiomon-cli" }
    }
}

try {
    $hostTarget = Get-HostTarget
} catch {
    $hostTarget = ""
    if (-not $Target) { throw }
}

if (-not $Target) { $Target = $hostTarget }
if ($Target -notlike "*-pc-windows-*") {
    throw "$Target is not a Windows target; use install.sh on macOS or Linux instead"
}
if (-not $Dir) { $Dir = Join-Path $env:LOCALAPPDATA "kiomon\bin" }

if (-not $ReleasesUrl) {
    $ReleasesUrl = "https://github.com/kiomon-hq/kiomon/releases"
}
$ReleasesUrl = $ReleasesUrl.TrimEnd("/")

$asset = "kiomon-$Target.zip"
if ($Version -eq "latest") {
    $url = "$ReleasesUrl/latest/download/$asset"
    $sumsUrl = "$ReleasesUrl/latest/download/SHA256SUMS"
} else {
    $url = "$ReleasesUrl/download/$Version/$asset"
    $sumsUrl = "$ReleasesUrl/download/$Version/SHA256SUMS"
}

Write-Note "kiomon $Version for $Target"
Write-Note "  from   $url"
Write-Note "  to     $(Join-Path $Dir 'kiomon.exe')"

if ($DryRun) {
    Write-Note "  sums   $sumsUrl"
    Write-Note "dry run: nothing downloaded, nothing written"
    exit 0
}

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("kiomon-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    $archive = Join-Path $tmp $asset
    Write-Note "  downloading"
    Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing

    if (-not $NoVerify) {
        $sums = Join-Path $tmp "SHA256SUMS"
        Invoke-WebRequest -Uri $sumsUrl -OutFile $sums -UseBasicParsing
        $expected = $null
        foreach ($line in Get-Content $sums) {
            # `<sha>  <name>`, exactly as `sha256sum` writes it — and `<sha> *<name>` from
            # its Windows build, hence the trim. The name is compared as a whole field so a
            # lookalike asset cannot satisfy the check.
            $fields = $line -split '\s+', 2
            if ($fields.Count -eq 2 -and $fields[1].Trim().TrimStart('*') -eq $asset) {
                $expected = $fields[0].ToLowerInvariant()
                break
            }
        }
        if (-not $expected) { throw "$sumsUrl has no line for $asset" }
        $actual = (Get-FileHash -Path $archive -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            throw "checksum mismatch for ${asset}: expected $expected, got $actual. Nothing was installed."
        }
        Write-Note "  sha256 verified"
    }

    $unpack = Join-Path $tmp "unpack"
    Expand-Archive -Path $archive -DestinationPath $unpack -Force
    $binary = Get-ChildItem -Path $unpack -Filter "kiomon.exe" -Recurse -File |
        Select-Object -First 1
    if (-not $binary) { throw "$asset does not contain kiomon.exe" }

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    $installed = Join-Path $Dir "kiomon.exe"
    Copy-Item -Path $binary.FullName -Destination $installed -Force

    if ($Target -eq $hostTarget) {
        $reported = (& $installed --version) 2>&1
        Write-Note "installed $reported"
    } else {
        Write-Note "installed $installed (not executed: built for $Target)"
    }

    if (-not $NoPath) {
        # The *user* PATH, because setting the machine's needs elevation and this script
        # does not. Read and written as a raw string: `[Environment]::GetEnvironmentVariable`
        # expands `%VAR%` on read, and writing that back would corrupt somebody's PATH.
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $entries = @()
        if ($userPath) { $entries = $userPath.Split(";") | Where-Object { $_ } }
        if ($entries -notcontains $Dir) {
            $updated = (@($entries) + $Dir) -join ";"
            [Environment]::SetEnvironmentVariable("Path", $updated, "User")
            Write-Note ""
            Write-Note "Added $Dir to your user PATH. Open a new terminal to use it everywhere;"
            Write-Note "in this one, run:  `$env:Path = `"$Dir;`$env:Path`""
        }
        # Make the "Next:" line below work in this session too.
        $env:Path = "$Dir;$env:Path"
    }

    Write-Note ""
    Write-Note "Next: kiomon init; kiomon add --kind semantic --title `"first memory`"; kiomon search first"
} finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

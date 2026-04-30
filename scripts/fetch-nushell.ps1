# Windows counterpart of fetch-nushell.sh.
# Usage:
#   .\scripts\fetch-nushell.ps1
#   $env:TARGET_TRIPLE = 'aarch64-pc-windows-msvc'; .\scripts\fetch-nushell.ps1
#   $env:NU_VERSION = '0.99.1'; .\scripts\fetch-nushell.ps1

$ErrorActionPreference = 'Stop'

$NuVersion = if ($env:NU_VERSION) { $env:NU_VERSION } else { '0.99.1' }

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Resolve-Path (Join-Path $ScriptDir '..')
$DestDir   = Join-Path $RepoRoot 'crates/athen-app/binaries'

$Triple = if ($env:TARGET_TRIPLE) { $env:TARGET_TRIPLE } else {
    (rustc -vV | Select-String -Pattern '^host: (.+)$').Matches.Groups[1].Value
}
if (-not $Triple) { throw "cannot determine target triple; set TARGET_TRIPLE" }

if ($Triple -like '*-pc-windows-*') {
    $ArchiveExt = 'zip'
    $BinExt     = '.exe'
} else {
    $ArchiveExt = 'tar.gz'
    $BinExt     = ''
}

$Archive = "nu-$NuVersion-$Triple.$ArchiveExt"
$Url     = "https://github.com/nushell/nushell/releases/download/$NuVersion/$Archive"
$DestBin = Join-Path $DestDir "nu-$Triple$BinExt"

if (Test-Path $DestBin) {
    Write-Host "fetch-nushell: $DestBin already present, skipping"
    exit 0
}

New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
$Tmp = New-Item -ItemType Directory -Path (Join-Path $env:TEMP "nu-fetch-$([guid]::NewGuid())")

try {
    Write-Host "fetch-nushell: downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile (Join-Path $Tmp $Archive)

    if ($ArchiveExt -eq 'zip') {
        Expand-Archive -Path (Join-Path $Tmp $Archive) -DestinationPath $Tmp
    } else {
        tar -xzf (Join-Path $Tmp $Archive) -C $Tmp
    }

    $SrcBin = Get-ChildItem -Path $Tmp -Recurse -Filter "nu$BinExt" | Select-Object -First 1
    if (-not $SrcBin) { throw "could not locate nu$BinExt inside $Archive" }

    Copy-Item $SrcBin.FullName $DestBin -Force
    Write-Host "fetch-nushell: wrote $DestBin"
} finally {
    Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

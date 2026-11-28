# Installs the forkrail CLI on Windows: the newest release's forkrail.exe for this computer, checked
# against its SHA-256, into %LOCALAPPDATA%\Programs\forkrail (or FORKRAIL_BIN), which is added to
# your PATH. Then `forkrail install` sets forkrail up in your own AWS account.
#
#   irm https://forkrail.com/install.ps1 | iex
#
# forkrail.com/install.ps1 redirects to this script in the newest release:
# https://github.com/soorower/forkrail-cli/releases/latest/download/install.ps1
#
# For Windows PowerShell 5.1 (every Windows 10 and 11) and PowerShell 7. It runs in the caller's
# own session (iex), so it never calls `exit`, which would close their window: a failure throws.
# ASCII only: Windows PowerShell reads a script without a byte order mark as the system's code page.

& {
  $ErrorActionPreference = 'Stop'
  # Windows PowerShell redraws its progress bar for every block, which slows a download to a crawl.
  $ProgressPreference = 'SilentlyContinue'
  # GitHub takes TLS 1.2 or newer; an older .NET under Windows PowerShell may not offer it unasked.
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $repo = if ($env:FORKRAIL_REPO) { $env:FORKRAIL_REPO } else { 'soorower/forkrail-cli' }
  $bin = if ($env:FORKRAIL_BIN) { $env:FORKRAIL_BIN } else { Join-Path $env:LOCALAPPDATA 'Programs\forkrail' }
  $bin = $bin.TrimEnd('\')

  # A 32-bit PowerShell on 64-bit Windows names the machine in PROCESSOR_ARCHITEW6432.
  $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  switch ($arch) {
    'AMD64' { $target = 'x86_64-pc-windows-gnu'; $name = 'x64' }
    'ARM64' { $target = 'aarch64-pc-windows-gnullvm'; $name = 'Arm64' }
    default { throw "forkrail has no build for Windows on $arch yet." }
  }

  $url = "https://github.com/$repo/releases/latest/download/forkrail-$target.zip"
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ("forkrail-" + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $tmp | Out-Null
  try {
    Write-Host "Downloading forkrail for Windows ($name)..."
    $zip = Join-Path $tmp 'forkrail.zip'
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
    Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256" -OutFile (Join-Path $tmp 'sum')
    $want = ((Get-Content -Raw (Join-Path $tmp 'sum')).Trim() -split '\s+')[0].ToLowerInvariant()
    $got = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
    if ($want -ne $got) {
      throw "The download doesn't match its SHA-256. Nothing was installed; try again."
    }
    Expand-Archive -Path $zip -DestinationPath (Join-Path $tmp 'forkrail') -Force
    New-Item -ItemType Directory -Path $bin -Force | Out-Null
    try {
      Copy-Item -Path (Join-Path $tmp 'forkrail\forkrail.exe') -Destination (Join-Path $bin 'forkrail.exe') -Force
    } catch {
      throw "Couldn't replace $bin\forkrail.exe. If forkrail is running, wait for it to finish or close it, then try again."
    }
  } finally {
    Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
  }

  $exe = Join-Path $bin 'forkrail.exe'
  $version = $null
  try { $version = & $exe --version } catch { }
  if (-not $version) { $version = 'forkrail' }
  Write-Host "Installed $version in $bin."

  # On the PATH for new terminals (the user's own PATH, kept as it's stored: entries such as
  # %USERPROFILE%\bin stay unexpanded) and for this one.
  $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
  try {
    $stored = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $entries = @($stored -split ';' | Where-Object { $_ -ne '' })
    if (@($entries | ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') }) -notcontains $bin) {
      $key.SetValue('Path', (@($entries + $bin) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
      # Setting a variable through .NET tells Windows the environment changed, so terminals opened
      # from now on see the new PATH; the variable itself goes again at once.
      [Environment]::SetEnvironmentVariable('FORKRAIL_PATH_CHANGED', '1', 'User')
      [Environment]::SetEnvironmentVariable('FORKRAIL_PATH_CHANGED', $null, 'User')
      Write-Host "Added $bin to your PATH."
    }
  } finally {
    $key.Close()
  }
  if (@($env:Path -split ';' | ForEach-Object { $_.TrimEnd('\') }) -notcontains $bin) {
    $env:Path = "$bin;$env:Path"
  }
  Write-Host 'Next, run: forkrail install'
}

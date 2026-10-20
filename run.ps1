$ErrorActionPreference = 'Stop'
Push-Location $PSScriptRoot
try {
    $launcherExecutable = Join-Path $PSScriptRoot 'tui-launcher.exe'
    if (-not (Test-Path -LiteralPath $launcherExecutable)) {
        $launcherExecutable = Join-Path $PSScriptRoot 'target\release\tui-launcher.exe'
    }
    if (Test-Path -LiteralPath $launcherExecutable) {
        & $launcherExecutable @args
        if ($LASTEXITCODE -ne 0) { throw "启动失败，退出代码 $LASTEXITCODE" }
        return
    }
    if (Test-Path -LiteralPath "$PSScriptRoot\.tools\cargo\bin\cargo.exe") {
        $env:RUSTUP_HOME = "$PSScriptRoot\.tools\rustup"
        $env:CARGO_HOME = "$PSScriptRoot\.tools\cargo"
        $env:PATH = "$PSScriptRoot\.tools\cargo\bin;$env:PATH"
    }
    cargo run --locked --release -- @args
    if ($LASTEXITCODE -ne 0) { throw "启动失败，退出代码 $LASTEXITCODE" }
} finally {
    Pop-Location
}

param(
    [ValidateSet("lint", "check")]
    [string]$Mode = "check"
)

$ErrorActionPreference = "Stop"

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "command failed with exit code $LASTEXITCODE`: $Command $($Arguments -join ' ')"
    }
}

function Invoke-CargoWithZigCacheRecovery {
    param([string[]]$Arguments)

    & cargo @Arguments
    if ($LASTEXITCODE -eq 0) {
        return
    }

    Write-Warning "cargo compile failed; clearing Zig build caches and retrying once"
    Remove-Item -Recurse -Force .zig-cache -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force vendor/libghostty-vt/.zig-cache -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force vendor/libghostty-vt/zig-out -ErrorAction SilentlyContinue
    Invoke-Checked cargo $Arguments
}

function Invoke-CargoTestFilter {
    param(
        [Parameter(Mandatory)]
        [string]$Filter,
        [switch]$Exact
    )

    $commonArguments = @(
        "test",
        "--locked",
        "--target",
        "x86_64-pc-windows-msvc",
        "--bin",
        "herdr",
        $Filter
    )
    $harnessArguments = @("--list")
    if ($Exact) {
        $harnessArguments += "--exact"
    }

    $listArguments = $commonArguments + @("--") + $harnessArguments
    $listOutput = @(& cargo @listArguments)
    if ($LASTEXITCODE -ne 0) {
        throw "could not enumerate tests for filter '$Filter': $($listOutput -join [Environment]::NewLine)"
    }

    $testNames = @(
        foreach ($line in $listOutput) {
            $match = [regex]::Match([string]$line, '^\s*(\S+): test\s*$')
            if ($match.Success) {
                $match.Groups[1].Value
            }
        }
    )
    if ($testNames.Count -eq 0) {
        throw "test filter '$Filter' selected zero tests"
    }

    Write-Host "Running $($testNames.Count) test(s) for '$Filter'"
    $runArguments = $commonArguments
    if ($Exact) {
        $runArguments += @("--", "--exact")
    }
    Invoke-Checked cargo $runArguments
}

function Invoke-CargoIntegrationTest {
    <#
    .SYNOPSIS
    Run one integration test target on Windows, and prove a named test in it
    actually executed.

    .DESCRIPTION
    Invoke-CargoTestFilter hardcodes `--bin herdr`, so it can only reach unit
    tests compiled into the binary; a `tests/*.rs` target is unreachable
    through it at any filter. That gap is invisible in a green run - Windows CI
    reported success while never compiling the target at all.

    RequiredTest is the non-vacuity guard. A test body behind
    `#[cfg(not(unix))]` that stops compiling on Windows removes itself from the
    listing rather than failing, so "the suite passed" and "the platform has no
    coverage" produce identical output. Naming the test makes its absence an
    error.
    #>
    param(
        [Parameter(Mandatory)]
        [string]$Target,
        [Parameter(Mandatory)]
        [string]$RequiredTest
    )

    $commonArguments = @(
        "test",
        "--locked",
        "--target",
        "x86_64-pc-windows-msvc",
        "--test",
        $Target
    )

    $listArguments = $commonArguments + @("--", "--list")
    $listOutput = @(& cargo @listArguments)
    if ($LASTEXITCODE -ne 0) {
        throw "could not enumerate tests in target '$Target': $($listOutput -join [Environment]::NewLine)"
    }

    $testNames = @(
        foreach ($line in $listOutput) {
            $match = [regex]::Match([string]$line, '^\s*(\S+): test\s*$')
            if ($match.Success) {
                $match.Groups[1].Value
            }
        }
    )
    if ($testNames.Count -eq 0) {
        throw "integration target '$Target' selected zero tests"
    }
    if ($testNames -notcontains $RequiredTest) {
        throw "integration target '$Target' does not contain '$RequiredTest'; it was cfg'd out rather than run, so this platform has no coverage for it. Found: $($testNames -join ', ')"
    }

    Write-Host "Running $($testNames.Count) test(s) in target '$Target' (requires '$RequiredTest')"
    Invoke-Checked cargo $commonArguments
}

Invoke-Checked rustup @("target", "add", "x86_64-pc-windows-msvc")
Invoke-Checked cargo @("fmt", "--check")
Invoke-CargoWithZigCacheRecovery @(
    "clippy",
    "--bin",
    "herdr",
    "--locked",
    "--target",
    "x86_64-pc-windows-msvc",
    "--",
    "-D",
    "warnings"
)

if ($Mode -eq "lint") {
    return
}

Invoke-CargoTestFilter "windows_"
Invoke-CargoTestFilter "server::client_transport::tests"
Invoke-CargoTestFilter "app::tests::native_repeats_and_releases_follow_the_pressed_pane" -Exact
# The incarnation token's Windows behaviour: no live-handoff path exists there,
# so the token must NOT rotate. That test is the only coverage this platform
# has for the feature, and until this line it was never compiled.
Invoke-CargoIntegrationTest -Target "server_epoch" -RequiredTest "the_token_is_stable_where_live_handoff_is_unsupported"
Invoke-Checked cargo @("build", "--locked", "--target", "x86_64-pc-windows-msvc")

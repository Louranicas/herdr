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

function Get-CargoTestNames {
    <#
    .SYNOPSIS
    Enumerate the tests a cargo invocation would run.

    .DESCRIPTION
    One definition of the enumeration contract - the `--list` call, its exit
    status, the harness listing format, and the refusal to treat an empty
    selection as a pass - so a correction to any of them lands everywhere.
    #>
    param(
        [Parameter(Mandatory)]
        [string[]]$Arguments,
        [Parameter(Mandatory)]
        [string]$Description
    )

    $listOutput = @(& cargo @Arguments)
    if ($LASTEXITCODE -ne 0) {
        throw "could not enumerate tests for $Description`: $($listOutput -join [Environment]::NewLine)"
    }

    $names = @(
        foreach ($line in $listOutput) {
            $match = [regex]::Match([string]$line, '^\s*(\S+): test\s*$')
            if ($match.Success) {
                $match.Groups[1].Value
            }
        }
    )
    if ($names.Count -eq 0) {
        throw "$Description selected zero tests"
    }

    # No leading comma: both callers wrap this in @(), which already keeps a
    # single name an array. Returning ,$names on top of that nests the array
    # inside a one-element array, so -contains and .Count silently stop working.
    return $names
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
    $testNames = @(Get-CargoTestNames -Arguments $listArguments -Description "test filter '$Filter'")

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
    Run one named test out of an integration test target on Windows, and prove
    that test actually exists there.

    .DESCRIPTION
    Invoke-CargoTestFilter hardcodes `--bin herdr`, so it can only reach unit
    tests compiled into the binary; a `tests/*.rs` target is unreachable
    through it at any filter. That gap is invisible in a green run - Windows CI
    reported success while never compiling the target at all.

    RequiredTest is both the non-vacuity guard and the entire run. A test body
    behind `#[cfg(not(unix))]` that stops compiling on Windows removes itself
    from the listing rather than failing, so "the suite passed" and "the
    platform has no coverage" produce identical output. Naming the test makes
    its absence an error.

    The run is scoped to RequiredTest, not the whole target. The target still
    compiles in full, so a Windows build break is still caught, while the job's
    runtime stays bounded by tests known to pass on this platform rather than
    by whatever else the target happens to contain. Widening coverage here is a
    deliberate act: name each further test once it is known to pass on Windows.
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

    $description = "integration target '$Target'"
    $listArguments = $commonArguments + @("--", "--list")
    $testNames = @(Get-CargoTestNames -Arguments $listArguments -Description $description)
    if ($testNames -notcontains $RequiredTest) {
        throw "$description does not contain '$RequiredTest'; it was cfg'd out rather than run, so this platform has no coverage for it. Found: $($testNames -join ', ')"
    }

    Write-Host "Running '$RequiredTest' from $description"
    Invoke-Checked cargo ($commonArguments + @("--", "--exact", $RequiredTest))
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
# The Windows update path installs through upstream's script rather than the
# asset a manifest names, so a configured update source must be refused before
# it is reached. Only Windows can prove that, and only by running.
Invoke-CargoIntegrationTest -Target "server_epoch" -RequiredTest "a_configured_update_source_never_reaches_the_windows_installer"
Invoke-Checked cargo @("build", "--locked", "--target", "x86_64-pc-windows-msvc")

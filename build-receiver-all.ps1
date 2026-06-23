# build-receiver-all.ps1
# This script compiles the AirMic.Receiver application for all five supported platforms as self-contained single-file executables
# and places a copy of each in the root of the "publish" directory.

# Ensure we run from the script directory
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Definition
if ($scriptDir) {
    Set-Location $scriptDir
}

Write-Host "=== AirMic.Receiver Multi-Platform Build ===" -ForegroundColor Cyan

# Define publish directory
$publishDir = Join-Path (Get-Location) "publish"
Write-Host "[*] Creating publish directory at: $publishDir" -ForegroundColor Gray

# Ensure publish folder exists (but clean up first to ensure fresh builds)
if (Test-Path $publishDir) {
    Write-Host "[*] Cleaning existing publish directory..." -ForegroundColor Gray
    Remove-Item -Path $publishDir -Recurse -Force -ErrorAction SilentlyContinue
}
New-Item -ItemType Directory -Path $publishDir -Force | Out-Null

# List of target platforms and target binaries
$platforms = @(
    @{ RID = "win-x64"; Binary = "AirMic.Receiver.exe"; TargetName = "AirMic.Receiver-win-x64.exe" },
    @{ RID = "win-arm64"; Binary = "AirMic.Receiver.exe"; TargetName = "AirMic.Receiver-win-arm64.exe" },
    @{ RID = "osx-arm64"; Binary = "AirMic.Receiver"; TargetName = "AirMic.Receiver-osx-arm64" },
    @{ RID = "osx-x64"; Binary = "AirMic.Receiver"; TargetName = "AirMic.Receiver-osx-x64" }
)

$successCount = 0

foreach ($platform in $platforms) {
    $rid = $platform.RID
    $targetName = $platform.TargetName
    $binary = $platform.Binary
    
    Write-Host "`n[*] Building for target: $rid..." -ForegroundColor Yellow
    
    $outputPath = Join-Path $publishDir $rid
    
    # Run dotnet publish
    dotnet publish src/AirMic.Receiver/AirMic.Receiver.csproj -c Release -r $rid --self-contained true -p:PublishSingleFile=true -o $outputPath
    
    if ($LASTEXITCODE -eq 0) {
        $sourceFile = Join-Path $outputPath $binary
        $destFile = Join-Path $publishDir $targetName
        
        if (Test-Path $sourceFile) {
            Copy-Item -Path $sourceFile -Destination $destFile -Force
            Write-Host "[+] SUCCESS: Copied $binary to $destFile" -ForegroundColor Green
            $successCount++
        } else {
            Write-Host "[!] Error: Binary not found at $sourceFile" -ForegroundColor Red
        }
    } else {
        Write-Host "[!] Error: Build failed for target $rid" -ForegroundColor Red
    }
}

Write-Host "`n=== Build Process Finished ===" -ForegroundColor Cyan
if ($successCount -eq $platforms.Count) {
    Write-Host "[+] All targets compiled and copied successfully!" -ForegroundColor Green
} else {
    Write-Host "[!] Warning: ($($platforms.Count - $successCount)) target(s) failed to build." -ForegroundColor Yellow
}

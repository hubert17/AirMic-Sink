param(
    [int]$Port = 8443,
    [string]$SiteName = "AirMicServer",
    [string]$AppPoolName = "AirMicPool",
    [string]$PhysicalPath = "c:\Users\Gabs\GitHub\hubert17\AirMic-Sink\src\AirMic.Server\bin\Release\net10.0\publish"
)

# Check Administrator Elevation
$currentPrincipal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $currentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    Write-Error "This script must be run as an Administrator. Please reopen PowerShell as Administrator."
    exit 1
}

Write-Host "Configuring IIS for AirMic.Server..." -ForegroundColor Cyan

# Import IIS WebAdministration module
Import-Module WebAdministration -ErrorAction SilentlyContinue

# Ensure IIS Drive exists
if (-not (Test-Path "IIS:\")) {
    Write-Error "IIS Provider is not available. Please ensure IIS is installed and enabled."
    exit 1
}

# 1. Clean up existing site if it exists
if (Test-Path "IIS:\Sites\$SiteName") {
    Write-Host "Stopping and removing existing website '$SiteName'..." -ForegroundColor Yellow
    Remove-Item "IIS:\Sites\$SiteName" -Recurse -Force -ErrorAction SilentlyContinue
}

# 2. Clean up existing app pool if it exists
if (Test-Path "IIS:\AppPools\$AppPoolName") {
    Write-Host "Stopping and removing existing App Pool '$AppPoolName'..." -ForegroundColor Yellow
    Remove-Item "IIS:\AppPools\$AppPoolName" -Recurse -Force -ErrorAction SilentlyContinue
}

# 3. Create App Pool (No Managed Code)
Write-Host "Creating App Pool '$AppPoolName'..." -ForegroundColor Green
$pool = New-Item "IIS:\AppPools\$AppPoolName"
Set-ItemProperty "IIS:\AppPools\$AppPoolName" -Name "managedRuntimeVersion" -Value ""

# 4. Ensure physical path exists
if (-not (Test-Path $PhysicalPath)) {
    Write-Host "Physical path '$PhysicalPath' does not exist yet. Creating directory..." -ForegroundColor Yellow
    New-Item -ItemType Directory -Force -Path $PhysicalPath | Out-Null
}

# 5. Create Website
Write-Host "Creating Website '$SiteName' bound to port $Port..." -ForegroundColor Green
$site = New-Item "IIS:\Sites\$SiteName" -bindings @{protocol="http";bindingInformation="*:$(${Port}):"} -physicalPath $PhysicalPath
Set-ItemProperty "IIS:\Sites\$SiteName" -Name "applicationPool" -Value $AppPoolName

# 6. Set folder permissions so IIS AppPool Identity can read/execute files
Write-Host "Setting folder permissions for AppPool Identity..." -ForegroundColor Green
try {
    $acl = Get-Acl $PhysicalPath
    # Grant Read & Execute to the IIS AppPool user
    $rule = New-Object System.Security.AccessControl.FileSystemAccessRule("IIS AppPool\$AppPoolName", "ReadAndExecute, ListDirectory, Read", "ContainerInherit, ObjectInherit", "None", "Allow")
    $acl.AddAccessRule($rule)
    Set-Acl $PhysicalPath $acl
} catch {
    Write-Host "Could not set ACL via .NET. Attempting command line fallback..." -ForegroundColor Yellow
}

# Run icacls to ensure permissions propagate correctly
icacls "$PhysicalPath" /grant "IIS AppPool\${AppPoolName}:(OI)(CI)(RX)" /Q /T

# 7. Start AppPool and Site
Write-Host "Starting App Pool and Website..." -ForegroundColor Green
Start-WebAppPool -Name $AppPoolName -ErrorAction SilentlyContinue
Start-Website -Name $SiteName -ErrorAction SilentlyContinue

Write-Host "`nIIS Setup completed successfully!" -ForegroundColor Green
Write-Host "Website: '$SiteName'" -ForegroundColor Cyan
Write-Host "Binding Port: $Port" -ForegroundColor Cyan
Write-Host "Physical Path: $PhysicalPath" -ForegroundColor Cyan
Write-Host "Local URL: http://localhost:$Port" -ForegroundColor Cyan

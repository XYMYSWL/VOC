# 把编译好的 voc-client.exe 打成签名的 MSIX 安装包。
#
#   .\deploy\windows\build-msix.ps1                       # 用 dist\voc-client.exe
#   .\deploy\windows\build-msix.ps1 -Exe path\to\voc-client.exe
#
# 签名证书：第一次运行时自动生成自签名证书（CN=VOC），私钥只保存在本机
#   %USERPROFILE%\.voc-signing\VOC.pfx （绝不要提交到 Git）
# 公钥证书 VOC.cer 会放进 dist\，随安装包一起发给别人。
param(
    [string]$Exe = "",
    [string]$Out = ""
)
$ErrorActionPreference = "Stop"
$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if (-not $Exe) { $Exe = Join-Path $root "dist\voc-client.exe" }
if (-not $Out) { $Out = Join-Path $root "dist" }
$Publisher = "CN=VOC"

# 版本号取自 Cargo.toml（MSIX 需要四段式）
$ver = (Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$msixVer = "$ver.0"

# Windows SDK 工具
$sdk = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Directory | Where-Object { Test-Path (Join-Path $_.FullName "x64\makeappx.exe") } | Sort-Object Name -Descending | Select-Object -First 1
if (-not $sdk) { throw "找不到 Windows SDK（makeappx.exe）。请安装 Windows 10/11 SDK。" }
$makeappx = Join-Path $sdk.FullName "x64\makeappx.exe"
$signtool = Join-Path $sdk.FullName "x64\signtool.exe"

# 签名证书
$keyDir = Join-Path $env:USERPROFILE ".voc-signing"
$pfx = Join-Path $keyDir "VOC.pfx"
$pwdFile = Join-Path $keyDir "pfx-password.txt"
if (-not (Test-Path $pfx)) {
    Write-Host "生成自签名代码签名证书 $Publisher ..."
    New-Item -ItemType Directory -Force $keyDir | Out-Null
    $cert = New-SelfSignedCertificate -Type Custom -Subject $Publisher -KeyUsage DigitalSignature -FriendlyName "VOC Code Signing" `
        -CertStoreLocation "Cert:\CurrentUser\My" -NotAfter (Get-Date).AddYears(10) `
        -TextExtension @("2.5.29.37={text}1.3.6.1.5.5.7.3.3", "2.5.29.19={text}")
    $plain = [Convert]::ToBase64String((1..24 | ForEach-Object { Get-Random -Maximum 256 }) -as [byte[]])
    Set-Content -Path $pwdFile -Value $plain -NoNewline
    $sec = ConvertTo-SecureString $plain -AsPlainText -Force
    Export-PfxCertificate -Cert $cert -FilePath $pfx -Password $sec | Out-Null
    Remove-Item "Cert:\CurrentUser\My\$($cert.Thumbprint)"
}
$pfxPwd = Get-Content $pwdFile -Raw
$pfxObj = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($pfx, $pfxPwd)

# 组装包目录
$stage = Join-Path ([IO.Path]::GetTempPath()) "voc-msix-stage"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force (Join-Path $stage "Assets") | Out-Null
Copy-Item $Exe (Join-Path $stage "VOC.exe")
Copy-Item (Join-Path $PSScriptRoot "Assets\*.png") (Join-Path $stage "Assets")
Remove-Item (Join-Path $stage "Assets\icon-256.png")
(Get-Content (Join-Path $PSScriptRoot "AppxManifest.xml") -Raw).Replace("{VERSION}", $msixVer).Replace("{PUBLISHER}", $Publisher) |
    Set-Content -Path (Join-Path $stage "AppxManifest.xml") -Encoding utf8

New-Item -ItemType Directory -Force $Out | Out-Null
$msix = Join-Path $Out "VOC-$ver.msix"
& $makeappx pack /d $stage /p $msix /o /nv | Out-Null
if ($LASTEXITCODE -ne 0) { throw "makeappx 打包失败" }
& $signtool sign /fd SHA256 /f $pfx /p $pfxPwd $msix | Out-Null
if ($LASTEXITCODE -ne 0) { throw "signtool 签名失败" }

# 公钥证书（给别人信任用）
$cer = Join-Path $Out "VOC.cer"
[IO.File]::WriteAllBytes($cer, $pfxObj.Export([System.Security.Cryptography.X509Certificates.X509ContentType]::Cert))
Copy-Item (Join-Path $PSScriptRoot "Install-VOC.cmd") $Out -Force
Copy-Item (Join-Path $PSScriptRoot "install.ps1") $Out -Force
Remove-Item -Recurse -Force $stage
Write-Host "完成：$msix（版本 $msixVer，签名 $Publisher，证书指纹 $($pfxObj.Thumbprint)）"

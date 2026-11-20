# VOC 一键安装：信任 VOC 的签名证书（只放进「受信任人」，不是根证书），然后安装 / 升级 MSIX。
# 用法：双击同目录的「Install-VOC.cmd」即可（会请求一次管理员权限，用来导入证书）。
param([switch]$Elevated)
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$cer = Join-Path $here "VOC.cer"
$msix = Get-ChildItem $here -Filter "VOC-*.msix" | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $msix) { Write-Host "找不到 VOC-*.msix，请把安装包和本脚本放在同一个文件夹。"; Read-Host "按回车退出"; exit 1 }

$cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($cer)
$trusted = Get-ChildItem Cert:\LocalMachine\TrustedPeople | Where-Object { $_.Thumbprint -eq $cert.Thumbprint }
if (-not $trusted) {
    $admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if (-not $admin) {
        Write-Host "第一次安装需要信任 VOC 的签名证书，正在请求管理员权限……"
        Start-Process powershell -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$($MyInvocation.MyCommand.Path)`" -Elevated" -Wait
        exit
    }
    Write-Host "信任证书：$($cert.Subject)（指纹 $($cert.Thumbprint)）"
    Import-Certificate -FilePath $cer -CertStoreLocation Cert:\LocalMachine\TrustedPeople | Out-Null
}

Write-Host "安装 $($msix.Name) ……"
Add-AppxPackage -Path $msix.FullName -ForceApplicationShutdown
Write-Host "安装完成！在开始菜单里搜索「VOC」即可打开。"
if ($Elevated) { Read-Host "按回车关闭" }

# 一键编译发布版：
#   dist\voc-client.exe / voc-server.exe            Windows 客户端 / 服务端
#   dist\VOC-Setup.exe                               单文件安装程序（内含下面的 MSIX）
#   dist\VOC-<版本>.msix + VOC.cer + Install-VOC.cmd    Windows 安装包（自签名）
#   dist\voc-server-linux.tar.gz                     Linux 服务端安装包
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot
$tc = "+stable-x86_64-pc-windows-msvc"
cargo $tc build --release -p voc-client -p voc-server
foreach ($t in "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl") {
    rustup $tc target add $t | Out-Null
    cargo $tc build --release -p voc-server --target $t
}
$target = (cargo $tc metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item "$target\release\voc-server.exe", "$target\release\voc-client.exe" dist\

# Windows 客户端绿色版
$cli = "dist\VOC客户端"
if (Test-Path $cli) { Remove-Item -Recurse -Force $cli }
New-Item -ItemType Directory -Force $cli | Out-Null
Copy-Item "$target\release\voc-client.exe" "$cli\VOC.exe"
Copy-Item deploy\windows\使用说明.txt $cli
Compress-Archive -Path $cli -DestinationPath dist\VOC-portable.zip -Force

# Linux 服务端
$pkg = "dist\voc-server-linux"
if (Test-Path $pkg) { Remove-Item -Recurse -Force $pkg }
New-Item -ItemType Directory -Force "$pkg\bin\x86_64", "$pkg\bin\aarch64" | Out-Null
Copy-Item "$target\x86_64-unknown-linux-musl\release\voc-server" "$pkg\bin\x86_64\"
Copy-Item "$target\aarch64-unknown-linux-musl\release\voc-server" "$pkg\bin\aarch64\"
Copy-Item deploy\linux\install.sh, deploy\linux\uninstall.sh, deploy\linux\安装说明.txt $pkg
tar -czf dist\voc-server-linux.tar.gz -C dist voc-server-linux

# Windows 安装包（MSIX，自签名）
& (Join-Path $PSScriptRoot "deploy\windows\build-msix.ps1") -Exe "$target\release\voc-client.exe"

# 单文件安装程序（build.rs 会把刚打好的 dist\VOC-<版本>.msix 和 VOC.cer 嵌进去）
cargo $tc build --release -p voc-setup
Copy-Item "$target\release\VOC-Setup.exe" dist\
Write-Host "完成：dist\ 下有 Windows 客户端 / 服务端、MSIX 安装包、Linux 服务端安装包"

<#
.SYNOPSIS
  One-shot Linux test VM for Lenny on a Windows PC: VirtualBox + Ubuntu 24.04 (Xfce) with v4l2loopback, OBS,
  Discord, Chromium and Lenny Desktop built from a branch. Bridged networking, so the phone reaches the VM directly.

.DESCRIPTION
  No OS installer: it boots Ubuntu's ready-made cloud disk and cloud-init does the setup on first boot
  (20-40 min, then it reboots into the desktop, logged in as lenny/lenny).
  Run from an elevated PowerShell:   powershell -ExecutionPolicy Bypass -File tools\linux-test-vm.ps1
  Start over:                        & "$env:ProgramFiles\Oracle\VirtualBox\VBoxManage.exe" unregistervm lenny-linux --delete

  Inside the VM (Xfce menu or a terminal):
    lenny-desktop      the app. Stream card should say "Virtual camera: active" (/dev/video10, "Lenny")
    lenny-fake-phone   synthetic phone streaming to this VM, if no phone is at hand
    lenny-update       git pull + rebuild
  Then pick the "Lenny" camera in OBS (Video Capture Device (V4L2)), Discord (Settings > Voice & Video) or Chromium.
  Phone: same Wi-Fi as the PC, scan the QR code in Lenny Desktop.
#>
param(
    [string]$VmName = "lenny-linux",
    [int]$Cpus = 4,
    [int]$MemoryMB = 8192,
    [int]$DiskGB = 40,
    [string]$Branch = "rust-rewrite-cloud",
    [string]$RepoUrl = "https://github.com/spizganed/Lenny.git",
    [string]$BridgeAdapter = "",   # default: the adapter that has the default route
    [string]$Dir = "$env:USERPROFILE\VirtualBox VMs\$VmName"
)
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

# ---- 1. VirtualBox ----
$vbm = "$env:ProgramFiles\Oracle\VirtualBox\VBoxManage.exe"
if (-not (Test-Path $vbm)) {
    Write-Host "Installing VirtualBox (winget)..."
    winget install -e --id Microsoft.VCRedist.2015+.x64 --silent --accept-package-agreements --accept-source-agreements
    winget install -e --id Oracle.VirtualBox --silent --accept-package-agreements --accept-source-agreements
    if (-not (Test-Path $vbm)) { throw "VirtualBox didn't install; install it from virtualbox.org and run this again." }
}
function VBox { & $vbm @args; if ($LASTEXITCODE) { throw "VBoxManage $($args -join ' ') failed" } }
if ((& $vbm list vms) -match "`"$VmName`"") { throw "VM '$VmName' exists. Delete it first: `"$vbm`" unregistervm $VmName --delete" }

# ---- 2. Ubuntu cloud disk -> resized VDI ----
New-Item -ItemType Directory -Force $Dir | Out-Null
$vmdk = Join-Path $Dir "noble-cloudimg.vmdk"
$vdi = Join-Path $Dir "$VmName.vdi"
if (-not (Test-Path $vmdk)) {
    Write-Host "Downloading Ubuntu 24.04 cloud image (~600 MB)..."
    curl.exe -fL -o $vmdk "https://cloud-images.ubuntu.com/noble/current/noble-server-cloudimg-amd64.vmdk"
    if ($LASTEXITCODE) { throw "download failed" }
}
VBox clonemedium disk $vmdk $vdi --format VDI
VBox modifymedium disk $vdi --resize ($DiskGB * 1024)

# ---- 3. cloud-init seed ISO (NoCloud, volume label "cidata") ----
$userData = @"
#cloud-config
hostname: lenny-vm
users:
  - name: lenny
    gecos: Lenny tester
    groups: [sudo, video, audio]
    shell: /bin/bash
    sudo: ALL=(ALL) NOPASSWD:ALL
    lock_passwd: false
    plain_text_passwd: lenny
growpart: {mode: auto, devices: ['/']}
package_update: true
packages:
  - xserver-xorg
  - xfce4
  - xfce4-terminal
  - lightdm
  - lightdm-gtk-greeter
  - dbus-x11
  - virtualbox-guest-x11
  - obs-studio
  - ffmpeg
  - v4l-utils
  - build-essential
  - pkg-config
  - git
  - curl
  - libxkbcommon-x11-0
  - libgl1
  - mesa-utils
write_files:
  - path: /etc/modules-load.d/lenny.conf
    content: "v4l2loopback\n"
  - path: /etc/modprobe.d/lenny.conf
    content: "options v4l2loopback devices=1 video_nr=10 exclusive_caps=1 card_label=Lenny\n"
  - path: /etc/lightdm/lightdm.conf.d/50-lenny.conf
    content: "[Seat:*]\nautologin-user=lenny\nautologin-session=xfce\n"
  - path: /usr/local/bin/lenny-desktop
    permissions: '0755'
    content: "#!/bin/sh\nexec /home/lenny/Lenny/target/release/lenny-desktop \"`$@\"\n"
  - path: /usr/local/bin/lenny-fake-phone
    permissions: '0755'
    content: "#!/bin/sh\nexec /home/lenny/Lenny/target/release/examples/fake_phone 127.0.0.1 47474 \"`$@\"\n"
  - path: /usr/local/bin/lenny-update
    permissions: '0755'
    content: "#!/bin/sh\nset -e\ncd /home/lenny/Lenny && git pull && ~/.cargo/bin/cargo build --release -p lenny_desktop --bins --examples\n"
  - path: /usr/share/applications/lenny-desktop.desktop
    content: "[Desktop Entry]\nType=Application\nName=Lenny Desktop\nExec=lenny-desktop\nIcon=camera-web\nCategories=AudioVideo;\n"
  - path: /usr/share/applications/lenny-fake-phone.desktop
    content: "[Desktop Entry]\nType=Application\nName=Lenny fake phone\nExec=lenny-fake-phone\nTerminal=true\nIcon=phone\nCategories=AudioVideo;\n"
runcmd:
  - apt-get install -y linux-headers-`$(uname -r) v4l2loopback-dkms
  - curl -fL -o /tmp/discord.deb 'https://discord.com/api/download?platform=linux&format=deb' && apt-get install -y /tmp/discord.deb
  - snap install chromium
  - su - lenny -c 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal'
  - su - lenny -c 'git clone -b $Branch $RepoUrl Lenny && cd Lenny && ~/.cargo/bin/cargo build --release -p lenny_desktop --bins --examples'
power_state: {mode: reboot, message: "Lenny test VM ready, rebooting into the desktop", timeout: 30}
"@
$seedDir = Join-Path $Dir "seed"
New-Item -ItemType Directory -Force $seedDir | Out-Null
$utf8 = New-Object System.Text.UTF8Encoding($false)   # no BOM: cloud-init needs "#cloud-config" as the first bytes
[IO.File]::WriteAllText((Join-Path $seedDir "user-data"), $userData.Replace("`r`n", "`n"), $utf8)
[IO.File]::WriteAllText((Join-Path $seedDir "meta-data"), "instance-id: $VmName-1`nlocal-hostname: lenny-vm`n", $utf8)

Add-Type -TypeDefinition @"
public static class IsoWriter {
    public static void Save(object stream, string path, int blockSize, int blocks) {
        var s = (System.Runtime.InteropServices.ComTypes.IStream)stream;
        var buf = new byte[blockSize];
        using (var o = System.IO.File.Create(path)) {
            for (int i = 0; i < blocks; i++) { s.Read(buf, blockSize, System.IntPtr.Zero); o.Write(buf, 0, blockSize); }
        }
    }
}
"@
$fsi = New-Object -ComObject IMAPI2FS.MsftFileSystemImage
$fsi.FileSystemsToCreate = 3   # ISO9660 + Joliet (keeps the "user-data" name)
$fsi.VolumeName = "cidata"
$fsi.Root.AddTree($seedDir, $false)
$img = $fsi.CreateResultImage()
$iso = Join-Path $Dir "seed.iso"
[IsoWriter]::Save($img.ImageStream, $iso, $img.BlockSize, $img.TotalBlocks)

# ---- 4. VM: bridged to the PC's LAN adapter, so the phone reaches it at its own IP ----
if (-not $BridgeAdapter) {
    $route = Get-NetRoute -DestinationPrefix 0.0.0.0/0 | Sort-Object RouteMetric, InterfaceMetric | Select-Object -First 1
    $BridgeAdapter = (Get-NetAdapter -InterfaceIndex $route.ifIndex).InterfaceDescription
}
Write-Host "Bridging to: $BridgeAdapter"
VBox createvm --name $VmName --ostype Ubuntu_64 --register --basefolder (Split-Path $Dir)
VBox modifyvm $VmName --memory $MemoryMB --cpus $Cpus --vram 128 --graphicscontroller vmsvga `
    --nic1 bridged --bridgeadapter1 $BridgeAdapter --clipboard-mode bidirectional --rtcuseutc on `
    --uart1 0x3F8 4 --uartmode1 file (Join-Path $Dir "serial.log")
VBox storagectl $VmName --name SATA --add sata --controller IntelAhci --portcount 2
VBox storageattach $VmName --storagectl SATA --port 0 --device 0 --type hdd --medium $vdi
VBox storageattach $VmName --storagectl SATA --port 1 --device 0 --type dvddrive --medium $iso
VBox startvm $VmName --type gui

Write-Host ""
Write-Host "VM started. First boot sets everything up (20-40 min), then reboots into the Xfce desktop as lenny/lenny."
Write-Host "Progress: $(Join-Path $Dir 'serial.log')  (look for 'Lenny test VM ready')"
Write-Host "Then: menu > Lenny Desktop. The phone must be on the same network as this PC."

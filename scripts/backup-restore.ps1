# WarpDeck 数据卷备份 / 恢复（P12-009，DESIGN §28.3）。
# 备份 = warpdeck-data 数据卷整体快照：warpdeck.db（SQLite WAL 合并后落盘）
#       + master.key + instances/（WARP 注册态，恢复后免重新注册）。
# 原则：备份/恢复期间 docker compose stop —— MVP 允许停服务复制 DB（§28.3），
#       不做热备，杜绝 WAL/内存中未落盘数据的不一致。
#
# 用法:
#   .\scripts\backup-restore.ps1 backup  [-Project warpdeck] [-BackupDir .\backups]
#   .\scripts\backup-restore.ps1 restore -Archive <备份文件绝对路径> [-Project warpdeck]
#   .\scripts\backup-restore.ps1 list    [-BackupDir .\backups]
#
# 恢复前会先验证归档含 warpdeck.db 与 master.key，再停服清空数据卷并解包；
# 恢复后自动 docker compose start，数据面（WARP 实例/GOST）由 reconciler 恢复。
#
# 备份文件命名：warpdeck-{project}-{yyyyMMdd-HHmmss}.tar.gz

param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet("backup", "restore", "list")]
    [string]$Action,

    [string]$Archive,
    [string]$Project = "warpdeck",
    [string]$BackupDir = (Join-Path $PSScriptRoot "..\backups")
)

$ErrorActionPreference = "Stop"

$AbsDir = if ([System.IO.Path]::IsPathRooted($BackupDir)) {
    [System.IO.Path]::GetFullPath($BackupDir)
}
else {
    [System.IO.Path]::GetFullPath((Join-Path $PWD $BackupDir))
}
New-Item -ItemType Directory -Path $AbsDir -Force | Out-Null
$Volume = "${Project}_warpdeck-data"

function Invoke-Alpine([string]$Script) {
    docker run --rm -v "${Volume}:/data" -v "${AbsDir}:/backup" alpine:3.20 sh -c $Script
    if ($LASTEXITCODE -ne 0) { throw "alpine volume operation failed (exit=$LASTEXITCODE)" }
}

function Expect-Volume {
    $exists = docker volume inspect $Volume 2>$null
    if (-not $exists) { throw "volume $Volume does not exist (project '$Project' up?)" }
}

function ComposeStop {
    docker compose -p $Project stop
    if ($LASTEXITCODE -ne 0) { throw "docker compose stop failed" }
}

function ComposeStart {
    docker compose -p $Project start
    if ($LASTEXITCODE -ne 0) { throw "docker compose start failed" }
}

switch ($Action) {
    "backup" {
        Expect-Volume
        $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
        $name = "warpdeck-$Project-$stamp.tar.gz"
        Write-Host "== backup $Volume -> $AbsDir\$name =="
        ComposeStop
        Invoke-Alpine "tar czf /backup/$name -C /data ."
        ComposeStart
        Write-Host "OK: $AbsDir\$name"
    }
    "restore" {
        if (-not $Archive) { throw "restore requires -Archive <path>" }
        $abs = [System.IO.Path]::GetFullPath($Archive)
        if (-not (Test-Path $abs)) { throw "archive not found: $abs" }
        $name = Split-Path $abs -Leaf
        $files = docker run --rm -v "${AbsDir}:/backup" alpine:3.20 sh -c "tar tzf /backup/$name" 2>$null
        $base = @($files | ForEach-Object { $_.TrimStart("./") })
        foreach ($required in @("warpdeck.db", "master.key")) {
            if ($base -notcontains $required) { throw "archive missing '$required' - aborting restore" }
        }
        Write-Host "== restore $abs -> $Volume =="
        Expect-Volume
        ComposeStop
        Invoke-Alpine "rm -rf /data/* /data/.[!.]*"
        Invoke-Alpine "tar xzf /backup/$name -C /data"
        ComposeStart
        Write-Host "OK: restored $name ($volume), containers started"
    }
    "list" {
        Get-ChildItem $AbsDir -Filter "warpdeck-*.tar.gz" | Sort-Object Name | ForEach-Object {
            "{0}  {1,10:N0} bytes" -f $_.Name, $_.Length
        }
    }
}
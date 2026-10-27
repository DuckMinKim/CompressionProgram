<#
.SYNOPSIS
  Pouch와 반디집의 압축·해제 속도와 크기를 같은 폴더로 비교한다.

.EXAMPLE
  powershell -ExecutionPolicy Bypass -File tools\bench.ps1 -Target "D:\사진"
  powershell -ExecutionPolicy Bypass -File tools\bench.ps1 -Target "D:\GitHub\some-project" -Runs 5

.NOTES
  - 먼저 cargo build --release 로 pouch.exe를 만들어 두어야 한다.
  - 반디집(bz.exe)이 없으면 Pouch끼리만 비교한다.
  - 결과는 표로 보여 주고 bench-result.md 로도 저장한다 (README에 붙여 넣기 좋게).
  - 측정마다 출력 폴더를 비우고, 각 항목을 Runs번 재서 중간값을 쓴다.
#>
param(
    [Parameter(Mandatory = $true)][string]$Target,
    [int]$Runs = 3
)
$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$pouch = Join-Path $root "target\release\pouch.exe"
if (-not (Test-Path $pouch)) { throw "pouch.exe가 없습니다. 먼저 'cargo build --release'를 실행하세요: $pouch" }
$Target = (Resolve-Path $Target).Path
$name = Split-Path -Leaf $Target

$bz = @("$env:ProgramFiles\Bandizip\bz.exe", "${env:ProgramFiles(x86)}\Bandizip\bz.exe") |
    Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
if (-not $bz) { $cmd = Get-Command bz.exe -ErrorAction SilentlyContinue; if ($cmd) { $bz = $cmd.Source } }

# 임시 폴더: 공백·한글 없는 경로 (명령줄 따옴표 문제를 피하려고)
$tmp = Join-Path $root "bench-tmp"
if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force }
New-Item -ItemType Directory $tmp | Out-Null
$out = Join-Path $tmp "out"

function Median([double[]]$xs) { $s = $xs | Sort-Object; return $s[[int][math]::Floor($s.Count / 2)] }
function Reset-Out { if (Test-Path $out) { Remove-Item $out -Recurse -Force }; New-Item -ItemType Directory $out | Out-Null }
function Time-It([scriptblock]$block) { (Measure-Command { & $block | Out-Null }).TotalSeconds }
function Tree-Summary($dir) {
    $f = Get-ChildItem $dir -Recurse -File -Force -ErrorAction SilentlyContinue
    return "{0} files / {1} bytes" -f $f.Count, ($f | Measure-Object Length -Sum).Sum
}

$expected = Tree-Summary $Target
Write-Host "대상: $Target  ($expected)" -ForegroundColor Cyan
Write-Host "CPU 스레드: $([Environment]::ProcessorCount)   반복: $Runs 번 (중간값)"
if ($bz) { Write-Host "반디집: $bz" } else { Write-Host "반디집(bz.exe)을 찾지 못해 Pouch만 측정합니다." -ForegroundColor Yellow }
Write-Host ""

$cases = @()
$cases += @{ Name = "Pouch (.bzx)"; File = "a.bzx";
    C = { & $pouch c $Target -o (Join-Path $tmp "a.bzx") -f };
    X = { & $pouch x (Join-Path $tmp "a.bzx") -o $out --on-conflict overwrite } }
$cases += @{ Name = "Pouch (.zip)"; File = "b.zip";
    C = { & $pouch c $Target -o (Join-Path $tmp "b.zip") --format zip -f };
    X = { & $pouch x (Join-Path $tmp "b.zip") -o $out --on-conflict overwrite } }
if ($bz) {
    $cases += @{ Name = "반디집 (.zip)"; File = "c.zip";
        C = { & $bz c -y -l:5 -fmt:zip (Join-Path $tmp "c.zip") $Target };
        X = { & $bz x -y "-o:$out" (Join-Path $tmp "c.zip") } }
}

$rows = @()
foreach ($case in $cases) {
    Write-Host "측정 중: $($case.Name)" -NoNewline
    $file = Join-Path $tmp $case.File
    $cs = @(); $xs = @()
    for ($i = 0; $i -lt $Runs; $i++) {
        if (Test-Path $file) { Remove-Item $file -Force }
        $cs += Time-It $case.C
        Reset-Out
        $xs += Time-It $case.X
        Write-Host "." -NoNewline
    }
    $got = Tree-Summary (Join-Path $out $name)
    $ok = if ($got -eq $expected) { "OK" } else { "다름! ($got)" }
    $size = (Get-Item $file).Length
    Write-Host " 완료"
    $rows += [pscustomobject]@{
        "방식"      = $case.Name
        "압축(초)"  = "{0:N2}" -f (Median $cs)
        "해제(초)"  = "{0:N2}" -f (Median $xs)
        "크기(MB)"  = "{0:N1}" -f ($size / 1MB)
        "풀린 결과" = $ok
    }
}

# 호환성: Pouch가 만든 zip을 반디집으로 풀어 본다
if ($bz) {
    Reset-Out
    & $bz x -y "-o:$out" (Join-Path $tmp "b.zip") | Out-Null
    $got = Tree-Summary (Join-Path $out $name)
    $compat = if ($got -eq $expected) { "OK — 반디집이 Pouch가 만든 zip을 정상적으로 풉니다" } else { "문제 있음 ($got)" }
}

Write-Host ""
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
if ($bz) { Write-Host "호환성 검사: $compat" }

# Markdown으로 저장
$md = @("## 벤치마크 — $name", "", "- 대상: $expected", "- CPU 스레드: $([Environment]::ProcessorCount), 각 $Runs 번 측정한 중간값", "",
        "| 방식 | 압축(초) | 해제(초) | 크기(MB) | 풀린 결과 |", "|---|---:|---:|---:|---|")
foreach ($r in $rows) { $md += "| $($r.'방식') | $($r.'압축(초)') | $($r.'해제(초)') | $($r.'크기(MB)') | $($r.'풀린 결과') |" }
if ($bz) { $md += ""; $md += "호환성: $compat" }
$mdPath = Join-Path $root "bench-result.md"
[IO.File]::WriteAllLines($mdPath, $md, (New-Object Text.UTF8Encoding($true)))
Write-Host "결과 저장: $mdPath" -ForegroundColor Cyan

Remove-Item $tmp -Recurse -Force

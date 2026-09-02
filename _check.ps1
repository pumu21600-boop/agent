[Console]::OutputEncoding = [Text.Encoding]::UTF8
$d = Join-Path $env:USERPROFILE 'Desktop'
Get-ChildItem -LiteralPath $d -Filter *.txt | ForEach-Object {
  Write-Output ('=== ' + $_.FullName + ' | ' + $_.Length + ' bytes | ' + $_.LastWriteTime)
  $bytes = [IO.File]::ReadAllBytes($_.FullName)
  Write-Output ('  head-bytes: ' + (($bytes | Select-Object -First 6 | ForEach-Object { $_.ToString('X2') }) -join ' '))
  $i = 0
  foreach ($line in (Get-Content -LiteralPath $_.FullName -Encoding UTF8)) {
    $i++
    Write-Output ('  ' + $i + ': ' + $line)
  }
}
Write-Output '--- workspace ---'
Get-ChildItem -Path . -Recurse -Filter *.txt -ErrorAction SilentlyContinue | ForEach-Object { Write-Output ('  ' + $_.FullName + ' | ' + $_.LastWriteTime) }

[Console]::OutputEncoding = [Text.Encoding]::UTF8
$d = Join-Path $env:USERPROFILE 'Desktop'

Write-Output '=== Desktop *.txt (non-ascii names) ==='
Get-ChildItem -LiteralPath $d -Filter *.txt | Where-Object { $_.Name -notmatch '^[\x00-\x7F]+$' } | ForEach-Object {
  Write-Output ('FILE : ' + $_.FullName)
  Write-Output ('SIZE : ' + $_.Length + '  MTIME: ' + $_.LastWriteTime)
  $b = [IO.File]::ReadAllBytes($_.FullName)
  Write-Output ('HEAD : ' + (($b | Select-Object -First 4 | ForEach-Object { $_.ToString('X2') }) -join ' '))
  $i = 0
  foreach ($l in [IO.File]::ReadAllLines($_.FullName, [Text.Encoding]::UTF8)) { $i++; Write-Output ('  ' + $i + ': ' + $l) }
}

Write-Output ''
Write-Output '=== Workspace (agent) tree ==='
Get-ChildItem -LiteralPath $PWD.Path -Recurse -File -Force -ErrorAction SilentlyContinue | ForEach-Object {
  Write-Output ('  ' + $_.FullName.Substring($PWD.Path.Length) + ' | ' + $_.Length + ' | ' + $_.LastWriteTime)
}

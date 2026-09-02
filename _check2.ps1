[Console]::OutputEncoding = [Text.Encoding]::UTF8
$p = Join-Path $env:USERPROFILE 'Desktop\计划.txt'
Write-Output ('PATH : ' + $p)
Write-Output ('EXISTS: ' + (Test-Path -LiteralPath $p))
Write-Output ('SIZE : ' + (Get-Item -LiteralPath $p).Length)
Write-Output ('MTIME: ' + (Get-Item -LiteralPath $p).LastWriteTime)
$b = [IO.File]::ReadAllBytes($p)
Write-Output ('HEAD : ' + (($b | Select-Object -First 6 | ForEach-Object { $_.ToString('X2') }) -join ' '))
Write-Output '--- CONTENT ---'
$i = 0
foreach ($l in [IO.File]::ReadAllLines($p, [Text.Encoding]::UTF8)) { $i++; Write-Output ($i.ToString() + ': [' + $l + ']') }
Write-Output '--- WORKSPACE COPY ---'
Get-ChildItem -Path 'C:\Users\大脚\Desktop\agent' -Recurse -File -ErrorAction SilentlyContinue | ForEach-Object { Write-Output ('  ' + $_.FullName + ' | ' + $_.LastWriteTime) }

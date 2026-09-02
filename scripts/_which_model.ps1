$m = Select-String -Path logs\agent.jsonl -Pattern '"model":"([^"]+)"' -AllMatches
$m.Matches | ForEach-Object { $_.Groups[1].Value } | Group-Object | Sort-Object Count -Descending | Select-Object Name,Count | Format-Table -AutoSize

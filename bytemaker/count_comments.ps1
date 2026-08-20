$files = Get-ChildItem -Recurse -Filter *.rs -Path src,tests | Where-Object { $_.FullName -notlike '*target*' }
$totalComments = 0
foreach ($file in $files) {
    $lines = Get-Content $file.FullName
    $commentLines = 0
    $inBlock = $false
    foreach ($line in $lines) {
        $trimmed = $line.Trim()
        if ($inBlock) {
            $commentLines++
            if ($trimmed -match '\*/') {
                $inBlock = $false
            }
        } elseif ($trimmed -match '^//') {
            $commentLines++
        } elseif ($trimmed -match '/\*') {
            $commentLines++
            if ($trimmed -notmatch '\*/') {
                $inBlock = $true
            }
        }
    }
    $totalComments += $commentLines
    Write-Output ("{0}: {1} comment lines" -f $file.FullName, $commentLines)
}
Write-Output ""
Write-Output ("Total comment lines across all files: {0}" -f $totalComments)

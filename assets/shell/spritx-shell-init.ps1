# SpritX Pastel: a two-line, true-color Powerline prompt. Scoped to SpritX's
# child shell; never edits the user's PowerShell profile.
if (-not $global:SpritXPromptEnabled) {
    $global:SpritXPromptEnabled = $true
    $global:SpritXOriginalPrompt = $function:prompt
    $global:SpritXNodeDirectory = $null
    $global:SpritXNodeVersion = $null
    function global:Get-SpritXCells([string]$Text) {
        try { return $Host.UI.RawUI.LengthInBufferCells($Text) } catch { return $Text.Length }
    }
    function global:Compress-SpritXLabel([string]$Text, [int]$Limit) {
        if ($Limit -le 0) { return '' }
        if ((Get-SpritXCells $Text) -le $Limit) { return $Text }
        $result = ''
        $elements = [Globalization.StringInfo]::GetTextElementEnumerator($Text)
        while ($elements.MoveNext()) {
            $next = $result + $elements.GetTextElement()
            if ((Get-SpritXCells ($next + '…')) -gt $Limit) { break }
            $result = $next
        }
        return $result + '…'
    }
    function global:Format-SpritXPromptBar([string]$UserName, [string]$Folder, [string]$NodeVersion, [string]$Time, [int]$Columns) {
        $e = [string][char]27
        $budget = [Math]::Max(0, $Columns - 1)
        if ($budget -lt 6) { return '' }
        $segments = [Collections.Generic.List[object]]::new()
        $segments.Add(@{ Text = ' ' + (Compress-SpritXLabel $UserName 16); Bg = '69;71;90'; Fg = '205;214;244'; Kind = 'user' })
        $segments.Add(@{ Text = (Compress-SpritXLabel $Folder 28); Bg = '250;179;135'; Fg = '30;30;46'; Kind = 'folder' })
        if ($NodeVersion) { $segments.Add(@{ Text = ' ' + $NodeVersion; Bg = '148;226;213'; Fg = '30;30;46'; Kind = 'node' }) }
        $segments.Add(@{ Text = ' ' + $Time; Bg = '245;194;231'; Fg = '30;30;46'; Kind = 'time' })
        # A segment includes its separator and one padding cell on each side.
        foreach ($optional in @('node', 'user', 'time')) {
            $width = 1
            foreach ($segment in $segments) { $width += (Get-SpritXCells $segment.Text) + 3 }
            if ($width -le $budget) { break }
            $item = $segments | Where-Object { $_.Kind -eq $optional } | Select-Object -First 1
            if ($item) { $null = $segments.Remove($item) }
        }
        $overhead = 1
        foreach ($segment in $segments) { $overhead += 3; if ($segment.Kind -ne 'folder') { $overhead += Get-SpritXCells $segment.Text } }
        $folderSegment = $segments | Where-Object { $_.Kind -eq 'folder' } | Select-Object -First 1
        $folderSegment.Text = Compress-SpritXLabel $folderSegment.Text ([Math]::Max(0, $budget - $overhead))
        $bar = $e + '[38;2;' + $segments[0].Bg + 'm'
        for ($i = 0; $i -lt $segments.Count; $i++) {
            $segment = $segments[$i]
            $bar += $e + '[48;2;' + $segment.Bg + ';38;2;' + $segment.Fg + 'm ' + $segment.Text + ' '
            if ($i + 1 -lt $segments.Count) {
                $bar += $e + '[38;2;' + $segment.Bg + ';48;2;' + $segments[$i + 1].Bg + 'm'
            } else { $bar += $e + '[38;2;' + $segment.Bg + ';49m' }
        }
        return $bar + $e + '[0m'
    }
    function global:prompt {
        $succeeded = $?
        $e = [string][char]27
        $location = Get-Location
        if ($location.Provider.Name -eq 'FileSystem') {
            $uri = [System.Uri]::new($location.ProviderPath)
            [Console]::Write($e + ']7;' + $uri.AbsoluteUri + [char]7)
        }
        # Cache Node per directory so editing a command never spawns a process.
        if ($global:SpritXNodeDirectory -ne $location.Path) {
            $global:SpritXNodeDirectory = $location.Path
            $global:SpritXNodeVersion = $null
            $node = Get-Command node -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($node) {
                $version = & $node.Source --version 2>$null | Select-Object -First 1
                if ($version -match '^v\d+\.\d+\.\d+') { $global:SpritXNodeVersion = $Matches[0] }
            }
        }
        $userName = [Environment]::UserName -replace '[\x00-\x1f\x7f]', ''
        $folder = if ($location.Path -eq $HOME) { '~' } else { Split-Path $location.Path -Leaf }
        if (-not $folder) { $folder = $location.Path }
        $folder = $folder -replace '[\x00-\x1f\x7f]', ''
        if ($folder.Length -gt 28) { $folder = '…' + $folder.Substring($folder.Length - 27) }
        $time = Get-Date -Format 'HH:mm'
        $columns = 80
        try { $columns = [Math]::Max(1, $Host.UI.RawUI.WindowSize.Width) } catch {}
        $bar = Format-SpritXPromptBar $userName $folder $global:SpritXNodeVersion $time $columns
        $arrow = if ($succeeded) { '166;227;161' } else { '243;139;168' }
        return $(if ($bar) { $bar + "`n" } else { '' }) + $e + '[38;2;' + $arrow + 'm❯' + $e + '[0m '
    }
}

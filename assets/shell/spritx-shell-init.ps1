# Embedded into SpritX. Keep the user's prompt and publish the filesystem cwd
# through OSC 7 so the sidebar follows Set-Location, including paths with spaces.
if (-not $global:SpritXOriginalPrompt) {
    $global:SpritXOriginalPrompt = $function:prompt
    function global:prompt {
        $location = Get-Location
        if ($location.Provider.Name -eq 'FileSystem') {
            $uri = [System.Uri]::new($location.ProviderPath)
            [Console]::Write(([string][char]27) + ']7;' + $uri.AbsoluteUri + [char]7)
        }
        if ($global:SpritXOriginalPrompt) { & $global:SpritXOriginalPrompt } else { "PS $($location.Path)> " }
    }
}

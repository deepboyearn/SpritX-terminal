// Bundled PowerShell prompt and command tracking, passed directly at launch.
//
// Always launch it as `-Command ". @'…'@"` and never bare, and never as
// `-EncodedCommand`. A bare `-Command <script>` runs *directly* in the
// top-level scope, so console Ctrl+C interrupts it exactly like a command the
// user typed; `Start-Sleep` then aborts the load midway, leaving
// `SpritXPromptEnabled` set with the prompt function never installed — no
// prompt, no command tracking, and every later Ctrl+C swallowed by the broken
// host state. Dot-sourcing from a here-string keeps the whole load in the
// wrapping scope: Ctrl+C during it is ignored, exactly like a profile.
pub const POWERSHELL_INIT: &str = r###"
# Windows PowerShell aliases curl to Invoke-WebRequest. Resolve the familiar
# curl command to the installed native executable inside SpritX sessions.
$spritxCurl = Get-Command curl.exe -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
$spritxCurlAlias = Get-Alias curl -ErrorAction SilentlyContinue
if ($spritxCurl -and $spritxCurlAlias -and $spritxCurlAlias.Definition -eq 'Invoke-WebRequest') {
    Set-Alias -Name curl -Value $spritxCurl.Source -Scope Global -Option $spritxCurlAlias.Options
}
# SpritX Pastel: a two-line, true-color Powerline prompt. Scoped to SpritX's
# child shell; never edits the user's PowerShell profile.
if (-not $global:SpritXPromptEnabled) {
    $global:SpritXPromptEnabled = $true
    $global:SpritXOriginalPrompt = $function:prompt
    $global:SpritXNodeDirectory = $null
    $global:SpritXNodeVersion = $null
    $global:SpritXCommandRunning = $false
    if ($env:SPRITX_COMMAND_TRACKING -eq '1') {
        Import-Module PSReadLine -ErrorAction SilentlyContinue
        $readLine = Get-Command PSConsoleHostReadLine -CommandType Function -ErrorAction SilentlyContinue
        if ($readLine) {
            $global:SpritXOriginalReadLine = $readLine.ScriptBlock
            function global:PSConsoleHostReadLine {
                $line = & $global:SpritXOriginalReadLine
                if (-not [string]::IsNullOrWhiteSpace($line)) {
                    $safeLine = $line -replace '[\x00-\x1f\x7f]', ' '
                    [Console]::Write([string][char]27 + ']133;E;' + $safeLine + [char]7)
                    [Console]::Write([string][char]27 + ']133;C' + [char]7)
                    $global:SpritXCommandRunning = $true
                }
                return $line
            }
        }
    }
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
                $bar += $e + '[38;2;' + $segment.Bg + ';48;2;' + $segments[$i + 1].Bg + 'm'
            } else { $bar += $e + '[38;2;' + $segment.Bg + ';49m' }
        }
        return $bar + $e + '[0m'
    }
    function global:prompt {
        $succeeded = $?
        if ($global:SpritXCommandRunning) {
            $global:SpritXCommandRunning = $false
            $status = if ($succeeded) { 0 } else { 1 }
            [Console]::Write([string][char]27 + ']133;D;' + $status + [char]7)
        }
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
"###;

/// The PowerShell command that loads [`POWERSHELL_INIT`] wrapped, so a console
/// Ctrl+C during startup cannot abort the load halfway.
///
/// The payload is base64 UTF-16 and therefore passed via `-EncodedCommand`,
/// not `-Command`. That is required, not stylistic: PowerShell 5.1 decodes a
/// `-Command` argument as MBCS whenever its fourth character is below `0x80`,
/// which mangles the private-use Powerline glyphs in the prompt (`\u{e0b0}`
/// became `Œ,`, producing "Missing ')' in method call"). `-EncodedCommand` is
/// encoding-safe on both 5.1 and 7.
///
/// The inner `& ([ScriptBlock]::Create(@'…'@))` is what keeps the load
/// *wrapped*: a bare script runs directly in the top-level scope, where the
/// console Ctrl+C inherits into it and `Start-Sleep` aborts the load, leaving
/// `SpritXPromptEnabled` set with no prompt function installed. The here-string
/// is single-quoted (a double-quoted one would interpolate `$global:…` away to
/// nothing) and its opening marker sits on one line, which is what 5.1
/// requires.
pub const POWERSHELL_INIT_WRAPPER: &str = "& ([ScriptBlock]::Create(@'\n";

/// Closes the here-string opened by [`POWERSHELL_INIT_WRAPPER`].
pub const POWERSHELL_INIT_WRAPPER_END: &str = "\n'@))";

/// `-Command` arguments for an interactive shell with the bundled integration.
pub fn interactive_args() -> Vec<String> {
    let mut args = vec!["-NoLogo".into(), "-NoExit".into(), "-Command".into()];
    args.push(format!(
        "{}{}{}",
        POWERSHELL_INIT_WRAPPER,
        POWERSHELL_INIT.trim_matches(['\r', '\n']),
        POWERSHELL_INIT_WRAPPER_END
    ));
    args
}

/// Arguments that load the bundled integration *and* run `launch` afterwards.
///
/// Used for script launchers (`.ps1`/`.bat`/`.cmd`), which cannot be spawned
/// directly: the launch is appended to the same base64 command, so the
/// integration still installs first and the shell stays interactive.
pub fn args_with_launch(launch: &str) -> Vec<String> {
    let mut args = encoded_args();
    if let Some(command) = args.last_mut() {
        if let Some(decoded) = decode(command) {
            *command = encode(&format!("{decoded}\n{launch}"));
        }
    }
    args
}

/// UTF-16LE base64, the encoding `-EncodedCommand` expects.
fn encode(command: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let bytes: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
    STANDARD.encode(bytes)
}

/// Decode a UTF-16LE base64 `-EncodedCommand` payload, if it is one.
pub fn decode_encoded_payload(encoded: &str) -> Option<String> {
    decode(encoded)
}

fn decode(encoded: &str) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let bytes = STANDARD.decode(encoded).ok()?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// `-EncodedCommand` arguments for the bundled integration, encoding-safe on
/// PowerShell 5.1 and 7. This is the shape production uses; see
/// [`POWERSHELL_INIT_WRAPPER`] for why the load must be both encoded and
/// wrapped.
pub fn encoded_args() -> Vec<String> {
    vec![
        "-NoLogo".into(),
        "-NoExit".into(),
        "-EncodedCommand".into(),
        encode(&integration_command()),
    ]
}

/// The wrapped load command, without any launcher appended.
pub fn integration_command() -> String {
    format!(
        "{}{}{}",
        POWERSHELL_INIT_WRAPPER,
        POWERSHELL_INIT.trim_matches(['\r', '\n']),
        POWERSHELL_INIT_WRAPPER_END
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A here-string is terminated by `'@` at the start of a line, so one
    /// inside the payload would silently truncate the script — the integration
    /// would load as a `#requires`-less fragment and the prompt would vanish.
    #[test]
    fn powershell_init_is_launchable_as_a_here_string() {
        for (index, line) in POWERSHELL_INIT.lines().enumerate() {
            assert!(
                !line.starts_with("'@"),
                "line {} of POWERSHELL_INIT terminates the here-string early: {line:?}",
                index + 1
            );
        }
        assert!(!POWERSHELL_INIT.contains('@') || !POWERSHELL_INIT.contains("'@\n"));
    }

    /// The launcher must use `-EncodedCommand`, not `-Command`: PowerShell 5.1
    /// decodes a `-Command` argument as MBCS whenever its fourth character is
    /// below `0x80`, which mangles the private-use Powerline glyphs and makes
    /// the prompt fail to parse.
    #[test]
    fn encoded_args_use_encoded_command_and_round_trip() {
        let args = encoded_args();
        assert!(args.contains(&"-NoExit".to_string()));
        assert!(args.contains(&"-EncodedCommand".to_string()));
        assert!(!args.contains(&"-Command".to_string()));
        let payload = args.last().expect("encoded payload");
        let decoded = decode(payload).expect("payload decodes as UTF-16");
        assert_eq!(decoded, integration_command());
        assert!(decoded.contains("# SpritX Pastel"), "body embedded verbatim");
    }

    /// The load must be *wrapped*, or a console Ctrl+C during startup aborts it
    /// and leaves `SpritXPromptEnabled` set with no prompt installed.
    #[test]
    fn the_load_is_wrapped_in_a_single_quoted_script_block() {
        let command = integration_command();
        assert!(command.starts_with("& ([ScriptBlock]::Create(@'\n"), "{command}");
        assert!(command.ends_with("\n'@))"), "{command}");
        // A double-quoted here-string would interpolate `$global:…` away.
        assert!(!command.starts_with("& ([ScriptBlock]::Create(@\""));
        assert!(command.contains("$global:SpritXPromptEnabled"));
        assert!(command.contains("function global:prompt"));
    }

    /// `-EncodedCommand` must not be combined with a second switch that would
    /// re-open the wrapped load as a bare script.
    #[test]
    fn interactive_args_keeps_the_switches_that_imply_an_interactive_shell() {
        let args = interactive_args();
        assert!(args.contains(&"-NoLogo".to_string()));
        assert!(args.contains(&"-NoExit".to_string()));
        let command = args.last().expect("command argument");
        assert!(command.starts_with("& ([ScriptBlock]::Create(@'\n"), "{command}");
        assert!(command.trim_end().ends_with("'@))"), "{command}");
        assert!(command.contains("# SpritX Pastel"));
    }

    /// A script launcher is appended *inside* the same encoded command, so the
    /// integration still installs and the shell stays interactive afterwards.
    #[test]
    fn args_with_launch_appends_the_launcher_to_the_same_command() {
        let args = args_with_launch("& 'C:\\tools\\agent.ps1'; exit $LASTEXITCODE");
        let decoded = decode(args.last().expect("payload")).expect("decodes");
        assert!(decoded.contains("# SpritX Pastel"), "integration still present");
        assert!(decoded.trim_end().ends_with("exit $LASTEXITCODE"));
        // The integration must come first, or the launcher would replace it.
        let integration_at = decoded.find("function global:prompt").expect("prompt defined");
        let launch_at = decoded.find("agent.ps1").expect("launcher present");
        assert!(integration_at < launch_at, "integration must load first");
    }
}

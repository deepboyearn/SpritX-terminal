# SpritX

A GPU-accelerated terminal emulator with built-in remote control: open the
same panes from another SpritX, or from a phone browser over an end-to-end
encrypted link.

![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)
[![tests](https://github.com/legacy7838-create/SpritX-terminal/actions/workflows/test.yml/badge.svg)](https://github.com/legacy7838-create/SpritX-terminal/actions/workflows/test.yml)

---

## Highlights

**Terminal**

- Split panes (up to four per tab), tabs, and layout presets.
- `vt100`-based screen model with scrollback, mouse reporting passed through
  to the child, text selection, copy/paste, and alternate-screen support.
- OSC 133 command blocks: prompt, command, output, exit code and duration are
  parsed from shell-integration markers; OSC 7 keeps track of the shell's
  working directory.
- Ten built-in themes (SpritX Dark, Catppuccin Mocha/Latte, Dracula, Nord,
  Tokyo Night, Gruvbox Dark, Solarized Dark, One Dark, Rosé Pine) plus a
  fully customisable 21-colour theme block.
- Configurable font family and size, cursor style (block/bar/underline),
  cursor blink and scrollback length.
- Desktop notification when a long-running command finishes.

**Workspace**

- File tree beside the terminal: directory listing command configurable, git
  branch and dirty state in the header, per-row git status badges
  (modified/added/deleted/untracked, respecting nested-repo boundaries),
  live refresh by bounded background polling (~1 s for a create/delete in
  the tree; no filesystem watcher), and context-menu actions
  (list / open / view / edit) driven by command templates.
- Command history in `~/.config/spritx/history.jsonl`, searchable from the
  command palette with filters such as `failed`, `ok`, `here`, `cwd:`,
  `project:` and `branch:`.
- Command palette for actions, tabs, panes and history.
- Settings UI that writes straight to `config.toml`.

**Remote control**

- This SpritX can act as a server: pair a phone or a second machine, then drive
  the same panes from there. Input, resizes and tab switches are applied by the
  desktop's own event loop, so both sides see one terminal.
- Every frame after the handshake is sealed with NaCl `secretbox`
  (XSalsa20-Poly1305). The one-time pairing secret travels in the URL *fragment*,
  which browsers never send to the server or the tunnel, and a device that fails
  the handshake is refused rather than downgraded to plaintext.
- Optional `cloudflared` quick tunnel for access over HTTPS from anywhere;
  without it the server binds the LAN address.
- Paired devices are listed in the UI and can be revoked at any time.
- It can also act as a client: paste a pairing link into the command palette to
  drive another SpritX from a pane here.

**Browser client**

- Served by the same binary at `/`, no build step: terminal picker, extra key
  row (Esc, Tab, Ctrl, arrows, `^C`, `^L`, `^D`), path-completion chips, font
  size controls and a WebGL renderer when the device supports it.
- Touch is translated to mouse and wheel events for full-screen TUIs, and the
  software keyboard follows the caret: it comes up when you tap the input line
  of a program that owns the mouse (or a shell prompt), not on every tap.
- Terminal size follows the device's own box, so panes fit the phone screen
  exactly; the desktop pane takes its size back when the last client leaves.
- The page loads xterm.js, its fit/webgl addons and tweetnacl from jsDelivr with
  SRI hashes, so the browser needs network access.

## Requirements

- Linux with Wayland or X11, or Windows 10 version 1809+ / Windows 11 (x64).
  Windows uses native ConPTY and Windows PowerShell by default; no WSL is required.
- A Rust toolchain (edition 2021) and working graphics drivers for GPUI.
  On Windows use the MSVC Rust toolchain with Visual Studio Build Tools
  (Desktop development with C++ and a Windows SDK). Linux needs the X11/Wayland,
  xkbcommon, fontconfig and freetype development libraries listed in CI.
- Optional: `cloudflared` for the Internet tunnel, `git` for the branch
  indicator, a Nerd Font for the icon set (the default font stack leads with
  `JetBrainsMono Nerd Font`).

## Build and run

```sh
cargo run --release
```

Notes on the build:

- The first build takes a few minutes by design: the dev profile compiles the
  dependency tree with `opt-level = 2` and turns incremental compilation off
  (see the comments in `Cargo.toml`). Later rebuilds of this crate are quick.
- `vendor/xattr` is a local patch of `xattr 0.2.3`, which references
  `libc::ENOATTR` — an alias recent `libc` no longer exposes on Linux. It is
  required by GPUI's file-watcher stack.
- `SPRITX_SMOKE=1` quits right after startup, which is handy in scripts.

## Keyboard shortcuts

`Ctrl/Cmd` means `Ctrl` on Linux and Windows, `Cmd` elsewhere.

| Shortcut | Action |
| --- | --- |
| `Ctrl/Cmd+T` | new tab |
| `Ctrl/Cmd+W` | close pane |
| `Ctrl/Cmd+Tab`, `Ctrl/Cmd+Shift+Tab` | next / previous tab |
| `Ctrl/Cmd+Shift+]`, `Ctrl/Cmd+Shift+[` | next / previous tab |
| `Ctrl/Cmd+1` … `9` | select tab |
| `Ctrl/Cmd+Shift+D`, `Ctrl/Cmd+Shift+E` | split right / split down |
| `Alt+←/→/↑/↓` | move focus between panes |
| `Ctrl/Cmd+Shift+P` | command palette |
| `Ctrl/Cmd+Shift+H` | command history |
| `Ctrl/Cmd+,` | settings |
| `Ctrl/Cmd+Shift+C`, `Ctrl/Cmd+Insert` | copy selection |
| `Ctrl/Cmd+Shift+V`, `Shift+Insert` | paste |
| `Ctrl/Cmd+Shift+A` | select all |
| `Ctrl/Cmd` `+` / `-` / `0` | font size up / down / reset |
| `Shift+PgUp`, `Shift+PgDn` | scroll ten lines |

## Remote control

1. Click the phone icon in the toolbar and choose **Start sharing**. SpritX
   binds `remote.port` (4530 by default) — loopback only when it also opens a
   tunnel, otherwise the LAN address — and shows a one-time pairing link and QR
   code.
2. Open that link on the other device. The page pairs once, stores its own
   device key locally and reconnects with it afterwards; the same device never
   needs a new code.
3. Pick a terminal in the browser's picker and start typing.

The pairing code is single-use and expires; devices can be revoked from the
remote panel, which also cuts their live connections.

## Configuration

`$XDG_CONFIG_HOME/spritx/config.toml`, falling back to
`~/.config/spritx/config.toml` on Linux; `%APPDATA%\spritx\config.toml` on Windows.
History and remote device credentials live beside this file. Keys are camelCase; every field has a default,
so a partial file still loads. The file is written atomically with `0600`
permissions, and a corrupt file is backed up next to it as
`config.toml.broken`.

| Section | Keys |
| --- | --- |
| `[appearance]` | `fontFamily`, `fontSize`, `cursorStyle`, `cursorBlink`, `scrollback`, `[appearance.theme]` (21 colours) |
| `[behavior]` | `focusFollowsMouse`, `showGitBranch`, `persistBlocks` |
| `[shell]` | `program`, `args` |
| `[notifications]` | `enabled`, `minDurationSec`, `sound` |
| `[fileTree]` | `dirList`, `dirOpen`, `fileView`, `fileEdit`, `fileOpen` — command templates, `{path}` is quoted for the running shell |
| `[remote]` | `port`, `autoTunnel` |
| `[history]` | `enabled`, `ignore` (glob patterns; matching commands are never recorded) |
| `[keybindings]` | action → keystroke map |

### Windows usage

Build and start from PowerShell with `cargo run --release`. Release builds are
GUI executables, so opening `spritx-terminal.exe` does not open a second console.
The portable Windows release ZIP contains the executable, README and license.

Settings → Shell accepts a full executable path, including spaces. For PowerShell 7,
select `C:\Program Files\PowerShell\7\pwsh.exe`; `cmd.exe` is also supported.
Arguments are a JSON array (for example `["-NoLogo", "-NoProfile"]`), preserving
spaces, quotes and backslashes; `[]` selects the shell's integration defaults.
Shell changes take effect in new tabs and panes. Custom arguments replace the
automatic integration arguments. Windows file actions default to PowerShell
commands and can be changed under Settings; standard defaults adapt to cmd.exe.

The embedded PowerShell prompt integration reports cwd (OSC 7), including Unicode
paths, so the file tree follows `Set-Location`. It does not currently emit OSC 133
command markers: rich command history and duration tracking need a shell that
provides those markers. cmd.exe has no automatic cwd/history integration.
Remote clients can connect between Linux and Windows using the same protocol.

### Shell integration

Command blocks, exit codes and the working-directory tracking come from a small
shell-integration script that SpritX sources through a generated rc file
(`~/.config/spritx/gpui-bashrc` for bash, `gpui-zdotdir` for zsh). The port
looks for it next to the crate:

```
../scripts/spritx-shell-init.bash
../scripts/spritx-shell-init.zsh
```

Without it, SpritX starts your shell with `-l` and the terminal itself works
exactly the same — only the block, history and cwd extras stay off.

## Packages

Releases are built by [`.github/workflows/release.yml`](.github/workflows/release.yml),
which is started by hand from the Actions tab and refuses to run unless the
tests workflow passed for the commit it is run on. It publishes six packages:

| Package | For | Built with |
| --- | --- | --- |
| `SpritX-<version>-windows-x86_64.zip` | Windows 10/11 x64 | MSVC release build, portable ZIP |
| `SpritX-<version>-x86_64.flatpak` | cross-distro, sandboxed | the manifest in `packaging/flatpak` |
| `SpritX-<version>-x86_64.AppImage` | portable, nothing to install | `appimagetool` over an AppDir |
| `spritx-terminal_<version>_amd64.deb` | Ubuntu, Debian, Mint | `cargo deb` |
| `spritx-terminal-<version>-1.x86_64.rpm` | Fedora, RHEL | `cargo generate-rpm` |
| `spritx-terminal-<version>-1-x86_64.pkg.tar.zst` | Arch, Omarchy, EndeavourOS | `makepkg` over `packaging/arch/PKGBUILD` |

GitHub does not keep the executable bit on release assets, so the AppImage needs
one command before its first run:

```sh
chmod +x SpritX-*-x86_64.AppImage && ./SpritX-*-x86_64.AppImage
```

The version comes from `Cargo.toml`, so a release is: bump it, push, let the
tests go green, then run the release workflow.

## Development

```sh
cargo test                  # integration tests: terminal, config, history,
                            # remote protocol, OSC parsing
SPRITX_UI_TESTS=1 cargo test  # also runs the smoke tests, which open a real
                              # window for ~3 seconds each
```

Every push and pull request runs the same suite on GitHub Actions
([`.github/workflows/test.yml`](.github/workflows/test.yml), Ubuntu with the
X11/Wayland/font libraries installed, and Windows). Unix PTY tests run on Linux;
Windows runs native ConPTY tests for input/output, exit status, resize, process
termination and PowerShell cwd reporting.

Runtime hooks used for automation and profiling:

| Variable | Effect |
| --- | --- |
| `SPRITX_OPEN=settings\|palette\|remote\|remote-menu` | open a modal (or the remote panel) at launch |
| `SPRITX_SMOKE=1` | quit after three seconds |
| `SPRITX_PROFILE=1` | print the latency profiler — debug builds do this by default, release builds need the variable. `SPRITX_ROWS=1` adds a per-row breakdown, `SPRITX_FRAMES=1` logs every frame, `SPRITX_QUIET=1` silences all of it |
| `SPRITX_DEBUG_KEYS=1` | log every keystroke |

## Project layout

```
src/main.rs          window and application bootstrap
src/platform.rs      platform config paths, default shell and path quoting
src/terminal.rs      PTY session, vt100 screen, grid rendering, cell metrics
src/keys.rs          keystroke → terminal byte encoding
src/theme.rs         theme presets and loading
src/state.rs         panes, tabs, settings, palette state
src/metrics.rs       optional latency profiler
src/screens/         app_shell (window, actions, terminal pump), pane_tree,
                     tab_bar, file_tree, overlays
src/components/      icons, buttons, toggles, text input (4,096-char cap),
                     context menus
src/backend/
  remote_server.rs   HTTP/WebSocket server, pairing, sealed frames, replay
  remote.rs          handshake, device store, protocol types
  remote_client.rs   outbound SpritX↔SpritX links
  osc.rs             OSC 7 / 133 / 7733 parsing (cwd, blocks, aliases)
  history.rs         command history store and search
  config.rs          config load/save/export
  git.rs, files.rs   branch information, per-path git statuses, file-tree helpers
```

## License

MIT — see [LICENSE](LICENSE).

### Stopping running commands

| Control | Behavior |
| --- | --- |
| `Ctrl+C` or right-click → Interrupt process | Interrupt the foreground job, including `npm run dev`, `bun run dev`, and Node servers. The shell remains open. |
| `Ctrl+Shift+K` or right-click → Force stop & restart | Stop the active local terminal's process tree and restart its shell in the same folder. Other panes are unaffected. This resets that terminal's live screen. |
| `Ctrl+Shift+C` | Copy selected text. |
| `Ctrl+D` | Send EOF where the shell/program supports it. |
| `Ctrl+Z` on Linux | Suspend the foreground job; `fg` resumes it. This does not terminate it. |
| `Ctrl+\` on Linux | Send the terminal quit character (`SIGQUIT`). |
| `Ctrl+Z`, then Enter on Windows | EOF for console readers that support the Windows convention. |

Programs may handle or ignore interrupts; force stop is an explicit fallback.
No runtime-specific kill command is substituted into your input. Normal shell
commands such as `kill -TERM <PID>` (Linux), `Stop-Process -Id <PID>` (PowerShell),
and `taskkill /PID <PID> /T /F` (Windows) remain available.

References: [Node signal events](https://nodejs.org/api/process.html#signal-events),
[Bun Ctrl+C](https://bun.sh/guides/process/ctrl-c),
[Bash signals](https://www.gnu.org/s/bash/manual/html_node/Signals.html),
[Windows console signals](https://learn.microsoft.com/en-us/windows/console/ctrl-c-and-ctrl-break-signals),
[taskkill](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/taskkill).

### SpritX Pastel appearance

The stock appearance now uses a dark plum background and pastel ANSI colors.
PowerShell, Bash and Zsh get a two-line Powerline prompt with user, current folder,
Node version (when available), and local time. Prompt styling is scoped to SpritX;
your shell profile is not modified. Custom shell arguments bypass this integration.
The previous stock SpritX Dark theme upgrades automatically; custom themes stay intact.

JetBrainsMonoNL Nerd Font Mono is bundled for the segment arrows and icons.
Font source and SIL Open Font License: [assets/fonts](assets/fonts/README.md).
Colored `ls` output depends on your shell's listing tool and its configuration;
this theme supplies the palette without replacing the listing command.

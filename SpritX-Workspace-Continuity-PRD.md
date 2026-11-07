# SpritX Workspace Continuity and Recent Projects

**Status** — Implemented with Windows validation. Linux runtime validation pending.

**Platforms** — Windows and Linux desktop

**Scope** — Recent Projects, terminal workspace recovery, command status and desktop notifications, Settings, and curl compatibility

## 1. Product goal

Users should be able to reopen SpritX and return to the same workspace they were using, including the project, terminal tab, split pane and working directory. If a terminal process is still alive, reopening should reconnect to it. If the process has ended, the saved workspace and transcript should provide enough context to continue in a fresh shell.

Recent is a project-oriented view of the current and last restored workspace. It gives users a place to see which project is running work, which command finished, and where to continue.

## 2. Sidebar layout

- Keep the sidebar as one continuous surface. In Light mode, the left workspace uses `#F0F2F4` and the right workspace surface uses `#FEFEFE`. Dark mode uses the existing theme tokens.
- Split the space between the sidebar header and the CLI logo strip equally. File Tree occupies the upper half and Recent occupies the lower half. Each list scrolls independently.
- Keep the five CLI logos at the bottom of the same sidebar surface. Preserve click-to-launch and the existing right-click options for Codex, Claude Code, OpenCode, Pi and Oh My Pi.
- Use the existing sidebar typography and spacing. Rows should remain readable at the minimum sidebar width, with truncated paths available in a tooltip.
- When Recent is empty, show a short empty state. Add the first row when tracked work starts in a project folder. Exclude the user's home/profile directory and idle terminal navigation.

## 3. Recent Projects

### Project identity

- Derive the current working directory from the terminal's shell integration. Use the Git repository root as the project identity when available. Otherwise use the resolved working folder.
- Show the actual project name as the primary label. The secondary label shows the pane's current folder or terminal name. Keep the project identity stable as the pane moves between subfolders of the same Git repository.
- Keep pane identity distinct from project identity. Two panes in the same project must remain independently selectable and show their own work status.
- Every row provides a Remove from Recent control. Removing an entry leaves its terminal session available. Idle updates keep the entry removed, while starting new work can add it again.
- Order rows by most recent interaction with the project and pane. Show five by default, with remaining entries reachable by scrolling. Keep recently closed project entries available for reopening.
- When a pane changes projects, update its live association. Earlier project entries may remain as reopenable entries with their last working directory.

### Row interaction

| Session state | On click |
| --- | --- |
| Live in the current window | Activate the correct tab and focus the exact split pane. Keep its running command and input intact. |
| Live in the background | Reattach to the session and restore its tab and pane position when available. |
| Closed or ended | Open a terminal in the last known working directory for that project. Give the new pane keyboard focus. |
| Folder unavailable | Show that the folder could not be opened and let the user choose a valid folder or dismiss the entry. |

For a closed session, the project folder is the entry point for the user's next CLI or command. Conversation recovery depends on the selected CLI's own resume support. Previously executed commands must remain historical output and require a fresh user action to run again.

## 4. Workspace restore and live session continuity

### Saved workspace

Persist the last workspace locally with a versioned format. Save tab names and order, the split tree and ratios, active tab and pane, per-pane working directory and shell configuration, Recent associations, and a bounded tail of terminal output. Save after meaningful changes and during orderly shutdown. Use atomic writes and recover gracefully from missing, stale or corrupt data.

On reopen, restore the previous tab and split structure and focus the last active pane. Reopen a shell in each saved working directory where a live session cannot be recovered. Present saved output as history and make the start of the newly opened shell clear.

### Live session continuity

Use a local background session owner that can keep PTYs and their child processes alive when the SpritX window closes. The desktop app should reconnect through a local, user-scoped channel and recover the available output from the point where it detached. Preserve session identity, input delivery, resize handling and completion status across reconnects.

Define the close actions explicitly. Closing the window hides or minimizes the desktop UI by default and keeps the same window state and terminal sessions alive. A system tray provides Open SpritX and Quit SpritX. A normal second launch reopens the existing window. Explicit Quit stops current terminal sessions. Explicitly closing a pane or ending a session terminates its process and updates Recent. Handle a crashed UI or unavailable session owner by restoring the saved workspace and reporting which sessions ended. Ensure that a reconnect cannot create duplicate shells for an existing live pane.

| Scenario | Expected result |
| --- | --- |
| Close and reopen SpritX while a command is running | Reconnect to the same process if the background owner remains alive. Restore pane placement and show output produced while detached. |
| Explicitly close a pane | End its session. The Recent row can reopen the project in a fresh shell. |
| Restart the PC or lose the background process | Restore saved workspace and available output. Fresh shells reopen in saved folders. Any command-level resume depends on the CLI. |
| Restore is disabled | Start a fresh workspace on the next launch. Make the treatment of any already-detached sessions explicit in the UI. |

Store workspace data under the user's existing SpritX config or data directory. Scope access to the current OS user and keep the transcript bounded. Terminal output can contain secrets, so provide a way to clear saved workspace data and make transcript persistence visible in Settings. Clearing saved data and terminating a live session are separate actions.

## 5. Live work status

Use the existing terminal command lifecycle and session exit signals. Associate events with a specific pane and command. Treat integrations that cannot reliably report command start or completion as status unknown, and avoid inferring completion from a pause in output.

| State | Recent row |
| --- | --- |
| Idle | No animation. |
| Running | Animate a three-dot wave. The wave continues while the tracked command is running, including periods with no output. |
| Completed | Replace the wave with a blue dot. Keep the result visible until acknowledged or new work begins in that pane. |
| Failed | Finish the animation and show the failure outcome and exit code in the status detail and notification. |
| Interrupted or session lost | Show an interrupted or unavailable state when known. |

Show status text in the row detail or tooltip so color alone is not needed to interpret the result. Provide a static running indicator when reduced motion is enabled.

## 6. Desktop completion announcements

- Send an OS desktop notification when a tracked command finishes, including work launched from Recent or a CLI logo.
- Include the project or terminal name and the outcome. Include a recognizable command or CLI name where available, without displaying raw arguments that could contain secrets. Include the exit code on failure.
- Send each completion notification once, including after reconnect. Use the app's existing completion flow so split panes and other terminals follow the same rules.
- When supported by the OS, clicking a notification brings SpritX forward and focuses the related pane. If the pane has ended, show its project in Recent.
- Completion sound follows its own setting. Visual status remains available when desktop notifications or sound are turned off.

## 7. Settings

| Section | Control | Behavior |
| --- | --- | --- |
| General | Restore workspace on startup | Enable by default so reopening returns to the last workspace. Persist the preference across launches. |
| General | Keep terminal sessions running when the window closes | Explain that this keeps processes alive in the background and define how to end them explicitly. |
| General | Saved terminal output | Explain local transcript storage and allow the user to disable or clear it. |
| Notifications | Desktop command completion notifications | Control OS-level completion announcements. Respect existing notification preferences and platform availability. |
| Notifications | Completion sound | Reuse the existing sound preference. Keep it independent of desktop announcements. |

## 8. Curl compatibility

Reproduce the reported failure in SpritX's interactive terminal. Test Windows PowerShell with both `curl` and `curl.exe`. Check command resolution, argument forwarding, quoting, and how multiline requests are pasted. Compare the same commands in the corresponding shell outside SpritX. On Linux, run the same request cases in the supported shell and compare the results.

Use a local HTTP test endpoint to verify a GET request, a custom header, a JSON POST, a URL containing query parameters, and a failing request with a known exit code. Use syntax appropriate to each shell. If Windows resolves `curl` to a PowerShell alias, verify the explicit executable path and document or fix the SpritX-specific behavior identified in testing. Where curl is missing, distinguish that from a terminal defect.

## 9. Acceptance criteria

1. A workspace with two projects, multiple tabs, and several split panes renders a 50/50 sidebar, with one shared sidebar background and five CLI logos at the bottom.
2. Recent shows the correct project and current folder. Moving into a Git project's subfolder keeps its project identity. Clicking each row focuses the correct pane.
3. Starting a long-running command displays the three-dot wave. Successful completion replaces it with a blue dot. Failed or interrupted work reports the correct outcome.
4. Desktop notifications respect Settings, report an outcome once per command, and focus the related pane on platforms that support activation. Sound toggles independently.
5. Closing and reopening the window during a long-running command reconnects to the same process when live continuity is enabled. Output produced while detached is recoverable.
6. With the background process unavailable, reopening restores tab names and order, splits, active pane, folders and saved output without rerunning past commands. A corrupt snapshot or unavailable directory produces a usable fallback.
7. Recent entries for ended panes reopen the last working folder. The new shell is ready for the user's next command.
8. The curl request matrix passes on Windows and Linux. The Windows result records how `curl` and `curl.exe` resolve and which invocation was tested.

## 10. Implementation sequence

1. Add persisted workspace and Recent models. Wire project detection, pane focus and the 50/50 sidebar.
2. Restore tabs, splits, directories and saved output. Then implement the background session owner and reconnect path, including orderly shutdown and failure recovery.
3. Connect Recent statuses, desktop notifications and Settings to tracked terminal events. Test notification deduplication through reconnect.
4. Reproduce and fix the curl issue, then verify the full desktop flow on Windows and Linux. Run real close/reopen, process continuity and notification tests in addition to unit tests.

## 11. Implementation and verification record

- Recent Projects and the equal sidebar split are implemented. Entries identify Git projects and individual panes, focus live panes, and reopen ended projects in their saved folders.
- Workspace snapshots restore tab order, split layout, active pane, directories and bounded output history. Saved output is available through scrollback and remains historical text.
- Local terminal processes can remain in a background owner when workspace recovery and session continuity are enabled. Explicit pane closure ends the process. Authenticated clients use a single active lease, so a second window cannot take over an attached session.
- Settings now expose workspace restore, background continuity, saved terminal output, reduced motion, and saved-workspace clearing. Existing desktop notification duration and sound preferences are respected.
- Recent status uses tracked shell commands and process lifecycle events. Interactive agent turn completion requires the CLI to emit usable lifecycle events. Remote terminals retain their workspace position and require an explicit remote reconnect.
- Windows PowerShell curl resolution now uses the native executable when the default Invoke-WebRequest alias is present. The local HTTP integration test covers GET, headers, JSON POST from a file, query parameters and curl failure code 22.
- Windows verification includes the full automated suite, actual app boot and modal smoke checks, two consecutive workspace reopen cycles, saved-output scrollback, and a real PTY owner detach/reconnect test covering PID, input, output, resize, completion and authentication.
- The File Tree refreshes live by bounded background polling (200 ms poll, 300 ms debounce/cooldown, bounded to expanded directories, capped entries; no filesystem watcher) and shows per-row git status badges computed off the render thread, with nested-repo boundaries respected.
- Linux-specific shell integration supports current-directory and command lifecycle reporting in Bash, Zsh and Fish. The repository's existing Linux/Windows CI matrix includes the new tests. A Linux runtime is unavailable on the current development machine, so a Linux CI or machine run is still required.
- Desktop notification delivery, click activation and audio need a final check against the target desktop's notification and sound services.

## 12. Background desktop and tray update

- Close window to background is enabled by default and can be changed in General settings.
- Windows uses a native notification-area icon. Left click or Open SpritX reopens the same hidden window. Quit SpritX saves workspace state and stops current sessions.
- A per-profile OS file lock and authenticated local reopen endpoint prevent a normal launcher click from creating a second desktop or fresh terminal.
- Linux uses the StatusNotifier backend and retains a minimized window so it remains recoverable from the desktop taskbar as well. Tray visibility depends on the desktop providing a StatusNotifier host.
- Windows native lifecycle validation passed with the real tray icon. It sends a real window-close message, verifies that work completes while hidden, then verifies both a normal second launch and the native tray click callback reopen the same window and terminal PID.
- Linux runtime tray validation remains pending on a Linux desktop.

# Terminal audit fixes — validation

Audit mein identify hue issues ke fixes implement kiye gaye hain. Existing workspace changes preserve kiye gaye hain.

| Issue | Implemented fix | Verification |
| --- | --- | --- |
| Same configuration ke terminals conflict | Har new pane ka independent background owner; reconnect saved connection se hota hai | Identical configurations ke independent ports/PIDs aur detach/reattach tests pass |
| Recovery par repeated output | Recovery snapshot existing parser mein append hone ke bajaye parser/history replace karta hai | Repeated snapshot regression: history length stable |
| Heavy output silently discard | PTY reader bounded backpressure use karta hai; raw ANSI/UTF-8 bytes discard nahi karta | Multi-megabyte stream byte-for-byte verified |
| Long command se growing memory | Per-command captured output preview 512 KiB par bounded | Large capture ke saath command text aur completion status verified |
| Bash prompt command expansion | Path/user values prompt expression mein concatenate nahi hote; parameter references se literal display hote hain | Harmless command-substitution payload literal rehta hai; Bash PTY lifecycle test pass |
| Prompt/output line jumping | Variable prompt spacer elements remove; output, caret, selection aur mouse mapping uniform row geometry use karte hain | Prompt viewport transition geometry regression aur actual app smoke checks pass |
| PowerShell table output cursor movement | Prompt ke andar console buffer inspect karke cursor upar move karna remove | Intentional blank lines aur previous output preserve hone ka test pass |
| Copy se command mein extra breaks | Soft-wrapped rows join hote hain; actual newlines aur wrap-boundary spaces preserve hote hain | Real PTY copy regression pass |
| Proportional fonts se broken columns | Actual platform glyph advances se compatible monospace fonts filter/validate hote hain; incompatible selection reject hoti hai | Font fallback tests aur actual settings startup/render smoke pass |
| Command history mein missing text | PowerShell, Bash aur Zsh integration command-text marker emit karte hain | PowerShell aur Bash real PTY command/lifecycle tests pass |
| Resize/buffer ordering | Pending local output old geometry par parse hota hai; owner resize polling ke saath serialize hota hai aur sized replacement snapshot return karta hai | Owner resize/recovery, ConPTY resize aur preserved-session tests pass |
| Remote reconnect/switch corrupt screen | Truncated raw replay tail ki jagah styled grid snapshot; snapshot/subscription atomic; stale-generation output discard; quiet/fallback panes reset | Loopback encrypted remote tests, colors/alternate-screen/UTF-8 replay aur fallback tests pass |
| Intermittent curl regression test failure | Accepted socket explicitly blocking; each request ke baad real command-completion marker ka wait | Full suite aur 3 consecutive curl test runs pass |

## Test results

- `cargo test -j 2 --lib --tests --no-fail-fast -- --test-threads=4`: **194 passed, 0 failed, 7 UI tests ignored** in the default run.
- Ignored UI tests separately execute kiye: **7 passed, 0 failed**.
- Actual Windows tray close/background/reopen test: **1 passed**; same terminal process preserved.
- Repeated `ls`/alias PTY regression: **12 listings**, har test file exactly **12 times**; command text bhi verified.
- Curl integration test: **3 consecutive additional runs passed**.
- Bash syntax check aur real PTY/security tests passed.
- `cargo build -j 2`: **passed**; debug binary updated, jo `cargo run dev` use karta hai.
- `git diff --check`: passed.

## Verification limits

Linux distribution/desktop runtime is machine par available nahi tha. Cross-platform changes aur Bash integration verify hue hain, lekin Linux GUI, Zsh runtime aur physical mobile-device end-to-end checks pending hain. Remote transport tests local encrypted connections par run hue hain.

Prompt ka visual layout ab uniform terminal grid follow karta hai. Purane dynamic 6px/2px inter-row spacers aur PowerShell cursor compression remove hue hain, kyunki wahi row geometry ko change kar rahe the.

## Follow-up: expanded file-tree crash

- Reported panic: `file_tree.rs` mein `attempt to shift left with overflow`.
- Root cause: path fingerprint code 16-byte chunks ko `u64` mein pack kar raha tha. Ninth byte par shift 64 bits ho jaata tha.
- Fix: chunk width ab `size_of::<u64>()`, yani 8 bytes hai. Debug panic aur release mein overlapping hash bytes dono prevent hote hain.
- Regression: long/Unicode paths, expanded descendants, stable repeated sampling, edits aur deletion — passed.
- Actual expanded-file-tree UI load/watch smoke test — passed. Earlier UI tests collapsed tree use karte the; new test expanded watcher path cover karta hai.
- Updated debug build — passed. Is follow-up ke liye targeted regression/UI tests run hue; poori suite dobara run nahi hui.

## Follow-up: running processes ko interrupt karna

- Bug actual servers par reproduce hua: direct PTY mein Node/npm/Bun ko Ctrl+C mil raha tha, lekin persistent owner ke through Node SIGINT ignore kar raha tha.
- Windows owner ke `CREATE_NEW_PROCESS_GROUP` flag se inherited Ctrl+C-ignore state aa rahi thi. Helper ab `CREATE_NO_WINDOW` use karta hai; ConPTY console isolation retain hoti hai.
- Plain Ctrl+C ka explicit terminal-focus route add hua. Ctrl+Shift+C copy selection ke liye reserved hai.
- Right-click → Interrupt process aur Force stop & restart actions add hue. Ctrl+Shift+K active local pane ka process tree stop karke same folder mein shell restart karta hai.
- Named Ctrl+backslash/bracket keys ab proper control bytes encode karte hain. EOF, Linux suspend/quit aur Windows EOF conventions Settings → Shortcuts aur README mein documented hain.
- Real Node, `npm run dev`, aur `bun run --bun dev` servers: direct aur persistent sessions mein SIGINT received, listening port released, aur original shell reusable raha.
- Actual GPUI keyboard test: Ctrl+C dispatched through the window; SIGINT ignore karne wale server par Ctrl+Shift+K verified; sibling pane ka process unaffected raha.
- Full default suite: **200 passed, 0 failed, 10 display-dependent tests ignored**. Process-control UI test aur Windows tray lifecycle test separately execute hue aur pass hue.
- Linux live runtime/GUI verification is host par available nahi tha.

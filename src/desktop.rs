//! Keep one desktop UI per profile, with a system tray and a reopen endpoint.
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopAction {
    Show,
    Quit,
}

#[derive(Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    token: String,
    pid: u32,
}

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    action: DesktopAction,
}

/// The held OS file lock is released even after an unclean process exit.
pub struct Instance {
    _lock: File,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub events: mpsc::Receiver<DesktopAction>,
}

pub enum Claim {
    Primary(Instance),
    Reopened,
}

/// Result of probing the endpoint a lock holder advertised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EndpointState {
    Alive,
    Dead,
}

impl Instance {
    pub fn claim(root: &Path) -> Result<Claim> {
        crate::backend::session_owner::secure_directory(root)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("desktop.lock"))?;
        let path = root.join("desktop.json");
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let started = std::time::Instant::now();
                loop {
                    // Distinguish "the primary is alive but busy" (retry) from
                    // "the endpoint behind the lock is dead" (stop retrying). A
                    // dead endpoint must never make every relaunch burn the full
                    // 5 s and then fail: the owner's accept thread can exit while
                    // the lock is still held (`Err(_) => break` above), and
                    // spinning on a corpse just blocks every restart. A *missing*
                    // endpoint is different — a primary that just took the lock
                    // has not written it yet — so that case keeps retrying.
                    match Self::read_endpoint(&path) {
                        Ok(endpoint) => match Self::liveness(&endpoint) {
                            EndpointState::Alive => {
                                if Self::send_at(&path, DesktopAction::Show).is_ok() {
                                    return Ok(Claim::Reopened);
                                }
                            }
                            // The lock's owner is either gone (in which case the
                            // OS has already released the lock and this succeeds
                            // on the first try) or wedged with a broken
                            // endpoint. One immediate attempt covers the first
                            // case; the second is a hard failure, so fail now
                            // with an actionable message instead of retrying.
                            EndpointState::Dead => match lock.try_lock() {
                                Ok(()) => break,
                                Err(TryLockError::WouldBlock) => bail!(
                                    "SpritX is running with a dead reopen endpoint (pid {} from desktop.json); \
                                     close the stuck process and start again",
                                    endpoint.pid
                                ),
                                Err(TryLockError::Error(error)) => return Err(error.into()),
                            },
                        },
                        // No endpoint yet: the owner may still be starting up.
                        Err(_) => match lock.try_lock() {
                            Ok(()) => break,
                            Err(TryLockError::WouldBlock) => {}
                            Err(TryLockError::Error(error)) => return Err(error.into()),
                        },
                    }
                    if started.elapsed() >= Duration::from_secs(5) {
                        bail!("SpritX is already running but its reopen endpoint is unavailable");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| anyhow::anyhow!("{error}"))?;
        let endpoint = Endpoint {
            port: listener.local_addr()?.port(),
            token: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            pid: std::process::id(),
        };
        let token = endpoint.token.clone();
        crate::backend::config::write_atomic(&path, &serde_json::to_string(&endpoint)?)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (sender, events) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
                        let mut line = String::new();
                        if BufReader::new((&stream).take(4096))
                            .read_line(&mut line)
                            .is_err()
                        {
                            continue;
                        }
                        let Ok(request) = serde_json::from_str::<Request>(&line) else {
                            continue;
                        };
                        if request.token == token && sender.send(request.action).is_ok() {
                            let _ = stream.write_all(
                                b"OK
",
                            );
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Claim::Primary(Self {
            _lock: lock,
            path,
            stop,
            thread: Some(thread),
            events,
        }))
    }

    /// Read the endpoint the primary wrote, if it is still there.
    fn read_endpoint(path: &Path) -> Result<Endpoint> {
        Ok(serde_json::from_slice(&std::fs::read(path)?)?)
    }

    /// One cheap TCP connect probe. Deliberately a real connect and not just a
    /// PID check: a stale `desktop.json` from a crashed process still carries a
    /// plausible PID (or one the OS has since reused), so only proof that the
    /// accept thread is still listening counts.
    fn endpoint_alive(endpoint: &Endpoint) -> bool {
        TcpStream::connect_timeout(
            &([127, 0, 0, 1], endpoint.port).into(),
            Duration::from_millis(200),
        )
        .is_ok()
    }

    /// Whether the process behind `desktop.json` can still answer.
    fn liveness(endpoint: &Endpoint) -> EndpointState {
        if Self::endpoint_alive(endpoint) {
            EndpointState::Alive
        } else {
            EndpointState::Dead
        }
    }

    pub fn send_at(path: &Path, action: DesktopAction) -> Result<()> {
        let endpoint = Self::read_endpoint(path)?;
        let mut stream = TcpStream::connect_timeout(
            &([127, 0, 0, 1], endpoint.port).into(),
            Duration::from_secs(2),
        )?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        // The token is the only proof this endpoint belongs to the process that
        // wrote `desktop.json`. Handing `AllowSetForegroundWindow` a PID read
        // from a file before that handshake would let anyone who can replace
        // the JSON name an arbitrary process, so it happens only after the
        // primary explicitly acknowledged our token with `OK`.
        let mut bytes = serde_json::to_vec(&Request {
            token: endpoint.token,
            action,
        })?;
        bytes.push(b'\n');
        stream.write_all(&bytes)?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        if line.trim() != "OK" {
            bail!("SpritX did not acknowledge the desktop action");
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(endpoint.pid);
        }
        Ok(())
    }
}

#[cfg(test)]
impl Instance {
    /// Keep the held OS lock alive outside `Drop`, the way a wedged owner would,
    /// so a relaunch has to decide what to do about a lock whose endpoint is
    /// dead instead of simply taking it. The accept thread and listener are
    /// dropped with the rest of the instance, leaving the endpoint genuinely
    /// unreachable.
    pub(crate) fn leak_lock(self) -> File {
        let instance = std::mem::ManuallyDrop::new(self);
        unsafe { std::ptr::read(&instance._lock) }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct Tray {
    _icon: tray_icon::TrayIcon,
    open: tray_icon::menu::MenuId,
    quit: tray_icon::menu::MenuId,
}

impl Tray {
    #[cfg(windows)]
    pub fn native_window(&self) -> isize {
        self._icon.window_handle() as isize
    }
    pub fn new() -> Result<Self> {
        use tray_icon::{
            menu::{Menu, MenuItem, PredefinedMenuItem},
            TrayIconBuilder,
        };
        let menu = Menu::new();
        let open = MenuItem::new("Open SpritX", true, None);
        let quit = MenuItem::new("Quit SpritX", true, None);
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit])?;
        let image = image::load_from_memory(include_bytes!("../assets/icon-128.png"))?.into_rgba8();
        let (width, height) = image.dimensions();
        let icon = tray_icon::Icon::from_rgba(image.into_raw(), width, height)?;
        let icon = TrayIconBuilder::new()
            .with_id("spritx-desktop")
            .with_icon(icon)
            .with_tooltip("SpritX Terminal")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;
        Ok(Self {
            _icon: icon,
            open: open.id().clone(),
            quit: quit.id().clone(),
        })
    }

    pub fn poll(&self) -> Vec<DesktopAction> {
        use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};
        let mut actions = Vec::new();
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                actions.push(DesktopAction::Show);
            }
        }
        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if event.id == self.open {
                actions.push(DesktopAction::Show);
            }
            if event.id == self.quit {
                actions.push(DesktopAction::Quit);
            }
        }
        actions
    }
}

/// Keep the native window and its entity alive while it is in the background.
pub fn background_window(window: &mut gpui::Window) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(handle) = HasWindowHandle::window_handle(window) {
            if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(
                        handle.hwnd.get() as _,
                        windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE,
                    );
                }
                return;
            }
        }
    }
    // GPUI's Wayland and X11 backends expose minimization but no hide/unmap API.
    // The window stays recoverable from both the taskbar and a StatusNotifier tray.
    window.minimize_window();
}

pub fn show_window(window: &mut gpui::Window) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        if let Ok(handle) = HasWindowHandle::window_handle(window) {
            if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                unsafe {
                    let hwnd = handle.hwnd.get() as _;
                    ShowWindow(
                        hwnd,
                        if IsIconic(hwnd) != 0 {
                            SW_RESTORE
                        } else {
                            SW_SHOW
                        },
                    );
                    SetForegroundWindow(hwnd);
                }
            }
        }
    }
    window.activate_window();
    window.refresh();
}

#[cfg(windows)]
pub fn native_window(window: &gpui::Window) -> Result<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .as_raw()
    {
        RawWindowHandle::Win32(handle) => Ok(handle.hwnd.get()),
        _ => bail!("Expected a Win32 desktop window"),
    }
}

#[cfg(windows)]
pub fn lifecycle_test(
    window: gpui::WindowHandle<crate::screens::app_shell::SpritXApp>,
    tray_available: bool,
    tray_window: isize,
    cx: &mut gpui::App,
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    assert!(tray_available, "The real Windows tray icon must be created");
    cx.spawn(async move |cx| {
        let mut ready = false;
        for _ in 0..50 {
            gpui::Timer::after(Duration::from_millis(100)).await;
            ready = window.update(cx, |app, _, _| {
                app.pump_terminals();
                app.session(app.active_tab().active_leaf)
                    .is_some_and(|session| session.powerline_prompt_end().is_some())
            }).unwrap();
            if ready { break; }
        }
        assert!(ready, "The test terminal must be ready");
        let (handle, pid) = window.update(cx, |app, window, _| {
            let pid = app.session(app.active_tab().active_leaf).unwrap().pid().unwrap();
            assert!(app.send_to_terminal(
                "[Console]::WriteLine('BACKGROUND_WORK_STARTED'); Start-Sleep -Milliseconds 1200; [Console]::WriteLine(('BACKGROUND_WORK_'+'FINISHED'))",
                true,
            ));
            (native_window(window).unwrap(), pid)
        }).unwrap();
        gpui::Timer::after(Duration::from_millis(100)).await;
        unsafe { PostMessageW(handle as _, WM_CLOSE, 0, 0); }
        gpui::Timer::after(Duration::from_millis(250)).await;
        assert_eq!(unsafe { IsWindowVisible(handle as _) }, 0, "Cross should hide the desktop window");
        let mut completed = false;
        for _ in 0..50 {
            gpui::Timer::after(Duration::from_millis(100)).await;
            completed = window.update(cx, |app, _, _| {
                app.pump_terminals();
                let session = app.session(app.active_tab().active_leaf).unwrap();
                assert_eq!(session.pid(), Some(pid));
                session.screen_text().contains("BACKGROUND_WORK_FINISHED")
            }).unwrap();
            if completed { break; }
        }
        assert!(completed, "Work must finish while the window is hidden");

        // A normal second launch must activate this process and exit.
        let launch = cx.background_executor().spawn(async {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("dev").env_remove("SPRITX_DESKTOP_TEST")
                .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
                .spawn().unwrap();
            let start = std::time::Instant::now();
            loop {
                if let Some(status) = child.try_wait().unwrap() { assert!(status.success()); break; }
                if start.elapsed() > Duration::from_secs(8) {
                    let _ = child.kill();
                    panic!("Second launch created another UI instead of reopening the first");
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        launch.await;
        gpui::Timer::after(Duration::from_millis(300)).await;
        assert_ne!(unsafe { IsWindowVisible(handle as _) }, 0, "Normal launch must show the hidden window");
        window.update(cx, |app, window, _| {
            assert_eq!(native_window(window).unwrap(), handle);
            assert_eq!(app.session(app.active_tab().active_leaf).unwrap().pid(), Some(pid));
        }).unwrap();
        // Exercise the native tray notification callback, including library event dispatch.
        unsafe { PostMessageW(handle as _, WM_CLOSE, 0, 0); }
        gpui::Timer::after(Duration::from_millis(200)).await;
        assert_eq!(unsafe { IsWindowVisible(handle as _) }, 0);
        // tray-icon 0.26 uses message 6002 for Shell_NotifyIcon mouse notifications.
        unsafe { PostMessageW(tray_window as _, 6002, 0, WM_LBUTTONUP as isize); }
        let mut tray_reopened = false;
        for _ in 0..20 {
            gpui::Timer::after(Duration::from_millis(100)).await;
            tray_reopened = unsafe { IsWindowVisible(handle as _) } != 0;
            if tray_reopened { break; }
        }
        assert!(tray_reopened, "The native tray click must reopen SpritX");
        let root = crate::platform::config_dir().unwrap().join("sessions");
        std::fs::write(root.join("desktop-test-passed"), format!("{pid}")).unwrap();
        // Exercise the same Quit action that the tray menu sends.
        let endpoint = root.join("desktop.json");
        cx.background_executor().spawn(async move {
            Instance::send_at(&endpoint, DesktopAction::Quit).unwrap();
        }).await;
    }).detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_launch_reopens_primary_and_stale_files_do_not_block_restart() {
        let root = std::env::temp_dir().join(format!("spritx-instance-{}", std::process::id()));
        let Claim::Primary(first) = Instance::claim(&root).unwrap() else {
            panic!("Primary instance expected");
        };
        assert!(matches!(Instance::claim(&root).unwrap(), Claim::Reopened));
        assert_eq!(
            first.events.recv_timeout(Duration::from_secs(1)).unwrap(),
            DesktopAction::Show
        );
        drop(first);
        let Claim::Primary(second) = Instance::claim(&root).unwrap() else {
            panic!("The lock must be released");
        };
        Instance::send_at(&root.join("desktop.json"), DesktopAction::Quit).unwrap();
        assert_eq!(
            second.events.recv_timeout(Duration::from_secs(1)).unwrap(),
            DesktopAction::Quit
        );
        drop(second);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// A port nothing is listening on, for a stale `desktop.json`.
    fn dead_port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[test]
    fn dead_endpoint_blocks_restart_only_briefly_and_never_spins() {
        let root = std::env::temp_dir().join(format!("spritx-dead-owner-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let Claim::Primary(first) = Instance::claim(&root).unwrap() else {
            panic!("Primary instance expected");
        };
        // A wedged owner: its accept thread is gone (so the endpoint is dead)
        // but the OS lock is still held by a live process. The relaunch must
        // fail fast instead of retrying the dead endpoint for the full 5 s.
        let lock = first.leak_lock();
        std::fs::write(
            root.join("desktop.json"),
            serde_json::to_string(&Endpoint {
                port: dead_port(),
                token: "0".repeat(64),
                pid: std::process::id(),
            })
            .unwrap(),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let refused = Instance::claim(&root);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "A dead endpoint must not spin for the full 5 s timeout (took {:?})",
            started.elapsed()
        );
        // `Claim` deliberately has no `Debug` (it owns the desktop instance), so
        // match instead of `unwrap_err()`.
        let message = match refused {
            Err(error) => format!("{error:#}"),
            Ok(_) => panic!("A wedged owner must be reported, not silently ignored"),
        };
        assert!(
            message.contains("dead reopen endpoint"),
            "The failure must name the dead endpoint, got: {message}"
        );
        drop(lock);

        // A genuinely dead owner releases the OS lock, so even with the stale
        // `desktop.json` still advertising the dead port, the relaunch must
        // become the primary instead of refusing.
        let started = std::time::Instant::now();
        let claimed = Instance::claim(&root).unwrap();
        assert!(
            matches!(claimed, Claim::Primary(_)),
            "A stale endpoint must not permanently block restart"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(claimed);
        let _ = std::fs::remove_dir_all(root);
    }
}

//! Small OS integrations that don't warrant extra dependencies.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Opens a file or folder with the platform's default handler.
pub fn open(path: &Path) {
    #[cfg(target_os = "macos")]
    let mut cmd = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = Command::new("explorer");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = Command::new("xdg-open");
    let _ = cmd.arg(path).spawn();
}

/// Shows a file selected in Finder or Explorer; other platforms open its folder.
/// A file that does not exist yet reveals its (created) folder instead.
pub fn reveal(path: &Path) {
    let Some(dir) = path.parent() else {
        return;
    };
    if !path.exists() {
        let _ = std::fs::create_dir_all(dir);
        return open(dir);
    }
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg("-R").arg(path).spawn();
    // Explorer parses its own command line: quote the path after `/select,`.
    #[cfg(target_os = "windows")]
    let _ = {
        use std::os::windows::process::CommandExt;
        Command::new("explorer")
            .raw_arg(format!("/select,\"{}\"", path.display()))
            .spawn()
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    open(dir);
}

/// Opens a text file in the user's editor.
pub fn edit(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg("-t").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = Command::new("notepad").arg(path).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = Command::new("xdg-open").arg(path).spawn();
}

pub fn copy_text(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text.to_owned()))
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Launch at login

fn exe() -> Option<PathBuf> {
    std::env::current_exe().ok()?.canonicalize().ok()
}

#[cfg(target_os = "macos")]
mod login {
    use super::*;
    const LABEL: &str = "io.github.coport.gui";

    fn plist() -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(format!("Library/LaunchAgents/{LABEL}.plist")))
    }
    pub fn enabled() -> bool {
        plist().is_some_and(|p| p.is_file())
    }
    pub fn set(enable: bool) -> std::io::Result<()> {
        let path = plist().ok_or_else(|| std::io::Error::other("no home directory"))?;
        if !enable {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let exe = exe().ok_or_else(|| std::io::Error::other("cannot locate executable"))?;
        let exe = xml_escape(&exe.to_string_lossy());
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>--background</string></array>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Interactive</string>
</dict>
</plist>
"#
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, body)
    }
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
}

#[cfg(target_os = "windows")]
mod login {
    use super::*;
    use std::os::windows::process::CommandExt;
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "Coport";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn reg(args: &[&str]) -> bool {
        Command::new("reg")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|o| o.status.success())
    }
    pub fn enabled() -> bool {
        reg(&["query", KEY, "/v", VALUE])
    }
    pub fn set(enable: bool) -> std::io::Result<()> {
        let ok = if enable {
            let exe = exe().ok_or_else(|| std::io::Error::other("cannot locate executable"))?;
            let exe = exe.to_string_lossy();
            // canonicalize() yields a verbatim `\\?\` path, which Run keys reject.
            let exe = exe.strip_prefix(r"\\?\").unwrap_or(&exe);
            reg(&[
                "add",
                KEY,
                "/v",
                VALUE,
                "/t",
                "REG_SZ",
                "/d",
                &format!("\"{exe}\" --background"),
                "/f",
            ])
        } else {
            !enabled() || reg(&["delete", KEY, "/v", VALUE, "/f"])
        };
        if ok {
            Ok(())
        } else {
            Err(std::io::Error::other("registry update failed"))
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod login {
    use super::*;

    fn desktop_file() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join("autostart/coport.desktop"))
    }
    pub fn enabled() -> bool {
        desktop_file().is_some_and(|p| p.is_file())
    }
    pub fn set(enable: bool) -> std::io::Result<()> {
        let path = desktop_file().ok_or_else(|| std::io::Error::other("no config directory"))?;
        if !enable {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        let exe = exe().ok_or_else(|| std::io::Error::other("cannot locate executable"))?;
        // Desktop Entry Exec values quote arguments with double quotes.
        let exe = exe
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let body = format!(
            "[Desktop Entry]\nType=Application\nName=Coport\n\
             Comment=Loopback proxy for Codex and Claude Code\nExec=\"{exe}\" --background\n\
             Terminal=false\nX-GNOME-Autostart-enabled=true\n"
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, body)
    }
}

pub use login::{enabled as launch_at_login, set as set_launch_at_login};

//! Small OS integrations that don't warrant extra dependencies.

use serde::Serialize;
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

/// Adds a CA certificate to the user's trust store once the system confirms
/// it (macOS asks for the login password, Windows shows a security warning).
/// On macOS the result counts only once `probe`, a certificate the CA issued,
/// verifies as clients verify it. Returns false where no user trust store is
/// managed: the user trusts it.
pub fn trust_certificate(
    path: &Path,
    probe: Option<&coport::local_tls::ProxyCaProbe>,
) -> std::io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let home =
            dirs::home_dir().ok_or_else(|| std::io::Error::other("The home folder is unknown."))?;
        ca_trust::install(path, &home, probe)?;
        Ok(true)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = probe;
        let output = Command::new("certutil")
            .args(["-user", "-addstore", "Root"])
            .arg(path)
            .output()?;
        if output.status.success() {
            return Ok(true);
        }
        let reason = String::from_utf8_lossy(&output.stderr);
        let reason = reason
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .unwrap_or("");
        Err(std::io::Error::other(match reason.trim() {
            "" => "The certificate was not trusted.".to_owned(),
            reason => reason.to_owned(),
        }))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (path, probe);
        Ok(false)
    }
}

/// The proxy CA in the user's macOS trust store, through Security framework
/// calls whose errors are returned rather than a tool's exit status.
#[cfg(target_os = "macos")]
mod ca_trust {
    use coport::local_tls::ProxyCaProbe;
    use security_framework::{
        certificate::SecCertificate,
        os::macos::keychain::SecKeychain,
        policy::SecPolicy,
        secure_transport::SslProtocolSide,
        trust::SecTrust,
        trust_settings::{Domain, TrustSettings},
    };
    use std::path::{Path, PathBuf};
    use tokio_rustls::rustls::pki_types::{CertificateDer, pem::PemObject};

    /// errSecDuplicateItem: the keychain already holds the certificate.
    const DUPLICATE_ITEM: i32 = -25299;

    fn failed(error: security_framework::base::Error) -> std::io::Error {
        std::io::Error::other(error.to_string())
    }

    /// The login keychain under `home`.
    pub fn login_keychain(home: &Path) -> PathBuf {
        home.join("Library/Keychains/login.keychain-db")
    }

    /// Store the CA in the PEM file at `path` in the login keychain and trust
    /// it as a root for this user, then [`verify`] it; macOS asks for the
    /// login password. A CA left out of every keychain is never found while
    /// clients build a chain, whatever its trust settings say. A CA this call
    /// added is removed again if it ends up untrusted, as when the password
    /// prompt is cancelled, so retries leave no stray certificates behind.
    pub fn install(path: &Path, home: &Path, probe: Option<&ProxyCaProbe>) -> std::io::Result<()> {
        let der = CertificateDer::from_pem_file(path)
            .map_err(|_| std::io::Error::other("Cannot read the new proxy CA."))?;
        let cert = SecCertificate::from_der(&der).map_err(failed)?;
        let keychain = SecKeychain::open(login_keychain(home)).map_err(failed)?;
        let added = match cert.add_to_keychain(Some(keychain)) {
            Ok(()) => true,
            Err(error) if error.code() == DUPLICATE_ITEM => false,
            Err(error) => return Err(failed(error)),
        };
        let trusted = TrustSettings::new(Domain::User)
            .set_trust_settings_always(&cert)
            .map_err(failed)
            .and_then(|()| verify(probe));
        if trusted.is_err() && added {
            // Best effort: the error that matters is the one returned.
            let _ = cert.delete();
        }
        trusted
    }

    /// Fails unless macOS accepts `probe` as clients would. The trust
    /// settings call has reported success while trustd refused the change
    /// (errSecMissingEntitlement), so success alone proves nothing.
    pub fn verify(probe: Option<&ProxyCaProbe>) -> std::io::Result<()> {
        if probe.is_none_or(accepts) {
            Ok(())
        } else {
            Err(std::io::Error::other(
                "The system still does not trust the certificate.",
            ))
        }
    }

    /// Whether macOS accepts the probe for its host as TLS clients do: the
    /// issuing CA must be found in a keychain and trusted, since only the
    /// probe itself is supplied. Never fetches anything from the network.
    pub fn accepts(probe: &ProxyCaProbe) -> bool {
        let Ok(leaf) = SecCertificate::from_der(&probe.leaf) else {
            return false;
        };
        let policy = SecPolicy::create_ssl(SslProtocolSide::SERVER, Some(&probe.host));
        let Ok(mut trust) = SecTrust::create_with_certificates(&[leaf], &[policy]) else {
            return false;
        };
        trust.set_network_fetch_allowed(false).is_ok() && trust.evaluate_with_error().is_ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Replace & Trust once installed a CA that was reported as trusted
        // but no keychain held, and every proxy-port client failed.
        #[test]
        fn a_ca_the_system_does_not_trust_is_never_installed() {
            let dir = tempfile::tempdir().unwrap();
            let hosts: std::collections::BTreeSet<String> =
                ["182.92.106.196", "cloudcode-pa.googleapis.com"]
                    .map(String::from)
                    .into();
            let mut probed = None;
            let result = coport::local_tls::replace_proxy_ca(dir.path(), &hosts, |_, probe| {
                let probe = probe.unwrap();
                probed = Some(probe.host.clone());
                // As if installing it had reported success.
                verify(Some(probe))
            });
            assert_eq!(probed.as_deref(), Some("cloudcode-pa.googleapis.com"));
            assert_eq!(
                result.unwrap_err().to_string(),
                "The system still does not trust the certificate."
            );
            assert!(!dir.path().join(coport::local_tls::PROXY_CA_FILE).exists());
        }

        #[test]
        fn the_ca_is_stored_in_the_login_keychain() {
            assert_eq!(
                login_keychain(Path::new("/Users/someone")),
                Path::new("/Users/someone/Library/Keychains/login.keychain-db")
            );
        }

        #[test]
        fn a_malformed_probe_is_not_accepted_and_no_probe_is_not_checked() {
            let probe = ProxyCaProbe {
                host: "chatgpt.com".into(),
                leaf: b"not a certificate".to_vec(),
            };
            assert!(!accepts(&probe));
            assert!(verify(Some(&probe)).is_err());
            assert!(verify(None).is_ok());
        }

        // Only a file that is no certificate is tried here: anything else
        // would change the user's keychain and trust settings.
        #[test]
        fn an_unreadable_certificate_is_not_installed() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("ca.pem");
            std::fs::write(&path, "not a certificate").unwrap();
            let error = install(&path, dir.path(), None).unwrap_err();
            assert_eq!(error.to_string(), "Cannot read the new proxy CA.");
        }
    }
}

trait TextClipboard: Send {
    fn set_text(&mut self, text: String) -> Result<(), arboard::Error>;
}
impl TextClipboard for arboard::Clipboard {
    fn set_text(&mut self, text: String) -> Result<(), arboard::Error> {
        arboard::Clipboard::set_text(self, text)
    }
}

/// Linux serves clipboard contents from their owner until another app takes over.
/// Keep that owner in application state, and release it when the app exits.
#[derive(Default)]
pub struct Clipboard {
    inner: std::sync::Mutex<Option<Box<dyn TextClipboard>>>,
}
impl Clipboard {
    pub fn copy_text(&self, text: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_none() {
            *inner = Some(Box::new(
                arboard::Clipboard::new().map_err(|e| e.to_string())?,
            ));
        }
        inner
            .as_mut()
            .unwrap()
            .set_text(text.to_owned())
            .map_err(|e| e.to_string())
    }

    pub fn clear(&self) {
        self.inner.lock().unwrap().take();
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct OwnedText(Arc<Mutex<Option<String>>>);
    impl TextClipboard for OwnedText {
        fn set_text(&mut self, text: String) -> Result<(), arboard::Error> {
            *self.0.lock().unwrap() = Some(text);
            Ok(())
        }
    }
    impl Drop for OwnedText {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = None;
        }
    }

    #[test]
    fn copied_text_remains_owned_until_application_exit() {
        let contents = Arc::new(Mutex::new(None));
        let clipboard = Clipboard {
            inner: Mutex::new(Some(Box::new(OwnedText(contents.clone())))),
        };
        for text in ["first", "replacement"] {
            clipboard.copy_text(text).unwrap();
            assert_eq!(contents.lock().unwrap().as_deref(), Some(text));
        }
        clipboard.clear();
        assert!(contents.lock().unwrap().is_none());
    }
}

// ---------------------------------------------------------------------------
// Launch at login

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginItem {
    Off,
    On,
    /// Registered, but switched off in System Settings (macOS).
    NeedsApproval,
}

impl From<bool> for LoginItem {
    fn from(on: bool) -> Self {
        if on { Self::On } else { Self::Off }
    }
}

fn exe() -> Option<PathBuf> {
    std::env::current_exe().ok()?.canonicalize().ok()
}

#[cfg(target_os = "macos")]
mod login {
    use super::*;
    use objc2::{
        msg_send,
        rc::Retained,
        runtime::{AnyClass, AnyObject},
    };
    use objc2_foundation::{NSError, NSString, NSURL};

    /// Written by earlier versions. macOS cannot attribute a plist outside the
    /// bundle to the app, so it lists it under the developer's name.
    const LEGACY_LABEL: &str = "io.github.coport.gui";
    // SMAppServiceStatus values.
    const ENABLED: isize = 1;
    const REQUIRES_APPROVAL: isize = 2;

    #[link(name = "ServiceManagement", kind = "framework")]
    unsafe extern "C" {}

    /// `SMAppService`, available from macOS 13.
    fn service_class() -> Option<&'static AnyClass> {
        AnyClass::get(c"SMAppService")
    }

    /// The app as a login item, listed under Coport in System Settings;
    /// missing outside an app bundle or before macOS 13.
    fn main_app() -> Option<Retained<AnyObject>> {
        let bundle = exe()?.ancestors().nth(3)?.to_path_buf();
        if bundle.extension().is_none_or(|ext| ext != "app") {
            return None;
        }
        let class = service_class()?;
        // SAFETY: a class property of SMAppService.
        unsafe { msg_send![class, mainAppService] }
    }

    fn status(service: &AnyObject) -> isize {
        // SAFETY: `status` is a read-only NSInteger property of SMAppService.
        unsafe { msg_send![service, status] }
    }

    fn error(e: Retained<NSError>) -> std::io::Error {
        std::io::Error::other(e.localizedDescription().to_string())
    }

    fn legacy_plist() -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(format!("Library/LaunchAgents/{LEGACY_LABEL}.plist")))
    }

    fn legacy_state(path: &Path) -> LoginItem {
        if !path.is_file() {
            return LoginItem::Off;
        }
        let blocked = service_class().is_some_and(|class| {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
            // SAFETY: a class method of SMAppService taking a file URL.
            let status: isize = unsafe { msg_send![class, statusForLegacyURL: &*url] };
            status == REQUIRES_APPROVAL
        });
        if blocked {
            LoginItem::NeedsApproval
        } else {
            LoginItem::On
        }
    }

    fn remove_legacy() -> std::io::Result<()> {
        let Some(path) = legacy_plist() else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    pub fn state() -> LoginItem {
        if let Some(service) = main_app() {
            match status(&service) {
                ENABLED => return LoginItem::On,
                REQUIRES_APPROVAL => return LoginItem::NeedsApproval,
                _ => {}
            }
        }
        legacy_plist().map_or(LoginItem::Off, |path| legacy_state(&path))
    }

    pub fn set(enable: bool) -> std::io::Result<()> {
        let Some(service) = main_app() else {
            return set_legacy(enable);
        };
        if enable {
            if status(&service) != ENABLED {
                // SAFETY: an SMAppService method returning BOOL with an NSError out-parameter.
                let result: Result<(), Retained<NSError>> =
                    unsafe { msg_send![&*service, registerAndReturnError: _] };
                // A login item switched off in System Settings stays
                // registered, awaiting approval there.
                if let Err(e) = result
                    && status(&service) != REQUIRES_APPROVAL
                {
                    return Err(error(e));
                }
            }
        } else if matches!(status(&service), ENABLED | REQUIRES_APPROVAL) {
            // SAFETY: as for registering.
            let result: Result<(), Retained<NSError>> =
                unsafe { msg_send![&*service, unregisterAndReturnError: _] };
            result.map_err(error)?;
        }
        remove_legacy()
    }

    /// Replaces a login item written by an earlier version with the app's own.
    /// One switched off in System Settings stays as it is: registering the
    /// app's own item would turn it back on against that choice.
    pub fn migrate() {
        if main_app().is_some()
            && legacy_plist().is_some_and(|path| legacy_state(&path) == LoginItem::On)
        {
            let _ = set(true);
        }
    }

    const fn code(c: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*c)
    }

    /// Whether macOS opened the app as a login item. Valid only while the app
    /// finishes launching, when the open-application event is current.
    pub fn launched_at_login() -> bool {
        let Some(class) = AnyClass::get(c"NSAppleEventManager") else {
            return false;
        };
        // SAFETY: NSAppleEventManager accessors without arguments.
        let event: Option<Retained<AnyObject>> = unsafe {
            let manager: Option<Retained<AnyObject>> = msg_send![class, sharedAppleEventManager];
            match manager {
                Some(manager) => msg_send![&*manager, currentAppleEvent],
                None => None,
            }
        };
        event.is_some_and(|event| is_login_launch(&event))
    }

    /// An open-application event whose property data is "launched as login item".
    fn is_login_launch(event: &AnyObject) -> bool {
        // SAFETY: NSAppleEventDescriptor accessors; event, keyword and
        // enumeration codes are four-character codes.
        unsafe {
            let id: u32 = msg_send![event, eventID];
            if id != code(b"oapp") {
                return false;
            }
            let data: Option<Retained<AnyObject>> =
                msg_send![event, paramDescriptorForKeyword: code(b"prdt")];
            data.is_some_and(|data| {
                let value: u32 = msg_send![&*data, enumCodeValue];
                value == code(b"lgit")
            })
        }
    }

    /// Opens System Settings at Login Items, where blocked items are allowed.
    pub fn open_settings() {
        if let Some(class) = service_class() {
            // SAFETY: a class method of SMAppService without arguments.
            let () = unsafe { msg_send![class, openSystemSettingsLoginItems] };
        }
    }

    /// Before macOS 13 or outside an app bundle: a plist in the user's
    /// LaunchAgents folder.
    fn set_legacy(enable: bool) -> std::io::Result<()> {
        if !enable {
            return remove_legacy();
        }
        let path = legacy_plist().ok_or_else(|| std::io::Error::other("no home directory"))?;
        let exe = exe().ok_or_else(|| std::io::Error::other("cannot locate executable"))?;
        let exe = xml_escape(&exe.to_string_lossy());
        let body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LEGACY_LABEL}</string>
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

    #[cfg(test)]
    mod tests {
        use super::*;

        /// An open-application event, as macOS sends at launch.
        fn open_event(data: Option<&[u8; 4]>) -> Retained<AnyObject> {
            let class = AnyClass::get(c"NSAppleEventDescriptor").unwrap();
            // SAFETY: NSAppleEventDescriptor constructors and setter taking
            // four-character codes, a nullable target and integer IDs.
            unsafe {
                let event: Option<Retained<AnyObject>> = msg_send![
                    class,
                    appleEventWithEventClass: code(b"aevt"),
                    eventID: code(b"oapp"),
                    targetDescriptor: std::ptr::null::<AnyObject>(),
                    returnID: -1i16,
                    transactionID: 0i32
                ];
                let event = event.unwrap();
                if let Some(data) = data {
                    let value: Option<Retained<AnyObject>> =
                        msg_send![class, descriptorWithEnumCode: code(data)];
                    let () = msg_send![
                        &*event,
                        setParamDescriptor: &*value.unwrap(),
                        forKeyword: code(b"prdt")
                    ];
                }
                event
            }
        }

        #[test]
        fn only_login_item_launches_are_recognized() {
            assert!(is_login_launch(&open_event(Some(b"lgit"))));
            assert!(!is_login_launch(&open_event(None)));
            assert!(!is_login_launch(&open_event(Some(b"xxxx"))));
        }
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
    fn enabled() -> bool {
        reg(&["query", KEY, "/v", VALUE])
    }
    pub fn state() -> LoginItem {
        enabled().into()
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
    pub fn state() -> LoginItem {
        desktop_file().is_some_and(|p| p.is_file()).into()
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
             Comment=Loopback proxy for Codex, Claude, Antigravity and Grok\nExec=\"{exe}\" --background\n\
             Terminal=false\nX-GNOME-Autostart-enabled=true\n"
        );
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, body)
    }
}

#[cfg(target_os = "macos")]
pub use login::{
    launched_at_login, migrate as migrate_launch_at_login, open_settings as open_login_items,
};
pub use login::{set as set_launch_at_login, state as launch_at_login};

/// tray-icon 0.25.1 attaches the menu only while showing it (tray-icon#365);
/// with an earlier release the app does so itself on macOS 27.
fn tray_icon_cfg() {
    println!("cargo:rustc-check-cfg=cfg(tray_icon_attached_menu)");
    let dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let version = dir
        .ancestors()
        .map(|dir| dir.join("Cargo.lock"))
        .find(|lock| lock.is_file())
        .and_then(|lock| {
            println!("cargo:rerun-if-changed={}", lock.display());
            let lock = std::fs::read_to_string(lock).ok()?;
            lock.split("[[package]]")
                .find(|package| package.contains("\nname = \"tray-icon\"\n"))?
                .lines()
                .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
                .map(|version| {
                    version
                        .split(['.', '-'])
                        .take(3)
                        .map(|part| part.parse().unwrap_or(0))
                        .collect::<Vec<u64>>()
                })
        });
    if version.is_none_or(|version| version < vec![0, 25, 1]) {
        println!("cargo:rustc-cfg=tray_icon_attached_menu");
    }
}

fn main() {
    tray_icon_cfg();
    tauri_build::build();
}

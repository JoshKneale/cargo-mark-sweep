#[cfg(target_os = "macos")]
pub use macos::{disable, enable, status};

#[cfg(not(target_os = "macos"))]
pub use stub::{disable, enable, status};

#[cfg(target_os = "macos")]
mod macos {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    use crate::daemon;

    const LABEL: &str = "local.cargo-mark-sweep";

    fn home() -> PathBuf {
        PathBuf::from(env::var("HOME").expect("HOME not set"))
    }

    fn plist_path() -> PathBuf {
        home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
    }

    fn log_path() -> PathBuf {
        home().join("Library/Logs/cargo-mark-sweep.log")
    }

    fn service_target() -> String {
        format!("gui/{}/{LABEL}", unsafe { libc::getuid() })
    }

    fn launchctl(args: &[&str]) -> Result<bool, String> {
        let out = Command::new("launchctl")
            .args(args)
            .output()
            .map_err(|e| format!("launchctl: {e}"))?;
        Ok(out.status.success())
    }

    pub fn enable(dry_run: bool) -> Result<(), String> {
        let exe = env::current_exe()
            .map_err(|e| format!("current_exe: {e}"))?
            .canonicalize()
            .map_err(|e| format!("canonicalize exe: {e}"))?;
        let mut prog_args = vec![exe.to_string_lossy().into_owned(), "daemon".into()];
        if dry_run {
            prog_args.push("--dry-run".into());
        }
        let args_xml: String = prog_args
            .iter()
            .map(|a| format!("        <string>{a}</string>\n"))
            .collect();
        let plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
{args_xml}    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{cargo_bin}:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
            cargo_bin = home().join(".cargo/bin").display(),
            log = log_path().display()
        );

        let path = plist_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
        fs::write(&path, plist).map_err(|e| format!("write {}: {e}", path.display()))?;

        let _ = launchctl(&["bootout", &service_target()]);
        if !launchctl(&[
            "bootstrap",
            &format!("gui/{}", unsafe { libc::getuid() }),
            &path.to_string_lossy(),
        ])? {
            return Err(format!(
                "launchctl bootstrap failed; inspect with: launchctl print {}",
                service_target()
            ));
        }

        println!(
            "enabled{}",
            if dry_run {
                " (dry run — sweeps report only)"
            } else {
                ""
            }
        );
        println!("  agent:  {}", path.display());
        println!("  binary: {}", exe.display());
        println!("  log:    {}", log_path().display());
        println!("  state:  {}", daemon::state_path().display());
        println!("re-run `cargo mark-sweep enable` after upgrading the binary.");
        Ok(())
    }

    pub fn disable() -> Result<(), String> {
        let was_loaded = launchctl(&["bootout", &service_target()])?;
        let path = plist_path();
        let had_plist = path.exists();
        if had_plist {
            fs::remove_file(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
        }
        if was_loaded || had_plist {
            println!("disabled");
        } else {
            println!("was not enabled");
        }
        Ok(())
    }

    pub fn status() -> Result<(), String> {
        let loaded = launchctl(&["print", &service_target()])?;
        println!(
            "daemon: {}",
            if loaded {
                "running (launchd)"
            } else {
                "not loaded"
            }
        );
        println!(
            "agent plist: {} ({})",
            plist_path().display(),
            if plist_path().exists() {
                "present"
            } else {
                "absent"
            }
        );
        println!("log: {}", log_path().display());
        println!();
        daemon::print_state();
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod stub {
    fn unsupported() -> Result<(), String> {
        Err("daemon mode is macOS-only (launchd); use one-shot `cargo mark-sweep` instead".into())
    }

    pub fn enable(_dry_run: bool) -> Result<(), String> {
        unsupported()
    }

    pub fn disable() -> Result<(), String> {
        unsupported()
    }

    pub fn status() -> Result<(), String> {
        unsupported()
    }
}

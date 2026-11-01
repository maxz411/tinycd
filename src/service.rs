//! Installing tinycd as a service, so the hub or an agent comes back with
//! the machine: a systemd unit on Linux, a launchd agent on macOS, and a
//! scheduled task on Windows.

use std::path::PathBuf;

use crate::util::Error;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Serve,
    Agent,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Serve => "tinycd",
            Self::Agent => "tinycd-agent",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::Agent => "agent",
        }
    }
}

/// Write the service definition and, when asked, enable it. Returns the
/// lines to show the user.
pub fn install(
    role: Role,
    data: Option<&std::path::Path>,
    enable: bool,
) -> Result<Vec<String>, Error> {
    let exe = std::env::current_exe()?;
    let exe = exe.display().to_string();
    let mut lines = Vec::new();
    let env_line = data.map(|dir| format!("TINYCD_HOME={}", dir.display()));

    if cfg!(target_os = "linux") {
        #[cfg(unix)]
        let root = unsafe { libc::geteuid() } == 0;
        #[cfg(not(unix))]
        let root = false;
        let (dir, scope) = if root {
            (PathBuf::from("/etc/systemd/system"), "")
        } else {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| crate::util::home_dir().map(|home| home.join(".config")))
                .ok_or("HOME must be set to install a user service")?;
            (config.join("systemd").join("user"), "--user ")
        };
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.service", role.name()));
        let environment = env_line
            .as_ref()
            .map(|line| format!("Environment={line}\n"))
            .unwrap_or_default();
        let unit = format!(
            "[Unit]\nDescription=tinycd {verb}\nAfter=network-online.target\nWants=network-online.target\n\n\
             [Service]\nExecStart={exe} {verb} --no-tui\nRestart=on-failure\nRestartSec=3\n{environment}\n\
             [Install]\nWantedBy={target}\n",
            verb = role.verb(),
            target = if root {
                "multi-user.target"
            } else {
                "default.target"
            },
        );
        std::fs::write(&path, unit)?;
        lines.push(format!("wrote {}", path.display()));
        let enable_command = format!("systemctl {scope}enable --now {}", role.name());
        if enable {
            let _ = std::process::Command::new("systemctl")
                .args(if root {
                    vec!["daemon-reload"]
                } else {
                    vec!["--user", "daemon-reload"]
                })
                .status();
            let mut args: Vec<&str> = Vec::new();
            if !root {
                args.push("--user");
            }
            args.extend(["enable", "--now", role.name()]);
            match std::process::Command::new("systemctl").args(&args).status() {
                Ok(status) if status.success() => {
                    lines.push(format!("enabled and started {}", role.name()))
                }
                _ => lines.push(format!(
                    "could not enable it automatically; run: {enable_command}"
                )),
            }
        } else {
            lines.push(format!("enable it with: {enable_command}"));
        }
        if !root {
            lines.push(
                "so it also runs while you are logged out: loginctl enable-linger $USER".to_owned(),
            );
        }
    } else if cfg!(target_os = "macos") {
        let home = crate::util::home_dir().ok_or("HOME must be set to install a launch agent")?;
        let dir = home.join("Library").join("LaunchAgents");
        std::fs::create_dir_all(&dir)?;
        let label = format!("dev.tinycd.{}", role.verb());
        let path = dir.join(format!("{label}.plist"));
        let environment = env_line
            .as_ref()
            .and_then(|line| line.split_once('='))
            .map(|(key, value)| {
                format!(
                    "    <key>EnvironmentVariables</key>\n    <dict>\n      <key>{key}</key>\n      <string>{value}</string>\n    </dict>\n"
                )
            })
            .unwrap_or_default();
        let logs = home.join("Library").join("Logs");
        let _ = std::fs::create_dir_all(&logs);
        let plist = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n    <key>Label</key>\n    <string>{label}</string>\n\
             <key>ProgramArguments</key>\n    <array>\n      <string>{exe}</string>\n      <string>{verb}</string>\n      <string>--no-tui</string>\n    </array>\n\
             <key>RunAtLoad</key>\n    <true/>\n    <key>KeepAlive</key>\n    <true/>\n{environment}\
             <key>StandardOutPath</key>\n    <string>{logs}/{label}.log</string>\n    <key>StandardErrorPath</key>\n    <string>{logs}/{label}.log</string>\n\
             </dict>\n</plist>\n",
            verb = role.verb(),
            logs = logs.display(),
        );
        std::fs::write(&path, plist)?;
        lines.push(format!("wrote {}", path.display()));
        let load = format!("launchctl load -w {}", path.display());
        if enable {
            match std::process::Command::new("launchctl")
                .args(["load", "-w"])
                .arg(&path)
                .status()
            {
                Ok(status) if status.success() => lines.push(format!("loaded {label}")),
                _ => lines.push(format!("could not load it automatically; run: {load}")),
            }
        } else {
            lines.push(format!("load it with: {load}"));
        }
    } else if cfg!(windows) {
        let environment = env_line
            .map(|line| format!(" (set {line} for the task's user first)"))
            .unwrap_or_default();
        lines.push(format!(
            "create a scheduled task that runs at logon{environment}:\n  schtasks /Create /SC ONLOGON /TN {} /TR \"\\\"{exe}\\\" {} --no-tui\"",
            role.name(),
            role.verb()
        ));
    } else {
        lines.push(format!(
            "run `{exe} {} --no-tui` from your init system",
            role.verb()
        ));
    }
    Ok(lines)
}

use std::{
    env,
    ffi::{CString, OsStr},
    fs::{self, File},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path, PathBuf},
};

const SYSTEM_UNIT_DIRECTORY: &str = "/etc/systemd/system";
const UNIT_NAME: &str = "soulsteads-router.service";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    User,
    System,
}

struct Account {
    name: String,
    home: PathBuf,
}

struct Install {
    scope: Scope,
    run_as: Option<String>,
    dry_run: bool,
}

pub(crate) fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let install = parse(args)?;
    let executable = current_executable()?;
    let (account, config_home) = match install.scope {
        Scope::User => {
            if unsafe { libc::geteuid() } == 0 {
                return Err("refusing to install a user service as root".into());
            }
            (None, user_config_home()?)
        }
        Scope::System => {
            let account = account(install.run_as.as_deref().ok_or(
                "system service installation requires --run-as USER (and USER must not be root)",
            )?)?;
            let config_home = account.home.join(".config");
            (Some(account), config_home)
        }
    };
    let config = absolute_clean(&config_home.join("slotline/config.yaml"))?;
    let unit = unit_text(
        &executable,
        &config,
        install.scope,
        account.as_ref(),
        &config_home,
    )?;
    let directory = match install.scope {
        Scope::User => config_home.join("systemd/user"),
        Scope::System => PathBuf::from(SYSTEM_UNIT_DIRECTORY),
    };
    let destination = directory.join(UNIT_NAME);

    if install.dry_run {
        println!("Would install {}\n\n{unit}", destination.display());
        return Ok(());
    }
    if install.scope == Scope::System && unsafe { libc::geteuid() } != 0 {
        return Err("system service installation must be run with administrator privileges".into());
    }
    let directory = secure_directory(&directory, install.scope == Scope::User)?;
    write_new(&directory, UNIT_NAME, unit.as_bytes())?;
    println!(
        "Installed {}. No service was enabled or started.",
        destination.display()
    );
    Ok(())
}

fn parse(args: &[String]) -> Result<Install, String> {
    if args.first().map(String::as_str) != Some("install") {
        return Err("usage: soulsteads-router service install [--user|--system] [--run-as USER] [--dry-run]".into());
    }
    let mut scope = Scope::User;
    let mut run_as = None;
    let mut dry_run = false;
    let mut scope_set = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--user" => {
                if scope_set {
                    return Err("choose exactly one of --user or --system".into());
                }
                scope = Scope::User;
                scope_set = true;
            }
            "--system" => {
                if scope_set {
                    return Err("choose exactly one of --user or --system".into());
                }
                scope = Scope::System;
                scope_set = true;
            }
            "--run-as" => {
                index += 1;
                let user = args
                    .get(index)
                    .filter(|value| !value.is_empty())
                    .ok_or("--run-as requires a user name")?;
                if run_as.replace(user.clone()).is_some() {
                    return Err("--run-as may be supplied only once".into());
                }
            }
            "--dry-run" if !dry_run => dry_run = true,
            _ => return Err("usage: soulsteads-router service install [--user|--system] [--run-as USER] [--dry-run]".into()),
        }
        index += 1;
    }
    if scope == Scope::User && run_as.is_some() {
        return Err("--run-as is only valid with --system".into());
    }
    Ok(Install {
        scope,
        run_as,
        dry_run,
    })
}

fn current_executable() -> Result<PathBuf, String> {
    let executable = absolute_clean(&env::current_exe().map_err(|error| error.to_string())?)?;
    let metadata = fs::symlink_metadata(&executable).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("current executable is not a regular non-symlink file".into());
    }
    Ok(executable)
}

fn user_config_home() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return absolute_clean(Path::new(&path));
    }
    let home = env::var_os("HOME").ok_or("HOME is required when XDG_CONFIG_HOME is unset")?;
    Ok(absolute_clean(Path::new(&home))?.join(".config"))
}

fn account(name: &str) -> Result<Account, String> {
    if !valid_user_name(name) {
        return Err("--run-as must name a local non-root account".into());
    }
    let name_c = CString::new(name).map_err(|_| "invalid --run-as user name")?;
    let entry = unsafe { libc::getpwnam(name_c.as_ptr()) };
    if entry.is_null() {
        return Err("--run-as must name an existing local account".into());
    }
    let entry = unsafe { &*entry };
    if entry.pw_uid == 0 {
        return Err("refusing a root system service".into());
    }
    let home = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) }
        .to_str()
        .map_err(|_| "run-as account home is not UTF-8")?;
    Ok(Account {
        name: name.to_owned(),
        home: absolute_clean(Path::new(home))?,
    })
}

fn valid_user_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_lowercase() || bytes[0] == b'_')
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(*byte, b'_' | b'-' | b'$')
        })
}

fn absolute_clean(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("service paths must be absolute".into());
    }
    for component in path.components() {
        if !matches!(component, Component::RootDir | Component::Normal(_)) {
            return Err("service paths must be normalized absolute paths".into());
        }
    }
    Ok(path.to_owned())
}

fn systemd_exec_quote(value: &Path) -> Result<String, String> {
    systemd_quote_text(
        value.to_str().ok_or("service paths must be valid UTF-8")?,
        true,
    )
}

fn systemd_setting_quote(value: &Path) -> Result<String, String> {
    systemd_quote_text(
        value.to_str().ok_or("service paths must be valid UTF-8")?,
        false,
    )
}

fn systemd_quote_text(value: &str, escape_dollar: bool) -> Result<String, String> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("service paths may not contain control characters".into());
    }
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            // Only ExecStart-style directives expand `$`; a literal dollar in
            // Environment= or path-list directives must remain a single dollar.
            '$' if escape_dollar => escaped.push_str("$$"),
            '%' => escaped.push_str("%%"),
            _ => escaped.push(character),
        }
    }
    escaped.push('"');
    Ok(escaped)
}

fn unit_text(
    executable: &Path,
    config: &Path,
    scope: Scope,
    account: Option<&Account>,
    config_home: &Path,
) -> Result<String, String> {
    let config_state = config
        .parent()
        .ok_or("service configuration path has no parent")?;
    let executable = systemd_exec_quote(executable)?;
    let config = systemd_exec_quote(config)?;
    let config_state = systemd_setting_quote(config_state)?;
    let config_home = config_home
        .to_str()
        .ok_or("service paths must be valid UTF-8")?;
    let config_home_environment =
        systemd_quote_text(&format!("XDG_CONFIG_HOME={config_home}"), false)?;
    let mut text = String::from(
        "[Unit]\nDescription=Soulsteads Model Router\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\n",
    );
    if let Some(account) = account {
        text.push_str(&format!("User={}\n", account.name));
        let home = account
            .home
            .to_str()
            .ok_or("service paths must be valid UTF-8")?;
        text.push_str(&format!(
            "Environment={}\n",
            systemd_quote_text(&format!("HOME={home}"), false)?
        ));
    }
    text.push_str(&format!("Environment={config_home_environment}\n"));
    text.push_str(&format!(
        "ExecStartPre={executable} --check-config {config}\n"
    ));
    text.push_str(&format!("ExecStart={executable} {config}\n"));
    if scope == Scope::System {
        text.push_str(&format!("ReadWritePaths={config_state}\n"));
    }
    text.push_str("Restart=on-failure\nRestartSec=3\nNoNewPrivileges=yes\n");
    if scope == Scope::System {
        text.push_str("ProtectSystem=strict\nProtectHome=read-only\nPrivateTmp=yes\n");
    }
    text.push_str("RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n\n[Install]\n");
    text.push_str(match scope {
        Scope::User => "WantedBy=default.target\n",
        Scope::System => "WantedBy=multi-user.target\n",
    });
    if scope == Scope::User {
        text.push_str("\n# User service; manage with systemctl --user.\n");
    }
    Ok(text)
}

fn secure_directory(path: &Path, create: bool) -> Result<File, String> {
    let path = absolute_clean(path)?;
    let root = CString::new("/").unwrap();
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(format!("open /: {}", std::io::Error::last_os_error()));
    }
    let mut directory = unsafe { File::from_raw_fd(fd) };
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = component_name(name)?;
        let mut child = open_directory_at(&directory, &name);
        if child.is_err()
            && create
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
        {
            let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
            if created < 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(format!(
                    "create {}: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                ));
            }
            child = open_directory_at(&directory, &name);
        }
        directory = child.map_err(|error| format!("open {}: {error}", path.display()))?;
    }
    Ok(directory)
}

fn component_name(name: &OsStr) -> Result<CString, String> {
    CString::new(name.as_bytes()).map_err(|_| "service path contains a NUL byte".into())
}

fn open_directory_at(directory: &File, name: &CString) -> Result<File, std::io::Error> {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn write_new(directory: &File, name: &str, contents: &[u8]) -> Result<(), String> {
    let name = CString::new(name).unwrap();
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o644,
        )
    };
    if fd < 0 {
        return Err(format!(
            "refusing to replace service unit: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut output = unsafe { File::from_raw_fd(fd) };
    let result = write_and_sync(&mut output, contents);
    if let Err(error) = result {
        // Do not unlink the final name after a late write error. Another actor
        // may have replaced it after our exclusive open; removing by pathname
        // would violate the no-clobber/no-follow installation contract.
        return Err(format!("write service unit: {error}"));
    }
    Ok(())
}

fn write_and_sync(output: &mut File, contents: &[u8]) -> Result<(), std::io::Error> {
    #[cfg(test)]
    if FORCE_NEXT_UNIT_WRITE_FAILURE.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(std::io::Error::other("forced unit write failure"));
    }
    output.write_all(contents).and_then(|()| output.sync_all())
}

#[cfg(test)]
static FORCE_NEXT_UNIT_WRITE_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_units_keep_environment_and_exec_quoting_context_specific() {
        let executable = Path::new("/opt/router $cash%/soulsteads-router");
        let config_home = Path::new("/var/lib/router $cash%");
        let config = config_home.join("slotline/config.yaml");
        let account = Account {
            name: "router".into(),
            home: PathBuf::from("/home/router $cash%"),
        };

        let user = unit_text(executable, &config, Scope::User, None, config_home).unwrap();
        assert!(user.contains("Environment=\"XDG_CONFIG_HOME=/var/lib/router $cash%%\""));
        assert!(user.contains("ExecStart=\"/opt/router $$cash%%/soulsteads-router\" \"/var/lib/router $$cash%%/slotline/config.yaml\""));
        assert!(user.contains("RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX"));
        assert!(user.contains("WantedBy=default.target"));
        assert!(!user.contains("ProtectSystem="));
        assert!(!user.contains("ProtectHome="));
        assert!(!user.contains("ReadWritePaths="));
        assert!(!user.contains("PrivateTmp="));

        let system = unit_text(
            executable,
            &config,
            Scope::System,
            Some(&account),
            config_home,
        )
        .unwrap();
        assert!(system.contains("User=router"));
        assert!(system.contains("Environment=\"HOME=/home/router $cash%%\""));
        assert!(system.contains("ReadWritePaths=\"/var/lib/router $cash%%/slotline\""));
        assert!(system.contains("ProtectSystem=strict"));
        assert!(system.contains("ProtectHome=read-only"));
        assert!(system.contains("PrivateTmp=yes"));
        assert!(system.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn write_failure_does_not_remove_the_created_final_name() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = File::open(temporary.path()).unwrap();
        let name = "soulsteads-router.service";

        // The test-only forced write failure represents a late I/O fault. The
        // final name must remain untouched after that fault: unlinking it could
        // remove a replacement created after the exclusive open.
        FORCE_NEXT_UNIT_WRITE_FAILURE.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(write_new(&directory, name, b"unit").is_err());
        assert!(temporary.path().join(name).exists());
    }
}

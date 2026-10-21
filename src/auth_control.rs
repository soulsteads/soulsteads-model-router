use super::*;

pub(super) fn initial_auth(policy: &Policy) -> Arc<AuthState> {
    let mut classes = HashMap::new();
    let mut credentials = HashMap::new();
    for item in &policy.credentials {
        let cfg = item.config.clone();
        let class_semaphore = classes
            .entry(cfg.class.clone())
            .or_insert_with(|| Arc::new(Semaphore::new(cfg.max_in_flight)))
            .clone();
        credentials.insert(
            cfg.name.clone(),
            Arc::new(CredentialState {
                rate: Mutex::new(RateState {
                    tokens: cfg.burst as f64,
                    last: Instant::now(),
                }),
                class_semaphore,
                config: cfg,
            }),
        );
    }
    Arc::new(AuthState {
        snapshot: policy.key_snapshot.clone(),
        credentials,
    })
}

pub(super) fn replaced_auth(
    previous: &AuthState,
    configs: Vec<config::Credential>,
    snapshot: Arc<key_manager::AuthSnapshot>,
) -> Arc<AuthState> {
    let mut classes: HashMap<(String, usize), Arc<Semaphore>> = HashMap::new();
    for state in previous.credentials.values() {
        classes
            .entry((state.class.clone(), state.max_in_flight))
            .or_insert_with(|| state.class_semaphore.clone());
    }
    let mut credentials = HashMap::new();
    for cfg in configs {
        let state = match previous.credentials.get(&cfg.name) {
            Some(existing) if existing.config == cfg => existing.clone(),
            _ => {
                let class_semaphore = classes
                    .entry((cfg.class.clone(), cfg.max_in_flight))
                    .or_insert_with(|| Arc::new(Semaphore::new(cfg.max_in_flight)))
                    .clone();
                Arc::new(CredentialState {
                    rate: Mutex::new(RateState {
                        tokens: cfg.burst as f64,
                        last: Instant::now(),
                    }),
                    class_semaphore,
                    config: cfg.clone(),
                })
            }
        };
        credentials.insert(cfg.name.clone(), state);
    }
    Arc::new(AuthState {
        snapshot: Some(snapshot),
        credentials,
    })
}

pub(super) async fn reload_auth(app: &AppState, path: &Path) -> bool {
    let _guard = app.reload_lock.lock().await;
    let runtime = app.current();
    let path = path.to_path_buf();
    let loaded = tokio::task::spawn_blocking(move || {
        key_manager::snapshot(&path).map_err(|error| error.to_string())
    })
    .await;
    let current = runtime.auth.read().unwrap().clone();
    let replacement = match loaded {
        Ok(Ok(keys)) => {
            match config::key_credentials_from_snapshot(&runtime.policy.config, &keys) {
                Ok(credentials) => Some(replaced_auth(&current, credentials, Arc::new(keys))),
                Err(error) => {
                    warn!(error=%error, "key store policy invalid; authentication closed");
                    None
                }
            }
        }
        Ok(Err(error)) => {
            warn!(error=%error, "key store reload failed; authentication closed");
            None
        }
        Err(error) => {
            warn!(error=%error, "key store reload task failed; authentication closed");
            None
        }
    };
    let valid = replacement.is_some();
    *runtime.auth.write().unwrap() = replacement.unwrap_or_else(|| {
        Arc::new(AuthState {
            snapshot: Some(Arc::new(HashMap::new())),
            credentials: current.credentials.clone(),
        })
    });
    valid
}

pub(super) fn bind_auth_control(store: &Path) -> io::Result<(UnixListener, PathBuf)> {
    bind_control_socket(key_manager::control_socket_path(store))
}

pub(super) fn policy_control_path(policy: &Path, store: Option<&Path>) -> PathBuf {
    // A policy may live in a shared/read-only repository directory. With a
    // verifier store, use its existing private, service-writable directory.
    let mut path = store.unwrap_or(policy).as_os_str().to_os_string();
    path.push(".policy.sock");
    PathBuf::from(path)
}

pub(super) fn bind_policy_control(
    policy: &Path,
    store: Option<&Path>,
) -> io::Result<(UnixListener, PathBuf)> {
    bind_control_socket(policy_control_path(policy, store))
}

pub(super) fn bind_control_socket(socket: PathBuf) -> io::Result<(UnixListener, PathBuf)> {
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::other("control socket needs a parent"))?;
    let parent_meta = fs::symlink_metadata(parent)?;
    if !parent_meta.file_type().is_dir()
        || parent_meta.uid() != unsafe { libc::geteuid() }
        || parent_meta.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "control directory must be owned by the service and private (0700)",
        ));
    }
    match fs::symlink_metadata(&socket) {
        Ok(meta) => {
            if !meta.file_type().is_socket() || meta.uid() != unsafe { libc::geteuid() } {
                return Err(io::Error::other("unsafe existing control socket path"));
            }
            match std::os::unix::net::UnixStream::connect(&socket) {
                Ok(_) => {
                    return Err(io::Error::other("control socket is already active"));
                }
                Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    fs::remove_file(&socket)?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // The private parent prevents other users from reaching the socket while
    // its permissions are tightened after bind.
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    Ok((listener, socket))
}

pub(super) fn lock_auth_store(store: &Path) -> io::Result<File> {
    let file = File::open(store)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

pub(super) struct ControlSocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}
impl ControlSocketGuard {
    pub(super) fn new(path: PathBuf) -> io::Result<Self> {
        let meta = fs::symlink_metadata(&path)?;
        if !meta.file_type().is_socket() {
            return Err(io::Error::other("control path is not a socket"));
        }
        Ok(Self {
            path,
            device: meta.dev(),
            inode: meta.ino(),
        })
    }
}
impl Drop for ControlSocketGuard {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.file_type().is_socket()
            && meta.dev() == self.device
            && meta.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) async fn auth_reload_loop(app: Arc<AppState>, path: PathBuf, listener: UnixListener) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = interval.tick() => { reload_auth(&app, &path).await; }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        // One task serializes timer and control reloads. An
                        // acknowledgment follows the snapshot swap.
                        handle_auth_control(&app, &path, stream).await;
                    }
                    Err(error) => warn!(error=%error, "key store control accept failed"),
                }
            }
        }
    }
}

pub(super) async fn handle_auth_control(app: &AppState, path: &Path, mut stream: UnixStream) {
    let mut command = [0u8; 7];
    let accepted =
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut command)).await;
    if !matches!(accepted, Ok(Ok(_))) || command != *b"reload\n" {
        let _ = stream.write_all(b"no\n").await;
        return;
    }
    let response = if reload_auth(app, path).await {
        b"ok\n".as_slice()
    } else {
        b"no\n".as_slice()
    };
    let _ = stream.write_all(response).await;
}

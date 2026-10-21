use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tracing::{info, warn};

use rx11_core::config::SshDefaults;

pub struct SshTunnel {
    child: Arc<Mutex<Option<tokio::process::Child>>>,
    local_addr: String,
    cancel: Arc<Notify>,
    /// Notified when the health check gives up, so the owning session can
    /// fail fast instead of grinding through reconnect attempts against a
    /// dead tunnel.
    dead: Arc<Notify>,
    health_task: tokio::task::JoinHandle<()>,
}

impl SshTunnel {
    pub async fn create(
        ssh_host: &str,
        ssh_port: u16,
        ssh_user: Option<&str>,
        remote_relay_port: u16,
        local_bind_port: u16,
        identity_file: Option<&str>,
    ) -> anyhow::Result<Self> {
        let child = create_forward_tunnel(
            ssh_host,
            ssh_port,
            ssh_user,
            remote_relay_port,
            local_bind_port,
            identity_file,
        )
        .await?;

        let local_addr = format!("127.0.0.1:{}", local_bind_port);
        let cancel = Arc::new(Notify::new());
        let dead = Arc::new(Notify::new());
        let cancel_clone = cancel.clone();
        let dead_clone = dead.clone();
        let local_addr_clone = local_addr.clone();
        let child_ref = Arc::new(Mutex::new(Some(child)));

        let health_child = child_ref.clone();
        let health_task = tokio::spawn(async move {
            health_check_loop(&local_addr_clone, cancel_clone, dead_clone, health_child).await;
        });

        Ok(Self {
            child: child_ref,
            local_addr,
            cancel,
            dead,
            health_task,
        })
    }

    /// Handle on the "tunnel is dead" signal for `tokio::select!` usage.
    pub fn dead_notify(&self) -> Arc<Notify> {
        self.dead.clone()
    }

    pub async fn wait(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        let mut child = self.child.lock().await;
        match child.as_mut() {
            Some(c) => c.wait().await.map_err(Into::into),
            None => Err(anyhow::anyhow!("SSH tunnel already killed")),
        }
    }

    pub async fn kill(&mut self) -> anyhow::Result<()> {
        self.cancel.notify_one();
        self.health_task.abort();
        let mut child = self.child.lock().await;
        if let Some(mut c) = child.take() {
            c.kill().await?;
            let _ = c.wait().await;
        }
        Ok(())
    }

    pub fn local_addr(&self) -> &str {
        &self.local_addr
    }
}

// ---------------------------------------------------------------------------
// Supervised tunnels: detect death, rebuild with backoff, keep the session alive.
// ---------------------------------------------------------------------------

/// Object-safe view of "a live tunnel we may have to replace", so the
/// supervisor can be unit-tested against fakes without spawning real ssh.
pub trait TunnelHandle: Send {
    /// Resolves when the tunnel process exits.
    fn wait(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<std::process::ExitStatus>> + Send + '_>>;
    /// The "tunnel is dead" signal from the health check (may never fire).
    fn dead_notify(&self) -> Arc<Notify>;
    /// Best-effort teardown: reap/kill the child and stop the health task.
    fn kill(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

impl TunnelHandle for SshTunnel {
    fn wait(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<std::process::ExitStatus>> + Send + '_>> {
        Box::pin(SshTunnel::wait(self))
    }

    fn dead_notify(&self) -> Arc<Notify> {
        self.dead.clone()
    }

    fn kill(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if let Err(e) = SshTunnel::kill(self).await {
                warn!("SSH tunnel teardown reported: {}", e);
            }
        })
    }
}

/// Why the supervisor considers the current tunnel gone.
pub enum TunnelDeath {
    /// The ssh process exited (with its raw wait() result, errors included).
    Exit(anyhow::Result<std::process::ExitStatus>),
    /// The health check gave up on a still-running process.
    Unhealthy,
}

impl std::fmt::Display for TunnelDeath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TunnelDeath::Exit(Ok(status)) => write!(f, "ssh process exited: {}", status),
            TunnelDeath::Exit(Err(e)) => write!(f, "ssh process wait failed: {}", e),
            TunnelDeath::Unhealthy => write!(f, "health check declared the tunnel dead"),
        }
    }
}

/// Backoff + budget for rebuilding a dead tunnel. Consecutive failed
/// attempts per tunnel death; a successful rebuild resets the count.
#[derive(Debug, Clone, Copy)]
pub struct RebuildPolicy {
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub max_attempts: u32,
}

impl Default for RebuildPolicy {
    fn default() -> Self {
        Self {
            base_delay: SshDefaults::REBUILD_BASE_DELAY,
            max_delay: SshDefaults::REBUILD_MAX_DELAY,
            max_attempts: SshDefaults::REBUILD_MAX_ATTEMPTS,
        }
    }
}

impl RebuildPolicy {
    /// Wait *before* attempt number `failures` (0-based): `base * 2^failures`
    /// capped at `max_delay`. Pure: unit-tested.
    fn backoff_for(&self, failures: u32) -> Duration {
        let base_ms = self.base_delay.as_millis() as u64;
        let capped = base_ms.saturating_mul(1u64 << failures.min(10));
        Duration::from_millis(capped.min(self.max_delay.as_millis() as u64))
    }
}

/// How a supervised tunnel+session pairing ended.
pub enum SupervisionEnd {
    /// The session future finished (Ok = graceful end, Err = the connector
    /// itself gave up or hit a fatal error). Tunnel already torn down.
    Session(anyhow::Result<()>),
    /// Tunnel died while auto-reconnect was disabled — the caller decides.
    TunnelDown(TunnelDeath),
    /// Rebuild budget exhausted for one tunnel death; caller should abort.
    RebuildExhausted { attempts: u32, last_error: String },
}

/// Run `session` (the relay connector) with a tunnel that gets rebuilt when
/// it dies. The session is never aborted by tunnel churn: while we are
/// rebuilding, the connector's own retry loop keeps knocking on the local
/// port, and once a fresh tunnel binds the same port the session resumes
/// through it (server-side session grace makes that transparent). The tunnel
/// is torn down on every exit path; the rebuild wait aborts early if the
/// session gives up first.
///
/// `auto_reconnect = false` preserves the pre-supervision behavior: the
/// first death returns [`SupervisionEnd::TunnelDown`] and nothing is rebuilt.
pub async fn supervise<S, R, RF>(
    mut tunnel: Box<dyn TunnelHandle>,
    session: S,
    mut rebuild: R,
    policy: RebuildPolicy,
    auto_reconnect: bool,
) -> SupervisionEnd
where
    S: Future<Output = anyhow::Result<()>>,
    R: FnMut() -> RF,
    RF: Future<Output = anyhow::Result<Box<dyn TunnelHandle>>>,
{
    let mut session = Box::pin(session);
    let mut failures: u32 = 0;
    let mut last_error: Option<String> = None;

    loop {
        // Subscribe to the health signal *before* parking on the arms, so a
        // death announced between tunnel creation and this loop cannot be
        // missed (notify_waiters only wakes current waiters).
        let dead = tunnel.dead_notify();
        let mut dead_wait = Box::pin(dead.notified());
        dead_wait.as_mut().enable();

        enum Watched {
            Done(anyhow::Result<()>),
            Dead(TunnelDeath),
        }
        let watched = tokio::select! {
            biased;
            r = &mut session => Watched::Done(r),
            status = tunnel.wait() => Watched::Dead(TunnelDeath::Exit(status)),
            _ = dead_wait => Watched::Dead(TunnelDeath::Unhealthy),
        };
        // Reap whatever is left (idempotent) before touching the port again.
        tunnel.kill().await;
        let death = match watched {
            Watched::Done(r) => return SupervisionEnd::Session(r),
            Watched::Dead(d) => d,
        };

        if !auto_reconnect {
            return SupervisionEnd::TunnelDown(death);
        }
        warn!(
            "SSH tunnel lost ({}); auto-rebuild enabled, trying to bring it back",
            death
        );

        loop {
            if failures >= policy.max_attempts {
                return SupervisionEnd::RebuildExhausted {
                    attempts: failures,
                    last_error: last_error
                        .unwrap_or_else(|| "tunnel could not be re-established".to_string()),
                };
            }
            // Session may give up (its retry budget is independent) while we
            // wait or while an attempt is in flight — stop burning rebuilds.
            tokio::select! {
                biased;
                r = &mut session => return SupervisionEnd::Session(r),
                _ = tokio::time::sleep(policy.backoff_for(failures)) => {}
            }
            let attempt = tokio::select! {
                biased;
                r = &mut session => return SupervisionEnd::Session(r),
                t = rebuild() => t,
            };
            match attempt {
                Ok(t) => {
                    info!("SSH tunnel rebuilt after {} failed attempt(s)", failures);
                    tunnel = t;
                    failures = 0;
                    last_error = None;
                    break;
                }
                Err(e) => {
                    failures += 1;
                    warn!("SSH tunnel rebuild attempt {} failed: {}", failures, e);
                    last_error = Some(e.to_string());
                }
            }
        }
    }
}

async fn health_check_loop(
    local_addr: &str,
    cancel: Arc<Notify>,
    dead: Arc<Notify>,
    child: Arc<Mutex<Option<tokio::process::Child>>>,
) {
    let mut interval = tokio::time::interval(SshDefaults::HEALTH_CHECK_INTERVAL);
    interval.tick().await;

    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            _ = cancel.notified() => break,
            _ = interval.tick() => {
                let process_alive = {
                    let mut guard = child.lock().await;
                    match guard.as_mut() {
                        Some(c) => c.try_wait().ok().flatten().is_none(),
                        None => false,
                    }
                };

                let port_alive = check_tunnel_port(local_addr).await;

                let healthy = process_alive && port_alive;

                if healthy {
                    if consecutive_failures > 0 {
                        info!("SSH tunnel health check recovered");
                    }
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    if !process_alive {
                        warn!(
                            "SSH process exited (port_alive={}) ({}/{})",
                            port_alive, consecutive_failures, SshDefaults::MAX_CONSECUTIVE_FAILURES
                        );
                    } else {
                        warn!(
                            "SSH tunnel health check failed ({}/{})",
                            consecutive_failures, SshDefaults::MAX_CONSECUTIVE_FAILURES
                        );
                    }
                    if consecutive_failures >= SshDefaults::MAX_CONSECUTIVE_FAILURES {
                        warn!(
                            "SSH tunnel health check failed {} consecutive times, tunnel appears dead",
                            SshDefaults::MAX_CONSECUTIVE_FAILURES
                        );
                        dead.notify_waiters();
                        break;
                    }
                }
            }
        }
    }
}

async fn check_tunnel_port(addr: &str) -> bool {
    let result = tokio::time::timeout(SshDefaults::HEALTH_CHECK_TIMEOUT, async {
        let mut stream = TcpStream::connect(addr).await?;
        stream.shutdown().await?;
        Ok::<(), std::io::Error>(())
    })
    .await;

    match result {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => false,
        Err(_) => false,
    }
}

/// ssh(1) options for every tunnel: no remote command, no tty, fail fast on
/// forward errors, never prompt, accept-new host keys, TCP keepalive.
const SSH_TUNNEL_OPTS: [&str; 5] = [
    "ExitOnForwardFailure=yes",
    "BatchMode=yes",
    "StrictHostKeyChecking=accept-new",
    "ServerAliveInterval=15",
    "ServerAliveCountMax=3",
];

/// Build the full ssh(1) argv for a `-L` forward tunnel (pure: unit-tested).
fn build_tunnel_args(
    ssh_host: &str,
    ssh_port: u16,
    ssh_user: Option<&str>,
    remote_relay_port: u16,
    local_bind_port: u16,
    identity_file: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-N".to_string(), "-T".to_string()];
    args.extend(
        SSH_TUNNEL_OPTS
            .iter()
            .flat_map(|opt| ["-o".to_string(), (*opt).to_string()]),
    );

    args.push("-L".to_string());
    args.push(format!(
        "127.0.0.1:{}:127.0.0.1:{}",
        local_bind_port, remote_relay_port
    ));

    if let Some(id) = identity_file {
        args.push("-i".to_string());
        args.push(id.to_string());
    }

    args.push("-p".to_string());
    args.push(ssh_port.to_string());

    let target = match ssh_user {
        Some(u) => format!("{}@{}", u, ssh_host),
        None => ssh_host.to_string(),
    };
    args.push(target);
    args
}

pub async fn create_forward_tunnel(
    ssh_host: &str,
    ssh_port: u16,
    ssh_user: Option<&str>,
    remote_relay_port: u16,
    local_bind_port: u16,
    identity_file: Option<&str>,
) -> anyhow::Result<tokio::process::Child> {
    let args = build_tunnel_args(
        ssh_host,
        ssh_port,
        ssh_user,
        remote_relay_port,
        local_bind_port,
        identity_file,
    );

    info!(
        "Starting SSH tunnel: {}@{}:{} -> 127.0.0.1:{}",
        ssh_user.unwrap_or("<default>"),
        ssh_host,
        ssh_port,
        local_bind_port
    );

    let child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_tunnel_args_full_shape() {
        let args = build_tunnel_args(
            "example.com",
            2222,
            Some("root"),
            7000,
            17001,
            Some("/home/me/.ssh/id_ed25519"),
        );
        assert_eq!(&args[0..2], ["-N", "-T"]);
        // every ssh option is an "-o key=value" pair
        let opts: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "-o")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(opts, SSH_TUNNEL_OPTS.to_vec());
        let l_idx = args.iter().position(|a| a == "-L").unwrap();
        assert_eq!(args[l_idx + 1], "127.0.0.1:17001:127.0.0.1:7000");
        let i_idx = args.iter().position(|a| a == "-i").unwrap();
        assert_eq!(args[i_idx + 1], "/home/me/.ssh/id_ed25519");
        let p_idx = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(args[p_idx + 1], "2222");
        assert_eq!(args.last().unwrap(), "root@example.com");
    }

    #[test]
    fn test_build_tunnel_args_optional_identity_absent() {
        let args = build_tunnel_args("host", 22, None, 7000, 12345, None);
        assert!(!args.iter().any(|a| a == "-i"));
        assert_eq!(args.last().unwrap(), "host");
    }

    #[tokio::test]
    async fn test_check_tunnel_health_unreachable() {
        assert!(!check_tunnel_port("127.0.0.1:1").await);
    }

    #[tokio::test]
    async fn test_check_tunnel_health_with_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();

        let listener_task = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });

        assert!(check_tunnel_port(&format!("127.0.0.1:{}", port)).await);
        listener_task.abort();
    }

    // ---- supervisor tests: fake tunnels, no real ssh processes ----

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;

    /// How a fake tunnel dies: an immediate process exit on `wait()`, or a
    /// health signal that fires shortly after construction (Unhealthy).
    enum FakeDeath {
        ProcessExit(&'static str),
        Unhealthy,
    }

    struct FakeTunnel {
        /// Some(Exit) is consumed by wait(); Unhealthy keeps its Notify.
        death: Mutex<Option<FakeDeath>>,
        notify: Arc<Notify>,
        killed: Arc<AtomicBool>,
    }

    impl TunnelHandle for FakeTunnel {
        fn wait(
            &mut self,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<std::process::ExitStatus>> + Send + '_>>
        {
            let exit = match self.death.lock().unwrap().as_ref() {
                Some(FakeDeath::ProcessExit(msg)) => Some(*msg),
                _ => None,
            };
            match exit {
                Some(msg) => Box::pin(async move { Err(anyhow::anyhow!(msg)) }),
                None => Box::pin(std::future::pending()),
            }
        }

        fn dead_notify(&self) -> Arc<Notify> {
            self.notify.clone()
        }

        fn kill(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            let killed = self.killed.clone();
            Box::pin(async move {
                killed.store(true, Ordering::SeqCst);
            })
        }
    }

    /// One scripted outcome for a rebuild attempt.
    enum Step {
        Fail,
        Alive,
        /// Rebuilt successfully, but dies immediately afterwards.
        ProcessDies,
        /// Stays in flight long enough to be cancelled by a session end.
        SlowAlive,
    }

    /// Scripted factory + bookkeeping, all behind Arc so the rebuild closure
    /// returns a self-contained future (FnMut() -> RF cannot borrow state).
    #[derive(Clone)]
    struct Factory {
        steps: Arc<Mutex<VecDeque<Step>>>,
        calls: Arc<AtomicU32>,
        killed: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
    }

    impl Factory {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: Arc::new(Mutex::new(steps.into())),
                calls: Arc::new(AtomicU32::new(0)),
                killed: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }

        /// Number of tunnels that were handed out and then reaped via kill().
        fn all_killed(&self) -> usize {
            self.killed
                .lock()
                .unwrap()
                .iter()
                .filter(|k| k.load(Ordering::SeqCst))
                .count()
        }

        /// Build a tracked initial tunnel (registers its kill flag).
        fn tunnel(&self, death: Option<FakeDeath>) -> Box<dyn TunnelHandle> {
            let flag = Arc::new(AtomicBool::new(false));
            self.killed.lock().unwrap().push(flag.clone());
            let notify = Arc::new(Notify::new());
            if matches!(death, Some(FakeDeath::Unhealthy)) {
                let fire = notify.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    fire.notify_waiters();
                });
            }
            Box::new(FakeTunnel {
                death: Mutex::new(death),
                notify,
                killed: flag,
            })
        }

        async fn make(self) -> anyhow::Result<Box<dyn TunnelHandle>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let step = self.steps.lock().unwrap().pop_front();
            match step {
                Some(Step::Fail) | None => Err(anyhow::anyhow!("no route to host")),
                Some(Step::SlowAlive) => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(self.tunnel(None))
                }
                Some(Step::Alive) => Ok(self.tunnel(None)),
                Some(Step::ProcessDies) => {
                    Ok(self.tunnel(Some(FakeDeath::ProcessExit("fake ssh exited"))))
                }
            }
        }
    }

    fn fast_policy() -> RebuildPolicy {
        RebuildPolicy {
            base_delay: Duration::from_millis(4),
            max_delay: Duration::from_millis(10),
            max_attempts: 3,
        }
    }

    /// Awaitable session stub that succeeds after `ms` (0 = immediately).
    async fn session_ok(ms: u64) -> anyhow::Result<()> {
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        Ok(())
    }

    async fn session_err_after(ms: u64, msg: &'static str) -> anyhow::Result<()> {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Err(anyhow::anyhow!(msg))
    }

    /// Debug label for match-only assertions.
    fn end_kind(e: &SupervisionEnd) -> &'static str {
        match e {
            SupervisionEnd::Session(Ok(())) => "Session(Ok)",
            SupervisionEnd::Session(Err(_)) => "Session(Err)",
            SupervisionEnd::TunnelDown(_) => "TunnelDown",
            SupervisionEnd::RebuildExhausted { .. } => "RebuildExhausted",
        }
    }

    /// Pure backoff arithmetic of the rebuild budget.
    #[test]
    fn rebuild_backoff_doubles_then_caps() {
        let p = RebuildPolicy {
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(25),
            max_attempts: 9,
        };
        assert_eq!(p.backoff_for(0), Duration::from_millis(10));
        assert_eq!(p.backoff_for(1), Duration::from_millis(20));
        assert_eq!(p.backoff_for(2), Duration::from_millis(25)); // capped
        assert_eq!(p.backoff_for(64), Duration::from_millis(25)); // saturating shift
    }

    #[tokio::test]
    async fn session_end_wins_and_tears_tunnel_down() {
        let factory = Factory::new(vec![]);
        let factory2 = factory.clone();
        let end = supervise(
            factory.tunnel(None),
            session_ok(0),
            move || factory2.clone().make(),
            fast_policy(),
            true,
        )
        .await;
        assert!(
            matches!(end, SupervisionEnd::Session(Ok(()))),
            "{:?}",
            end_kind(&end)
        );
        assert_eq!(factory.all_killed(), 1, "live tunnel reaped on session end");
        assert_eq!(factory.calls(), 0);
    }

    #[tokio::test]
    async fn tunnel_death_rebuilds_and_session_survives() {
        let factory = Factory::new(vec![Step::Alive]);
        let factory2 = factory.clone();
        let end = supervise(
            factory.tunnel(Some(FakeDeath::ProcessExit("gone"))),
            session_ok(30),
            move || factory2.clone().make(),
            fast_policy(),
            true,
        )
        .await;
        assert!(
            matches!(end, SupervisionEnd::Session(Ok(()))),
            "{:?}",
            end_kind(&end)
        );
        assert_eq!(factory.calls(), 1);
        // Original tunnel killed on death, rebuilt tunnel killed on session end.
        assert_eq!(factory.all_killed(), 2);
    }

    #[tokio::test]
    async fn unhealthy_health_signal_also_rebuilds() {
        let factory = Factory::new(vec![Step::Alive]);
        let factory2 = factory.clone();
        let end = supervise(
            factory.tunnel(Some(FakeDeath::Unhealthy)),
            session_ok(100),
            move || factory2.clone().make(),
            fast_policy(),
            true,
        )
        .await;
        assert!(
            matches!(end, SupervisionEnd::Session(Ok(()))),
            "{:?}",
            end_kind(&end)
        );
        assert_eq!(factory.calls(), 1);
        assert_eq!(factory.all_killed(), 2, "unhealthy tunnel was reaped");
    }

    #[tokio::test]
    async fn kill_switch_off_returns_tunnel_down_without_rebuilding() {
        let factory = Factory::new(vec![]);
        let factory2 = factory.clone();
        let end = supervise(
            factory.tunnel(Some(FakeDeath::Unhealthy)),
            std::future::pending::<anyhow::Result<()>>(),
            move || factory2.clone().make(),
            fast_policy(),
            false,
        )
        .await;
        assert!(matches!(
            end,
            SupervisionEnd::TunnelDown(TunnelDeath::Unhealthy)
        ));
        assert_eq!(factory.calls(), 0);
        assert_eq!(factory.all_killed(), 1);
    }

    #[tokio::test]
    async fn rebuild_budget_exhaustion_reports_attempts() {
        let factory = Factory::new(vec![]); // empty => every attempt fails
        let factory2 = factory.clone();
        let end = supervise(
            factory.tunnel(Some(FakeDeath::ProcessExit("gone"))),
            std::future::pending::<anyhow::Result<()>>(),
            move || factory2.clone().make(),
            fast_policy(), // max_attempts = 3
            true,
        )
        .await;
        match end {
            SupervisionEnd::RebuildExhausted {
                attempts,
                last_error,
            } => {
                assert_eq!(attempts, 3);
                assert_eq!(last_error, "no route to host");
            }
            other => panic!("expected exhaustion, got {:?}", end_kind(&other)),
        }
        assert_eq!(factory.calls(), 3);
    }

    #[tokio::test]
    async fn budget_is_per_death_not_lifetime() {
        // fast_policy allows 3 consecutive failures per death. Each death here
        // eats one failure before succeeding, so a lifetime-global counter
        // would trip on the second death.
        let factory = Factory::new(vec![
            Step::Fail,
            Step::ProcessDies, // rebuild lands, but dies again
            Step::Fail,
            Step::Alive,
        ]);
        let factory2 = factory.clone();
        let end = supervise(
            factory.tunnel(Some(FakeDeath::ProcessExit("gone"))),
            session_ok(60),
            move || factory2.clone().make(),
            fast_policy(),
            true,
        )
        .await;
        assert!(
            matches!(end, SupervisionEnd::Session(Ok(()))),
            "{:?}",
            end_kind(&end)
        );
        assert_eq!(factory.calls(), 4);
    }

    #[tokio::test]
    async fn session_giving_up_during_backoff_stops_rebuilds() {
        let factory = Factory::new(vec![]);
        let factory2 = factory.clone();
        let slow = RebuildPolicy {
            base_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(5),
            max_attempts: 3,
        };
        let end = supervise(
            factory.tunnel(Some(FakeDeath::ProcessExit("gone"))),
            session_err_after(20, "connector gave up"),
            move || factory2.clone().make(),
            slow,
            true,
        )
        .await;
        match end {
            SupervisionEnd::Session(Err(e)) => assert_eq!(e.to_string(), "connector gave up"),
            other => panic!("expected session error, got {:?}", end_kind(&other)),
        }
        assert_eq!(factory.calls(), 0, "rebuild wait aborted early");
    }

    #[tokio::test]
    async fn session_giving_up_mid_rebuild_attempt_stops_rebuilds() {
        let factory = Factory::new(vec![Step::SlowAlive]);
        let factory2 = factory.clone();
        let end = supervise(
            factory.tunnel(Some(FakeDeath::ProcessExit("gone"))),
            session_err_after(30, "connector gave up"),
            move || factory2.clone().make(),
            fast_policy(),
            true,
        )
        .await;
        match end {
            SupervisionEnd::Session(Err(e)) => assert_eq!(e.to_string(), "connector gave up"),
            other => panic!("expected session error, got {:?}", end_kind(&other)),
        }
        assert_eq!(factory.calls(), 1, "attempt started but was cancelled");
    }
}

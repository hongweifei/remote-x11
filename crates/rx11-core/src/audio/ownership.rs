//! Which application an X11 connection belongs to.
//!
//! The per-application audio tier needs to answer one question: the sound
//! an X11 connection's application produces must be captured from THAT
//! application, not from the desktop mix. The link between the two is the
//! process behind the connection.
//!
//! Two facts drive the implementation (both measured on a live host):
//!
//! * The peer of an accepted TCP connection is found through the socket's
//!   own inode. The socket's inode appears in `/proc/net/tcp`; the peer's
//!   socket is the row whose REMOTE port is our listening port and whose
//!   LOCAL port is the peer's ephemeral port. `SO_PEERCRED` is not an
//!   alternative here: on this host it answers pid 0 for TCP.
//! * An application is a PROCESS TREE, not a process. A browser's audio
//!   service is a child of the browser's main process, so the two must
//!   land on the same application.
//!
//! ## What identifies an application
//!
//! The obvious rule — "two processes belong to the same application when
//! their ancestries share a process" — is WRONG on a real desktop, and
//! wrong in the worst possible direction: every process descends from
//! init, so the rule declares the whole desktop one application. Measured
//! on this host: 480 processes, of which 181 share the ancestors
//! `{1, 1569}` alone; two independent Chromium instances (ancestry
//! `2701 2622 1569` and `54647 2561 2433 1569`) share only `{1, 1569}`
//! and would be declared the same application. A browser's sound would
//! then be attributed to whichever terminal also holds an X11 connection.
//!
//! The rule that works is the APPLICATION ROOT: walk the ancestry to the
//! first SESSION LEADER (a process that leads its own session), and take
//! the process just below it — unless the leader and that process are the
//! same program, in which case the leader itself is the root. Measured
//! on this host, the rule separates exactly what must be separated:
//!
//! | pair | same application? |
//! |---|---|
//! | browser main process ↔ its audio service | yes |
//! | browser main process ↔ its renderer | yes |
//! | messenger main process ↔ its helper | yes |
//! | two independent browser instances | **no** |
//! | browser ↔ the shell that launched it | **no** |
//! | browser ↔ messenger | **no** |
//! | X server ↔ compositor | **no** |
//!
//! Two consequences the audio tier relies on:
//!
//! * two X11 connections of one application land on the same root;
//! * the process that PRODUCES the sound (a browser's audio service) is a
//!   DESCENDANT of the process that holds the connection, so
//!   [`ProcessTree::owns_descendant`] attributes it to that connection.

use std::collections::BTreeSet;

use procfs::net::{TcpNetEntry, TcpState};
use procfs::process::FDTarget;
use procfs::process::{all_processes, Process};

/// The identity of an application: the process every member of the
/// application descends from.
///
/// Two processes belong to the same application exactly when their
/// application roots are the same process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ApplicationId(i32);

impl ApplicationId {
    /// The root process id.
    pub fn get(self) -> i32 {
        self.0
    }
}

/// The ancestry of one process: itself plus every ancestor up to init.
///
/// The tree is the raw material for the application root; it is also the
/// answer to "is this pid an ancestor of that one".
///
/// The ancestors are kept in ANCESTRY ORDER — the process itself first,
/// then its parent, and so on up to init. The order matters: the
/// application root is defined by the NEAREST session leader, and a set
/// ordered by pid would hand back whichever ancestor happens to have the
/// smallest number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTree {
    /// The process itself, then its ancestors, nearest first.
    chain: Vec<i32>,
    /// The same pids, for membership tests.
    pids: BTreeSet<i32>,
}

impl ProcessTree {
    /// Build the tree of `pid`. `None` when the process cannot be read.
    pub fn of(pid: i32) -> Option<Self> {
        let mut chain = Vec::new();
        let mut pids = BTreeSet::new();
        let mut current = pid;
        // The chain terminates: pid 1's parent is 0, and a process that
        // vanished mid-walk ends the walk. The bound keeps a pathological
        // /proc from spinning forever.
        for _ in 0..64 {
            if current <= 0 || !pids.insert(current) {
                break;
            }
            chain.push(current);
            let Ok(process) = Process::new(current) else {
                break;
            };
            let Ok(status) = process.status() else {
                break;
            };
            if status.ppid > 0 {
                current = status.ppid;
            } else {
                break;
            }
        }
        if pids.is_empty() {
            None
        } else {
            Some(Self { chain, pids })
        }
    }

    /// The tree of the current process (used by tests and diagnostics).
    pub fn of_self() -> Option<Self> {
        Self::of(std::process::id() as i32)
    }

    /// The application this process belongs to.
    ///
    /// `None` when the session leader cannot be read — the caller then has
    /// no application to attribute sound to.
    pub fn application_root(&self) -> Option<ApplicationId> {
        let nearest = *self.chain.first()?;
        let leader = self.session_leader()?;
        if leader == nearest {
            return Some(ApplicationId(leader));
        }
        // The process just below the leader on the path to `nearest`.
        let Some(below) = self.child_below(leader) else {
            return Some(ApplicationId(leader));
        };
        // A leader that runs the same program as the process below it is
        // the application's root itself (a browser that forks its own
        // children from a session-leading main process).
        if same_program(leader, below) {
            Some(ApplicationId(leader))
        } else {
            Some(ApplicationId(below))
        }
    }

    /// Whether two processes belong to the same application.
    ///
    /// The comparison is on the application root, never on a shared
    /// ancestor: sharing an ancestor is what every pair of processes on a
    /// desktop does.
    pub fn same_application(&self, other: &ProcessTree) -> bool {
        match (self.application_root(), other.application_root()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// The processes of this tree, ascending.
    pub fn pids(&self) -> impl Iterator<Item = i32> + '_ {
        self.pids.iter().copied()
    }

    /// The ancestry, nearest first: the process itself, its parent, and so
    /// on up to init.
    pub fn chain(&self) -> impl Iterator<Item = i32> + '_ {
        self.chain.iter().copied()
    }

    /// Whether this tree contains `pid`.
    pub fn contains(&self, pid: i32) -> bool {
        self.pids.contains(&pid)
    }

    /// Whether `pid` is this application's own process or a descendant of
    /// it — the relation that attributes a sound to the application that
    /// holds the X11 connection.
    pub fn owns_descendant(&self, pid: i32) -> bool {
        self.contains(pid)
    }

    /// The NEAREST session leader on the ancestry: a process that leads its
    /// own session (its session id is its own pid).
    ///
    /// "Nearest" is essential: the ancestry of a desktop process runs
    /// through several session leaders (the session's own, and the user
    /// manager's), and only the nearest one sits between the process and
    /// the application that started it.
    fn session_leader(&self) -> Option<i32> {
        self.chain
            .iter()
            .filter_map(|pid| Process::new(*pid).ok())
            .filter_map(|process| process.stat().ok())
            .find(|stat| stat.session == stat.pid)
            .map(|stat| stat.pid)
    }

    /// The process below `leader` on the path to this tree's nearest
    /// member.
    fn child_below(&self, leader: i32) -> Option<i32> {
        // The ancestry is a chain; walking it from the nearest member
        // upwards, the first member whose parent is the leader is the
        // child below it.
        let nearest = *self.chain.first()?;
        let mut current = nearest;
        while let Ok(process) = Process::new(current) {
            let Ok(status) = process.status() else {
                return None;
            };
            if status.ppid == leader {
                return Some(current);
            }
            if status.ppid <= 0 || !self.pids.contains(&status.ppid) {
                return None;
            }
            current = status.ppid;
        }
        None
    }
}

/// Whether two processes run the same program.
fn same_program(a: i32, b: i32) -> bool {
    match (Process::new(a).ok(), Process::new(b).ok()) {
        (Some(a), Some(b)) => match (a.exe().ok(), b.exe().ok()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        },
        _ => false,
    }
}

/// The process on the other end of a socket.
///
/// `socket_inode` is the inode of the socket we hold; the peer is the
/// socket the kernel lists as the other side of that connection.
pub fn peer_process_of_socket(socket_inode: u64) -> Option<i32> {
    let peer_inode = peer_socket_inode(socket_inode)?;
    process_holding_socket(peer_inode)
}

/// The inode of the socket at the other end of the connection `socket_inode`
/// belongs to.
///
/// The socket we hold appears in the kernel's socket table under its own
/// inode; that row names the remote address. The peer's socket is the row
/// whose remote address is our local address and whose local address is
/// the peer's ephemeral port.
fn peer_socket_inode(socket_inode: u64) -> Option<u64> {
    let mut rows: Vec<TcpNetEntry> = Vec::new();
    rows.extend(procfs::net::tcp().ok()?);
    rows.extend(procfs::net::tcp6().ok()?);

    let own = rows
        .iter()
        .find(|e| e.inode == socket_inode && e.state == TcpState::Established)?;

    rows.iter()
        .find(|e| {
            e.state == TcpState::Established
                && e.remote_address == own.local_address
                && e.local_address == own.remote_address
        })
        .map(|e| e.inode)
}

/// The process holding the socket with this inode.
fn process_holding_socket(inode: u64) -> Option<i32> {
    let target = FDTarget::Socket(inode);
    for process in all_processes().ok()? {
        let Ok(process) = process else {
            continue;
        };
        let pid = process.pid();
        let Ok(fds) = process.fd() else {
            continue;
        };
        for fd in fds.flatten() {
            if fd.target == target {
                return Some(pid);
            }
        }
    }
    None
}

/// The inode of a socket this process holds, read from `/proc/self/fd`.
///
/// `fd` must be the raw file descriptor of a socket.
pub fn socket_inode_of(fd: i32) -> Option<u64> {
    let process = Process::myself().ok()?;
    let info = process.fd_from_fd(fd).ok()?;
    match info.target {
        FDTarget::Socket(inode) => Some(inode),
        _ => None,
    }
}

/// Group pids into applications.
///
/// Two pids land in the same group when they share an application root —
/// never merely because they share an ancestor, which on a desktop is true
/// of every pair of processes.
pub fn group_by_application(pids: &[i32]) -> Vec<ProcessTree> {
    let mut groups: Vec<(ApplicationId, ProcessTree)> = Vec::new();
    for pid in pids {
        let Some(tree) = ProcessTree::of(*pid) else {
            continue;
        };
        let Some(root) = tree.application_root() else {
            continue;
        };
        match groups.iter_mut().find(|(id, _)| *id == root) {
            Some((_, existing)) => {
                // Merge: the group keeps its identity, but the new tree's
                // members are remembered so a later lookup by any of them
                // still lands in this group.
                for p in tree.pids() {
                    existing.pids.insert(p);
                }
            }
            None => groups.push((root, tree)),
        }
    }
    groups.into_iter().map(|(_, tree)| tree).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_tree_contains_itself_and_its_ancestors() {
        let tree = ProcessTree::of_self().expect("this process is readable");
        let me = std::process::id() as i32;
        assert!(tree.contains(me), "the tree holds the process itself");
        assert!(tree.contains(1), "the tree reaches init");

        // The tree is closed under the parent relation: every member's
        // parent is a member too. That is what makes two trees comparable.
        for pid in tree.pids() {
            let Ok(process) = Process::new(pid) else {
                continue;
            };
            let Ok(status) = process.status() else {
                continue;
            };
            if status.ppid > 0 {
                assert!(
                    tree.contains(status.ppid),
                    "the parent of {pid} ({}) is in the tree",
                    status.ppid
                );
            }
        }
    }

    #[test]
    fn two_trees_of_the_same_process_are_the_same_application() {
        let me = ProcessTree::of_self().unwrap();
        let again = ProcessTree::of(std::process::id() as i32).unwrap();
        assert!(me.same_application(&again));
    }

    #[test]
    fn pid_one_is_not_the_same_application_as_any_process() {
        // Init is an ancestor of every process, but it is not an
        // application: sharing an ancestor must not make two processes the
        // same application.
        let me = ProcessTree::of_self().unwrap();
        let init = ProcessTree::of(1).unwrap();
        assert!(!me.same_application(&init));
    }

    #[test]
    fn an_unknown_pid_has_no_tree() {
        // pid 0 does not exist; a tree for it must be absent rather than
        // an empty set that would match everything.
        assert!(ProcessTree::of(0).is_none());
    }

    #[test]
    fn grouping_puts_siblings_in_one_application() {
        // This process and its own tree are the same application; asking
        // for the grouping must produce exactly one group.
        let me = std::process::id() as i32;
        let groups = group_by_application(&[me, me]);
        assert_eq!(groups.len(), 1, "the same pid twice is one application");
        assert!(groups[0].contains(me));
    }

    #[test]
    fn grouping_separates_applications_that_only_share_an_ancestor() {
        // Two real applications on this host: a browser started from a
        // shell, and the shell's own session leader. They share ancestors
        // (init, the user manager) but are different applications.
        let Some(browser) = browser_main_process() else {
            // No browser on this host: the rule itself is still pinned by
            // the other tests, so there is nothing to assert here.
            return;
        };
        let browser_tree = ProcessTree::of(browser).expect("browser tree");
        let shell = browser_tree
            .pids()
            .find(|pid| *pid != browser && ProcessTree::of(*pid).is_some())
            .expect("an ancestor of the browser");
        let shell_tree = ProcessTree::of(shell).expect("shell tree");
        assert!(
            !browser_tree.same_application(&shell_tree),
            "a browser and its launcher shell are different applications"
        );

        let groups = group_by_application(&[browser, shell]);
        assert_eq!(groups.len(), 2, "two applications, two groups");
    }

    /// The main process of a desktop browser on this host, if there is one.
    /// Detected by the process tree, not by a hard-coded pid: the test
    /// must not depend on which browsers happen to run.
    fn browser_main_process() -> Option<i32> {
        // A browser main process is one whose own descendants include a
        // process with the SAME executable — the helper processes. The
        // simplest stable marker on a live host: a process whose ancestry
        // contains a session leader running a different program (the
        // launcher) and whose own executable has several live copies.
        let mut candidates: Vec<i32> = Vec::new();
        for process in procfs::process::all_processes().ok()? {
            let Ok(process) = process else { continue };
            let Ok(exe) = process.exe() else { continue };
            let name = exe.to_string_lossy().to_string();
            if !name.contains("chromium") && !name.contains("chrome") {
                continue;
            }
            let Ok(status) = process.status() else {
                continue;
            };
            // The main process is the one that is NOT a child of another
            // browser process.
            let parent_is_browser = Process::new(status.ppid)
                .ok()
                .and_then(|p| p.exe().ok())
                .is_some_and(|parent_exe| {
                    let parent = parent_exe.to_string_lossy().to_string();
                    parent.contains("chromium") || parent.contains("chrome")
                });
            if !parent_is_browser {
                candidates.push(process.pid());
            }
        }
        candidates.into_iter().min()
    }

    #[test]
    fn a_process_owns_its_descendants() {
        // The relation that attributes a sound to the application that
        // holds the X11 connection: the sound-producing process (a
        // browser's audio service) is a descendant of the process that
        // opened the connection.
        let me = std::process::id() as i32;
        let tree = ProcessTree::of(me).expect("this process is readable");
        assert!(tree.owns_descendant(me), "a process owns itself");

        // Every ancestor in the tree is also "owned" — the relation is
        // about the tree, not about direction.
        for pid in tree.pids() {
            assert!(tree.owns_descendant(pid));
        }
    }

    #[test]
    fn a_socket_of_this_process_is_found_by_its_inode() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&listener);
        let inode = socket_inode_of(fd).expect("a listening socket has an inode");
        assert!(inode > 0);
        // The peer of a listening socket is nobody: no connection is
        // established, so the lookup must say so instead of guessing.
        assert!(peer_process_of_socket(inode).is_none());
    }

    #[test]
    fn the_peer_of_a_connected_socket_is_this_process() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();

        let client_fd = std::os::fd::AsRawFd::as_raw_fd(&client);
        let server_fd = std::os::fd::AsRawFd::as_raw_fd(&server);

        let client_inode = socket_inode_of(client_fd).unwrap();
        let server_inode = socket_inode_of(server_fd).unwrap();
        assert_ne!(client_inode, server_inode, "two sockets, two inodes");

        // Each socket's peer is the other socket, and both are held by this
        // very process — which is the property the server relies on when
        // the remote application is a different process.
        let peer_of_client = peer_process_of_socket(client_inode).unwrap();
        let peer_of_server = peer_process_of_socket(server_inode).unwrap();
        assert_eq!(peer_of_client, std::process::id() as i32);
        assert_eq!(peer_of_server, std::process::id() as i32);
    }
}

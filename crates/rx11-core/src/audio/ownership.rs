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
//!   service is a child of the browser's main process, so the two share
//!   ancestors. Two pids belong to the same application when their
//!   ancestries meet.

use std::collections::BTreeSet;

use procfs::net::{TcpNetEntry, TcpState};
use procfs::process::FDTarget;
use procfs::process::{all_processes, Process};

/// The ancestry of one process: itself plus every ancestor up to init.
///
/// The set is the application's identity: two connections belong to the
/// same application when their trees share at least one process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessTree {
    pids: BTreeSet<i32>,
}

impl ProcessTree {
    /// Build the tree of `pid`. `None` when the process cannot be read.
    pub fn of(pid: i32) -> Option<Self> {
        let mut pids = BTreeSet::new();
        let mut current = pid;
        // The chain terminates: pid 1's parent is 0, and a process that
        // vanished mid-walk ends the walk. The bound keeps a pathological
        // /proc from spinning forever.
        for _ in 0..64 {
            if current <= 0 || !pids.insert(current) {
                break;
            }
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
            Some(Self { pids })
        }
    }

    /// The tree of the current process (used by tests and diagnostics).
    pub fn of_self() -> Option<Self> {
        Self::of(std::process::id() as i32)
    }

    /// Whether two processes belong to the same application — their
    /// ancestries share at least one process.
    pub fn same_application(&self, other: &ProcessTree) -> bool {
        self.pids.intersection(&other.pids).next().is_some()
    }

    /// The processes of this tree, ascending.
    pub fn pids(&self) -> impl Iterator<Item = i32> + '_ {
        self.pids.iter().copied()
    }

    /// Whether this tree contains `pid`.
    pub fn contains(&self, pid: i32) -> bool {
        self.pids.contains(&pid)
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

/// Group pids into applications: each group is the set of pids sharing a
/// process tree.
pub fn group_by_application(pids: &[i32]) -> Vec<ProcessTree> {
    let mut trees: Vec<ProcessTree> = Vec::new();
    for pid in pids {
        let Some(tree) = ProcessTree::of(*pid) else {
            continue;
        };
        match trees.iter_mut().find(|t| t.same_application(&tree)) {
            Some(existing) => {
                // Merge: the group keeps its identity, but the new tree's
                // members are remembered so a later lookup by any of them
                // still lands in this group.
                for p in tree.pids() {
                    existing.pids.insert(p);
                }
            }
            None => trees.push(tree),
        }
    }
    trees
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
    fn pid_one_is_the_same_application_as_any_process() {
        let me = ProcessTree::of_self().unwrap();
        let init = ProcessTree::of(1).unwrap();
        assert!(me.same_application(&init));
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

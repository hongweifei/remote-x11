//! Which application an X11 connection belongs to.
//!
//! The per-application audio tier needs to answer two questions:
//!
//! 1. Which X11 connections belong to the SAME application? An
//!    application may open several X11 connections (a browser's renderer
//!    processes each hold one), and their sound is captured once and
//!    shipped on one audio channel per connection.
//! 2. Which application does a sound belong to? A sound comes from a
//!    process; the process is the application itself or one of its
//!    descendants (a browser's audio service is a child of the browser's
//!    main process).
//!
//! The attribution is by PROCESS LINEAGE, not by a shared ancestor. A
//! shared-ancestor rule over-merges on a real host: every process on a
//! desktop shares the init process, and every process started from the
//! same terminal shares the shell — so "the ancestries meet" would put
//! the browser, the terminal and the shell into ONE application, and the
//! browser's sound would ship on the terminal's channel.
//!
//! The rule used here is lineage:
//!
//! * Two connections are the same application when one connection's peer
//!   process is an ancestor of the other's.
//! * A sound belongs to the connection whose peer process is the
//!   sound's process or one of its ancestors; when several connections
//!   match, the DEEPEST (nearest) one wins — that is the connection the
//!   sound was produced for.

use std::collections::HashMap;

use rx11_core::audio::ProcessTree;
use rx11_core::types::ConnectionId;

/// One application's connections.
#[derive(Debug, Clone)]
pub struct Application {
    /// The application's identity: the peer process of its first
    /// connection, and the process tree that ties the rest of its
    /// connections to it.
    tree: ProcessTree,
    /// The X11 connections of this application, in the order they
    /// appeared.
    connections: Vec<ConnectionId>,
}

impl Application {
    /// The application's peer process (the process that opened its first
    /// X11 connection).
    pub fn peer_pid(&self) -> Option<i32> {
        self.tree.pids().next()
    }

    /// The X11 connections of this application.
    pub fn connections(&self) -> &[ConnectionId] {
        &self.connections
    }
}

/// The registry of X11 connections and their applications.
///
/// Every X11 connection is registered with the peer process behind it;
/// connections of the same application land in one [`Application`].
#[derive(Debug, Default)]
pub struct ApplicationRegistry {
    /// The applications, in registration order.
    applications: Vec<Application>,
    /// The application of each X11 connection.
    owner: HashMap<u32, usize>,
}

impl ApplicationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an X11 connection with the process behind it.
    ///
    /// `peer_pid` is the process on the other end of the X11 socket: the
    /// application itself, or the process that opened the connection on
    /// the application's behalf.
    ///
    /// Returns the application's index, or `None` when the process has no
    /// readable tree (the process already exited — a race with a
    /// disconnecting application).
    pub fn register(&mut self, connection_id: ConnectionId, peer_pid: i32) -> Option<usize> {
        let tree = ProcessTree::of(peer_pid)?;
        let index = match self.find_application(&tree) {
            Some(index) => index,
            None => {
                self.applications.push(Application {
                    tree,
                    connections: Vec::new(),
                });
                self.applications.len() - 1
            }
        };
        self.applications[index].connections.push(connection_id);
        self.owner.insert(connection_id.get(), index);
        Some(index)
    }

    /// The application of a connection, if it is registered.
    pub fn application_of(&self, connection_id: ConnectionId) -> Option<&Application> {
        let index = self.owner.get(&connection_id.get())?;
        self.applications.get(*index)
    }

    /// The application of a connection, by index.
    pub fn application(&self, index: usize) -> Option<&Application> {
        self.applications.get(index)
    }

    /// Forget a connection. The application survives as long as it has
    /// other connections; when its last connection goes, the application
    /// is forgotten too — its capture stream has nothing left to ship.
    pub fn unregister(&mut self, connection_id: ConnectionId) -> Option<usize> {
        let index = self.owner.remove(&connection_id.get())?;
        let application = self.applications.get_mut(index)?;
        application.connections.retain(|c| *c != connection_id);
        if application.connections.is_empty() {
            self.applications.remove(index);
            // Every later application shifted down by one; the owner map
            // must follow.
            for owned in self.owner.values_mut() {
                if *owned > index {
                    *owned -= 1;
                }
            }
        }
        Some(index)
    }

    /// The application a sound belongs to: the connection whose peer
    /// process is the sound's process or one of its ancestors. When
    /// several connections match, the DEEPEST one wins — the connection
    /// the sound was produced for, not a more distant relative.
    ///
    /// `sound_pid` is the process that produced the sound.
    pub fn application_of_sound(&self, sound_pid: i32) -> Option<usize> {
        let sound_tree = ProcessTree::of(sound_pid)?;
        let mut best: Option<(usize, i32)> = None;
        for (index, application) in self.applications.iter().enumerate() {
            let Some(peer_pid) = application.peer_pid() else {
                continue;
            };
            if !sound_tree.contains(peer_pid) {
                continue;
            }
            // The deepest match wins: the larger the peer pid, the closer
            // it sits to the sound's process in the lineage.
            let deeper = match best {
                Some((_, best_pid)) => peer_pid > best_pid,
                None => true,
            };
            if deeper {
                best = Some((index, peer_pid));
            }
        }
        best.map(|(index, _)| index)
    }

    /// Whether a connection's peer process is an ancestor of another
    /// process: the test that decides "this sound belongs to that
    /// connection".
    pub fn connection_owns_process(&self, connection_id: ConnectionId, pid: i32) -> bool {
        let Some(application) = self.application_of(connection_id) else {
            return false;
        };
        let Some(peer_pid) = application.peer_pid() else {
            return false;
        };
        ProcessTree::of(pid)
            .map(|tree| tree.contains(peer_pid))
            .unwrap_or(false)
    }

    /// The application index that a process belongs to: the application
    /// whose peer process is an ancestor of it, deepest match first.
    ///
    /// Used to decide which application's capture a sound stream feeds.
    pub fn application_of_process(&self, pid: i32) -> Option<usize> {
        self.application_of_sound(pid)
    }

    /// Find the application a process tree belongs to: the application
    /// whose peer process is an ancestor of the tree's root.
    fn find_application(&self, tree: &ProcessTree) -> Option<usize> {
        let mut best: Option<(usize, i32)> = None;
        for (index, application) in self.applications.iter().enumerate() {
            let Some(peer_pid) = application.peer_pid() else {
                continue;
            };
            if !tree.contains(peer_pid) {
                continue;
            }
            let deeper = match best {
                Some((_, best_pid)) => peer_pid > best_pid,
                None => true,
            };
            if deeper {
                best = Some((index, peer_pid));
            }
        }
        best.map(|(index, _)| index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: u32) -> ConnectionId {
        ConnectionId::new(id)
    }

    /// This process's own tree: the tests use it as a stand-in for an
    /// application's tree, because it is guaranteed to be readable.
    fn own_tree() -> ProcessTree {
        ProcessTree::of(std::process::id() as i32).expect("this process is readable")
    }

    #[test]
    fn a_connection_is_registered_under_its_application() {
        let mut registry = ApplicationRegistry::new();
        let me = std::process::id() as i32;
        let index = registry.register(conn(1), me).expect("register");
        let application = registry.application(index).expect("application");
        assert_eq!(application.connections(), &[conn(1)]);
        assert_eq!(registry.application_of(conn(1)).map(|a| a.connections()), Some(&[conn(1)][..]));
    }

    #[test]
    fn two_connections_of_the_same_process_share_one_application() {
        let mut registry = ApplicationRegistry::new();
        let me = std::process::id() as i32;
        let first = registry.register(conn(1), me).expect("register");
        let second = registry.register(conn(2), me).expect("register");
        assert_eq!(first, second, "one application for one process");
        let application = registry.application(first).expect("application");
        assert_eq!(application.connections().len(), 2);
    }

    #[test]
    fn the_last_connection_going_frees_the_application() {
        let mut registry = ApplicationRegistry::new();
        let me = std::process::id() as i32;
        let index = registry.register(conn(1), me).expect("register");
        registry.unregister(conn(1));
        assert!(
            registry.application(index).is_none(),
            "an application without connections is gone"
        );
        assert!(registry.application_of(conn(1)).is_none());
    }

    #[test]
    fn one_connection_going_keeps_the_application_for_the_others() {
        let mut registry = ApplicationRegistry::new();
        let me = std::process::id() as i32;
        let index = registry.register(conn(1), me).expect("register");
        registry.register(conn(2), me).expect("register");
        registry.unregister(conn(1));
        let application = registry.application(index).expect("application survives");
        assert_eq!(application.connections(), &[conn(2)]);
    }

    #[test]
    fn a_sound_from_this_process_belongs_to_this_processs_connection() {
        let mut registry = ApplicationRegistry::new();
        let me = std::process::id() as i32;
        let index = registry.register(conn(1), me).expect("register");
        assert_eq!(
            registry.application_of_sound(me),
            Some(index),
            "the sound of the application belongs to it"
        );
        assert!(registry.connection_owns_process(conn(1), me));
    }

    #[test]
    fn an_unknown_process_has_no_application() {
        let registry = ApplicationRegistry::new();
        // pid 0 does not exist: the lookup must say so rather than guess.
        assert!(registry.application_of_sound(0).is_none());
    }

    #[test]
    fn the_tree_of_a_process_is_reachable_and_contains_itself() {
        let tree = own_tree();
        assert!(tree.contains(std::process::id() as i32));
    }
}

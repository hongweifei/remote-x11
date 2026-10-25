//! Which application an X11 connection belongs to.
//!
//! The per-application audio tier needs to answer two questions:
//!
//! 1. Which X11 connections belong to the SAME application? An
//!    application may open several X11 connections (a browser's renderer
//!    processes each hold one), and their sound is captured once and
//!    shipped on one audio channel per connection.
//! 2. Which application does a sound belong to? A sound comes from a
//!    process; that process is the application's own process or one of its
//!    descendants (a browser's audio service is a child of the browser's
//!    main process).
//!
//! The attribution rule lives in the core crate, so the client and the
//! server measure "same application" with the same ruler: the APPLICATION
//! ROOT. Sharing an ancestor is not enough — every process on a desktop
//! shares the init process, and every process started from the same
//! terminal shares the shell, so "the ancestries meet" would put the
//! browser, the terminal and the shell into one application.
//!
//! The registry here owns the bookkeeping: which connections are
//! registered, which application each belongs to, and which application a
//! sound-producing process belongs to.

use std::collections::HashMap;

use rx11_core::audio::{ApplicationId, ProcessTree};
use rx11_core::types::ConnectionId;

/// One application's connections.
#[derive(Debug, Clone)]
pub struct Application {
    /// The application's root: the process every member descends from.
    root: ApplicationId,
    /// The X11 connections of this application, in registration order.
    connections: Vec<ConnectionId>,
}

impl Application {
    /// The application's root process.
    pub fn root(&self) -> ApplicationId {
        self.root
    }

    /// The X11 connections of this application.
    pub fn connections(&self) -> &[ConnectionId] {
        &self.connections
    }
}

/// The registry of X11 connections and their applications.
///
/// Every X11 connection is registered with the process behind it;
/// connections of the same application land in one [`Application`].
#[derive(Debug, Default)]
pub struct ApplicationRegistry {
    /// The applications, in registration order.
    applications: Vec<Application>,
    /// The application index of each X11 connection.
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
        let root = tree.application_root()?;
        let index = match self.find_application(root) {
            Some(index) => index,
            None => {
                self.applications.push(Application {
                    root,
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

    /// The application at an index.
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

    /// The application a sound belongs to.
    ///
    /// `sound_pid` is the process that produced the sound. The test is
    /// the APPLICATION ROOT, compared for equality — not "the sound's
    /// ancestry contains the application's root". The ancestry test is
    /// too wide: it holds for EVERY application on the sound's ancestor
    /// chain, so a terminal launched before a browser would claim the
    /// browser's sound. The core's application root already places a
    /// browser's audio service under the browser's own root, so the
    /// sound's root equals the application's root exactly.
    ///
    /// When several connections of the SAME application match, they all
    /// belong to it — the sound is that application's sound, and every one
    /// of its X11 connections must receive it.
    ///
    /// Returns the application index, or `None` when the sound belongs to
    /// no registered application.
    pub fn application_of_sound(&self, sound_pid: i32) -> Option<usize> {
        let sound_root = ProcessTree::of(sound_pid)?.application_root()?;
        self.applications
            .iter()
            .position(|application| application.root() == sound_root)
    }

    /// Whether a connection's application owns a process: the test that
    /// decides "this sound belongs to that connection".
    pub fn connection_owns_process(&self, connection_id: ConnectionId, pid: i32) -> bool {
        let Some(application) = self.application_of(connection_id) else {
            return false;
        };
        ProcessTree::of(pid)
            .and_then(|tree| tree.application_root())
            .is_some_and(|root| root == application.root())
    }

    /// The application index a process belongs to.
    pub fn application_of_process(&self, pid: i32) -> Option<usize> {
        self.application_of_sound(pid)
    }

    /// Find the application with this root.
    fn find_application(&self, root: ApplicationId) -> Option<usize> {
        self.applications
            .iter()
            .position(|application| application.root() == root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: u32) -> ConnectionId {
        ConnectionId::new(id)
    }

    #[test]
    fn a_connection_is_registered_under_its_application() {
        let mut registry = ApplicationRegistry::new();
        let me = std::process::id() as i32;
        let index = registry.register(conn(1), me).expect("register");
        let application = registry.application(index).expect("application");
        assert_eq!(application.connections(), &[conn(1)]);
        assert_eq!(
            registry.application_of(conn(1)).map(|a| a.connections()),
            Some(&[conn(1)][..])
        );
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
    fn a_sound_from_this_process_belongs_to_this_processs_application() {
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
    fn a_sound_from_another_application_is_not_attributed_here() {
        let mut registry = ApplicationRegistry::new();
        let me = std::process::id() as i32;
        registry.register(conn(1), me).expect("register");
        // pid 1 is init: its application root is init itself, which is
        // not this process's application. The sound must not be claimed
        // by this connection — otherwise a terminal registered first would
        // take the browser's sound.
        assert!(
            registry.application_of_sound(1).is_none(),
            "another application's sound belongs to another application"
        );
        // The connection still owns its own process.
        assert!(registry.connection_owns_process(conn(1), me));
    }
}

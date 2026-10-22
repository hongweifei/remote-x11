//! Unit tests for the overview's view model: rates and the running count.
//!
//! Split out of `console/mod.rs` so the page file stays about layout and
//! painting, the same split `app/` used for its tests.

use super::*;
use crate::model::Role;

fn sample(id: u64, role: Role) -> SessionRow {
    SessionRow {
        id,
        role,
        health: Health::Running,
        headline: "x".into(),
        detail: String::new(),
        sent: Some(1),
        received: Some(2),
        uptime: Some(3),
    }
}

#[test]
fn rates_come_from_the_newest_sample() {
    assert_eq!(ConsoleData::default().rates(), (0.0, 0.0));
    let d = ConsoleData {
        scope: vec![
            Sample {
                sent_bps: 1.0,
                recv_bps: 2.0,
            },
            Sample {
                sent_bps: 30.0,
                recv_bps: 40.0,
            },
        ],
        ..ConsoleData::default()
    };
    assert_eq!(d.rates(), (30.0, 40.0));
}

#[test]
fn running_counts_all_live_phases() {
    let mut d = ConsoleData {
        sessions: vec![
            sample(1, Role::Server),
            sample(2, Role::Client),
            sample(3, Role::Ssh),
        ],
        ..ConsoleData::default()
    };
    d.sessions[2].health = Health::Reconnecting;
    assert_eq!(d.running(), 3);
    d.sessions[0].health = Health::Stopped;
    assert_eq!(d.running(), 2);
}

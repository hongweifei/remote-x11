//! Composition-root unit tests: the traffic ratio and the role → key/page
//! routing tables the shell and the form pages depend on.

use super::*;

#[test]
fn savings_ratio_handles_empty_traffic() {
    let t = TrafficTotals::default();
    assert_eq!(t.savings_ratio(), 0.0);
    let t = TrafficTotals {
        saved: 30,
        raw: 70,
        ..Default::default()
    };
    assert!((t.savings_ratio() - 0.3).abs() < 1e-9);
}

#[test]
fn role_key_sets_are_disjoint() {
    let s: Vec<&str> = keys_for_role(Role::Server).to_vec();
    let c: Vec<&str> = keys_for_role(Role::Client).to_vec();
    let h: Vec<&str> = keys_for_role(Role::Ssh).to_vec();
    for (a, b) in [(&s, &c), (&s, &h), (&c, &h)] {
        for k in a {
            assert!(!b.contains(k), "key {k} belongs to two roles");
        }
    }
}

#[test]
fn role_pages_are_the_expected_ones() {
    assert_eq!(page_for_role(Role::Server), Page::Server);
    assert_eq!(page_for_role(Role::Client), Page::Client);
    assert_eq!(page_for_role(Role::Ssh), Page::Ssh);
}

//! Mass-outage detection. Heartbeat checks every app from one machine, so when its own
//! network fails, every app looks down at once and each one would page someone. When at
//! least `MIN_DOWN` apps *and* `pct`% of the monitored ones are down, the gate sends one
//! "mass outage" notice instead and holds the individual down alerts. When the outage
//! ends, apps that are still down get their normal alert; the ones that came back never
//! alert at all.

use std::collections::HashSet;

use crate::alerts::{MassOutage, StatusChange};
use crate::uptime::Status;

/// Fewest apps down at once that can count as a mass outage: with two apps, both failing
/// together is too likely to be a real, shared problem to hold back.
pub const MIN_DOWN: usize = 3;

/// What to do with one round's alerts.
#[derive(Debug, Default)]
pub struct Review {
    pub outage: Option<MassOutage>,
    pub deliver: Vec<StatusChange>,
    /// Apps whose down alert was held; on `MassOutage::Ended` the caller alerts the ones
    /// that are still down.
    pub release: Vec<String>,
}

#[derive(Debug, Default)]
pub struct OutageGate {
    active: bool,
    held: HashSet<String>,
}

impl OutageGate {
    /// `down` of `total` monitored apps are down after this round; `pct` is
    /// `UPTIME_MASS_DOWN_PCT` (0 = never).
    #[must_use]
    pub fn is_mass(down: usize, total: usize, pct: u8) -> bool {
        pct > 0 && down >= MIN_DOWN && down * 100 >= usize::from(pct) * total
    }

    pub fn review(
        &mut self,
        changes: Vec<StatusChange>,
        down: usize,
        total: usize,
        pct: u8,
    ) -> Review {
        let mass = Self::is_mass(down, total, pct);
        let mut review = Review::default();
        match (self.active, mass) {
            (false, false) => review.deliver = changes,
            (_, true) => {
                if !self.active {
                    self.active = true;
                    review.outage = Some(MassOutage::Started { down, total });
                }
                for change in changes {
                    if change.is_reminder() {
                        continue;
                    }
                    if change.to == Status::Down {
                        self.held.insert(change.slug);
                    } else if !self.held.remove(&change.slug) {
                        review.deliver.push(change);
                    }
                }
            }
            (true, false) => {
                self.active = false;
                review.outage = Some(MassOutage::Ended { total });
                review.deliver = changes
                    .into_iter()
                    .filter(|c| !self.held.contains(&c.slug))
                    .collect();
                review.release = self.held.drain().collect();
            }
        }
        review
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::AppRoute;
    use crate::uptime::Heartbeat;

    fn change(slug: &str, from: Status, to: Status) -> StatusChange {
        StatusChange {
            slug: slug.into(),
            name: slug.into(),
            from,
            to,
            beat: Heartbeat {
                at: 0,
                status: to,
                latency_ms: None,
                message: String::new(),
            },
            down_since: None,
            route: AppRoute::default(),
        }
    }

    fn slugs(changes: &[StatusChange]) -> Vec<&str> {
        changes.iter().map(|c| c.slug.as_str()).collect()
    }

    #[test]
    fn mass_needs_both_the_minimum_and_the_share() {
        assert!(
            !OutageGate::is_mass(2, 2, 50),
            "two apps are never a mass outage"
        );
        assert!(OutageGate::is_mass(3, 6, 50));
        assert!(!OutageGate::is_mass(3, 7, 50));
        assert!(!OutageGate::is_mass(10, 10, 0), "0 turns it off");
    }

    #[test]
    fn a_normal_round_passes_everything_through() {
        let mut gate = OutageGate::default();
        let review = gate.review(vec![change("a", Status::Up, Status::Down)], 1, 10, 50);
        assert_eq!(slugs(&review.deliver), ["a"]);
        assert!(review.outage.is_none());
    }

    #[test]
    fn a_mass_outage_holds_downs_and_releases_only_the_apps_still_down() {
        let mut gate = OutageGate::default();
        let downs = ["a", "b", "c"].map(|s| change(s, Status::Up, Status::Down));
        let review = gate.review(downs.to_vec(), 3, 4, 50);
        assert_eq!(
            review.outage,
            Some(MassOutage::Started { down: 3, total: 4 })
        );
        assert!(review.deliver.is_empty(), "individual downs are held");

        // During the outage: a held app recovering stays quiet, reminders are dropped.
        let review = gate.review(
            vec![
                change("a", Status::Down, Status::Up),
                change("b", Status::Down, Status::Down),
            ],
            3,
            4,
            50,
        );
        assert!(review.deliver.is_empty() && review.outage.is_none());

        // Over: the rest come back, "a" came back earlier; "b" and "c" are released.
        let review = gate.review(vec![change("c", Status::Down, Status::Up)], 1, 4, 50);
        assert_eq!(review.outage, Some(MassOutage::Ended { total: 4 }));
        assert!(
            review.deliver.is_empty(),
            "c's recovery was never announced"
        );
        let mut released = review.release;
        released.sort();
        assert_eq!(released, ["b", "c"]);
    }
}

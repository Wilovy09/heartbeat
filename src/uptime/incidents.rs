//! Incidents: runs of consecutive down checks, derived from the heartbeat history (so they
//! cover the retention window and survive restarts without a store of their own).

use serde::Serialize;

use crate::uptime::{Heartbeat, Status};

/// One outage: from the first down check to the first check that wasn't down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Incident {
    pub started_at: u64,
    /// `None` while it's still going on.
    pub ended_at: Option<u64>,
    /// Until `ended_at`, or until `now` for an ongoing one.
    pub duration_secs: u64,
    /// The first failed check's message: usually the actual cause.
    pub cause: String,
    pub failed_checks: u32,
}

/// Incident statistics for one window.
#[derive(Debug, Clone, Default, Serialize)]
pub struct IncidentReport {
    /// Newest first.
    pub incidents: Vec<Incident>,
    /// Mean time to recovery over the ended incidents.
    pub mttr_secs: Option<u64>,
    pub total_down_secs: u64,
}

impl IncidentReport {
    /// Incidents among `beats` (oldest first) that were still going on at or after
    /// `cutoff`; the newest `limit` are kept, the statistics cover all of them.
    pub fn build<'a>(
        beats: impl Iterator<Item = &'a Heartbeat>,
        cutoff: u64,
        now: u64,
        limit: usize,
    ) -> Self {
        let mut incidents: Vec<Incident> = Vec::new();
        let mut current: Option<Incident> = None;
        for beat in beats {
            match (beat.status, current.as_mut()) {
                (Status::Down, Some(open)) => open.failed_checks += 1,
                (Status::Down, None) => {
                    current = Some(Incident {
                        started_at: beat.at,
                        ended_at: None,
                        duration_secs: 0,
                        cause: beat.message.clone(),
                        failed_checks: 1,
                    });
                }
                (Status::Up | Status::Degraded, Some(_)) => {
                    if let Some(mut done) = current.take() {
                        done.ended_at = Some(beat.at);
                        done.duration_secs = beat.at.saturating_sub(done.started_at);
                        incidents.push(done);
                    }
                }
                (Status::Up | Status::Degraded, None) => {}
            }
        }
        if let Some(mut open) = current {
            open.duration_secs = now.saturating_sub(open.started_at);
            incidents.push(open);
        }
        incidents.retain(|i| i.ended_at.is_none_or(|end| end >= cutoff));

        let ended: Vec<u64> = incidents
            .iter()
            .filter(|i| i.ended_at.is_some())
            .map(|i| i.duration_secs)
            .collect();
        let mttr_secs = (!ended.is_empty()).then(|| ended.iter().sum::<u64>() / ended.len() as u64);
        let total_down_secs = incidents.iter().map(|i| i.duration_secs).sum();
        incidents.reverse();
        incidents.truncate(limit);
        Self {
            incidents,
            mttr_secs,
            total_down_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(at: u64, status: Status, message: &str) -> Heartbeat {
        Heartbeat {
            at,
            status,
            latency_ms: None,
            message: message.into(),
        }
    }

    #[test]
    fn down_runs_become_incidents_with_their_first_cause() {
        let beats = [
            beat(0, Status::Up, "ok"),
            beat(60, Status::Down, "HTTP 502"),
            beat(120, Status::Down, "timeout"),
            beat(180, Status::Degraded, "slow"),
            beat(240, Status::Down, "HTTP 500"),
        ];
        let report = IncidentReport::build(beats.iter(), 0, 300, 10);
        assert_eq!(
            report.incidents,
            [
                Incident {
                    started_at: 240,
                    ended_at: None,
                    duration_secs: 60,
                    cause: "HTTP 500".into(),
                    failed_checks: 1,
                },
                Incident {
                    started_at: 60,
                    ended_at: Some(180),
                    duration_secs: 120,
                    cause: "HTTP 502".into(),
                    failed_checks: 2,
                },
            ]
        );
        assert_eq!(report.mttr_secs, Some(120), "only ended incidents count");
        assert_eq!(report.total_down_secs, 180);
    }

    #[test]
    fn incidents_that_ended_before_the_window_are_left_out() {
        let beats = [
            beat(0, Status::Down, "x"),
            beat(60, Status::Up, "ok"),
            beat(500, Status::Down, "y"),
            beat(560, Status::Up, "ok"),
        ];
        let report = IncidentReport::build(beats.iter(), 100, 600, 10);
        assert_eq!(report.incidents.len(), 1);
        assert_eq!(report.incidents[0].cause, "y");
    }
}

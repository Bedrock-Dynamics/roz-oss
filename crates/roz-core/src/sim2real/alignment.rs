use serde::{Deserialize, Serialize};

/// An anchor that maps a simulation timestamp to a real-world timestamp
/// via a shared named event observed in both timelines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeAnchor {
    pub sim_time_secs: f64,
    pub real_time_secs: f64,
    pub event_name: String,
}

/// Match events by name between sim and real timelines.
///
/// For each event name found in both lists, create an anchor pairing
/// the first occurrence in sim with the first occurrence in real.
/// Anchors are returned sorted by `sim_time_secs`.
pub fn event_align(sim_events: &[(f64, &str)], real_events: &[(f64, &str)]) -> Vec<TimeAnchor> {
    let mut anchors = Vec::new();

    for &(sim_t, sim_name) in sim_events {
        if anchors.iter().any(|a: &TimeAnchor| a.event_name == sim_name) {
            continue;
        }
        if let Some(&(real_t, _)) = real_events.iter().find(|&&(_, rn)| rn == sim_name) {
            anchors.push(TimeAnchor {
                sim_time_secs: sim_t,
                real_time_secs: real_t,
                event_name: sim_name.to_string(),
            });
        }
    }

    anchors.sort_by(|a, b| {
        a.sim_time_secs
            .partial_cmp(&b.sim_time_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    anchors
}

/// Linearly interpolate a sim time to a real time using the given anchors.
///
/// Returns `None` if there are no anchors or if `sim_time` falls outside
/// the range covered by the anchors.
pub fn interpolate_at(anchors: &[TimeAnchor], sim_time: f64) -> Option<f64> {
    if anchors.is_empty() {
        return None;
    }

    if anchors.len() == 1 {
        let a = &anchors[0];
        #[expect(clippy::float_cmp, reason = "exact anchor match is intentional")]
        if sim_time == a.sim_time_secs {
            return Some(a.real_time_secs);
        }
        return None;
    }

    let first = &anchors[0];
    let last = &anchors[anchors.len() - 1];

    if sim_time < first.sim_time_secs || sim_time > last.sim_time_secs {
        return None;
    }

    // Find the bounding anchors
    for window in anchors.windows(2) {
        let lo = &window[0];
        let hi = &window[1];
        if sim_time >= lo.sim_time_secs && sim_time <= hi.sim_time_secs {
            let range = hi.sim_time_secs - lo.sim_time_secs;
            if range == 0.0 {
                return Some(lo.real_time_secs);
            }
            let t = (sim_time - lo.sim_time_secs) / range;
            return Some(t.mul_add(hi.real_time_secs - lo.real_time_secs, lo.real_time_secs));
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_events_produce_anchors() {
        let sim = vec![(1.0, "start"), (3.0, "grasp"), (5.0, "release")];
        let real = vec![(1.1, "start"), (3.5, "grasp"), (5.2, "release")];
        let anchors = event_align(&sim, &real);
        assert_eq!(anchors.len(), 3);
        assert_eq!(anchors[0].event_name, "start");
        assert!((anchors[0].real_time_secs - 1.1).abs() < f64::EPSILON);
    }

    #[test]
    fn interpolation_between_anchors() {
        let anchors = vec![
            TimeAnchor {
                sim_time_secs: 0.0,
                real_time_secs: 0.0,
                event_name: "a".into(),
            },
            TimeAnchor {
                sim_time_secs: 10.0,
                real_time_secs: 20.0,
                event_name: "b".into(),
            },
        ];
        let result = interpolate_at(&anchors, 5.0).unwrap();
        assert!((result - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn interpolation_at_anchor_point() {
        let anchors = vec![
            TimeAnchor {
                sim_time_secs: 0.0,
                real_time_secs: 0.0,
                event_name: "a".into(),
            },
            TimeAnchor {
                sim_time_secs: 10.0,
                real_time_secs: 20.0,
                event_name: "b".into(),
            },
        ];
        let result = interpolate_at(&anchors, 0.0).unwrap();
        assert!((result - 0.0).abs() < f64::EPSILON);
        let result = interpolate_at(&anchors, 10.0).unwrap();
        assert!((result - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_events_produce_no_anchors() {
        let anchors = event_align(&[], &[]);
        assert!(anchors.is_empty());
    }

    #[test]
    fn no_matching_events() {
        let sim = vec![(1.0, "start")];
        let real = vec![(1.0, "stop")];
        let anchors = event_align(&sim, &real);
        assert!(anchors.is_empty());
    }

    #[test]
    fn single_anchor_interpolation_exact() {
        let anchors = vec![TimeAnchor {
            sim_time_secs: 5.0,
            real_time_secs: 7.0,
            event_name: "only".into(),
        }];
        assert_eq!(interpolate_at(&anchors, 5.0), Some(7.0));
    }

    #[test]
    fn single_anchor_interpolation_outside() {
        let anchors = vec![TimeAnchor {
            sim_time_secs: 5.0,
            real_time_secs: 7.0,
            event_name: "only".into(),
        }];
        assert_eq!(interpolate_at(&anchors, 6.0), None);
    }

    #[test]
    fn interpolation_outside_range_returns_none() {
        let anchors = vec![
            TimeAnchor {
                sim_time_secs: 1.0,
                real_time_secs: 2.0,
                event_name: "a".into(),
            },
            TimeAnchor {
                sim_time_secs: 3.0,
                real_time_secs: 6.0,
                event_name: "b".into(),
            },
        ];
        assert_eq!(interpolate_at(&anchors, 0.5), None);
        assert_eq!(interpolate_at(&anchors, 3.5), None);
    }
}

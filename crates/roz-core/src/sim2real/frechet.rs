use nalgebra::Point3;

/// Compute the discrete Frechet distance between two 3-D trajectories.
///
/// Returns `None` if either input is empty.
pub fn discrete_frechet(curve_p: &[Point3<f64>], curve_q: &[Point3<f64>]) -> Option<f64> {
    let rows = curve_p.len();
    let cols = curve_q.len();

    if rows == 0 || cols == 0 {
        return None;
    }

    // ca[i][j] holds the coupling distance at (i, j)
    let mut ca = vec![vec![-1.0_f64; cols]; rows];

    for i in 0..rows {
        for j in 0..cols {
            let dist = nalgebra::distance(&curve_p[i], &curve_q[j]);
            match (i, j) {
                (0, 0) => ca[0][0] = dist,
                (0, _) => ca[0][j] = f64::max(ca[0][j - 1], dist),
                (_, 0) => ca[i][0] = f64::max(ca[i - 1][0], dist),
                _ => {
                    let prev_min = ca[i - 1][j].min(ca[i][j - 1]).min(ca[i - 1][j - 1]);
                    ca[i][j] = f64::max(prev_min, dist);
                }
            }
        }
    }

    Some(ca[rows - 1][cols - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_trajectories_zero_distance() {
        let traj = vec![
            Point3::new(0.0, 0.0, 0.0),
            Point3::new(1.0, 0.0, 0.0),
            Point3::new(2.0, 0.0, 0.0),
        ];
        let d = discrete_frechet(&traj, &traj).unwrap();
        assert!(d.abs() < f64::EPSILON);
    }

    #[test]
    fn shifted_trajectories() {
        let p = vec![Point3::new(0.0, 0.0, 0.0), Point3::new(1.0, 0.0, 0.0)];
        let q = vec![Point3::new(0.0, 1.0, 0.0), Point3::new(1.0, 1.0, 0.0)];
        let d = discrete_frechet(&p, &q).unwrap();
        assert!((d - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn different_lengths() {
        let p = vec![Point3::new(0.0, 0.0, 0.0), Point3::new(1.0, 0.0, 0.0)];
        let q = vec![
            Point3::new(0.0, 0.0, 0.0),
            Point3::new(0.5, 0.0, 0.0),
            Point3::new(1.0, 0.0, 0.0),
        ];
        let d = discrete_frechet(&p, &q).unwrap();
        // Discrete Frechet distance is 0.5 here: optimal coupling maps
        // p[0]->q[0], p[1]->q[1..2], with max leash length 0.5.
        assert!((d - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn single_point_each() {
        let p = vec![Point3::new(1.0, 2.0, 3.0)];
        let q = vec![Point3::new(4.0, 6.0, 3.0)];
        let expected = nalgebra::distance(&p[0], &q[0]);
        let d = discrete_frechet(&p, &q).unwrap();
        assert!((d - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn empty_input_returns_none() {
        let p = vec![Point3::new(0.0, 0.0, 0.0)];
        assert!(discrete_frechet(&p, &[]).is_none());
        assert!(discrete_frechet(&[], &p).is_none());
        let empty: &[Point3<f64>] = &[];
        assert!(discrete_frechet(empty, empty).is_none());
    }

    #[test]
    fn frechet_is_commutative() {
        let p = vec![
            Point3::new(0.0, 0.0, 0.0),
            Point3::new(1.0, 1.0, 0.0),
            Point3::new(2.0, 0.0, 0.0),
        ];
        let q = vec![Point3::new(0.0, 0.5, 0.0), Point3::new(2.0, 0.5, 0.0)];
        let d1 = discrete_frechet(&p, &q).unwrap();
        let d2 = discrete_frechet(&q, &p).unwrap();
        assert!((d1 - d2).abs() < f64::EPSILON);
    }
}

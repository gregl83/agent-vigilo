//! Deterministic counterbalanced campaign scheduling.
//!
//! Comparison blocks alternate `ABBA` and `BAAB` orientations derived from a
//! stable seed, workload ID, and tuple ID. This balances binary position and
//! pairs observations without adapting the schedule to measured results.

use anyhow::{
    Result,
    bail,
};

use super::model::{
    BinaryRole,
    Orientation,
};

/// One binary execution at a fixed position within a measurement block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledExecution {
    /// Binary assigned to the position.
    pub role: BinaryRole,
    /// Adjacent-pair index used by the block estimator.
    pub pair_id: u8,
    /// One-based execution position within the block.
    pub position: u8,
}

/// Selects the deterministic orientation for one workload block.
pub fn orientation(seed: u64, workload_id: &str, tuple_id: &str, block_id: u32) -> Orientation {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&seed.to_le_bytes());
    hasher.update(workload_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(tuple_id.as_bytes());
    let first_is_abba = hasher.finalize().as_bytes()[0] & 1 == 0;
    if first_is_abba == block_id.is_multiple_of(2) {
        Orientation::Abba
    } else {
        Orientation::Baab
    }
}

/// Expands an orientation into its four ordered binary executions.
pub fn executions(orientation: Orientation) -> [ScheduledExecution; 4] {
    let roles = match orientation {
        Orientation::Abba => [
            BinaryRole::Baseline,
            BinaryRole::Candidate,
            BinaryRole::Candidate,
            BinaryRole::Baseline,
        ],
        Orientation::Baab => [
            BinaryRole::Candidate,
            BinaryRole::Baseline,
            BinaryRole::Baseline,
            BinaryRole::Candidate,
        ],
        Orientation::Single => [BinaryRole::Single; 4],
    };
    std::array::from_fn(|index| ScheduledExecution {
        role: roles[index],
        pair_id: (index / 2) as u8,
        position: (index + 1) as u8,
    })
}

/// Orders equal one-per-binary preconditioning from the first block orientation.
pub fn preconditioning_order(first: Orientation) -> [BinaryRole; 2] {
    match first {
        Orientation::Abba => [BinaryRole::Baseline, BinaryRole::Candidate],
        Orientation::Baab => [BinaryRole::Candidate, BinaryRole::Baseline],
        Orientation::Single => [BinaryRole::Single, BinaryRole::Single],
    }
}

/// Validates that a planned schedule is positive, even, alternating, and balanced.
pub fn validate(blocks: u32, seed: u64, workload_id: &str, tuple_id: &str) -> Result<()> {
    if blocks == 0 || !blocks.is_multiple_of(2) {
        bail!("blocking schedules require a positive even block count");
    }
    let orientations: Vec<_> = (0..blocks)
        .map(|block| orientation(seed, workload_id, tuple_id, block))
        .collect();
    validate_orientations(&orientations)
}

/// Proves each adjacent block pair contains one `ABBA` and one `BAAB` schedule.
fn validate_orientations(orientations: &[Orientation]) -> Result<()> {
    if orientations.is_empty() || !orientations.len().is_multiple_of(2) {
        bail!("blocking schedules require a positive even block count");
    }
    let mut abba = 0;
    let mut baab = 0;
    let mut previous = None;
    for (block, current) in orientations.iter().copied().enumerate() {
        if previous == Some(current) {
            bail!("schedule did not alternate at block {block}");
        }
        match current {
            Orientation::Abba => abba += 1,
            Orientation::Baab => baab += 1,
            Orientation::Single => unreachable!(),
        }
        previous = Some(current);
    }
    if abba != baab {
        bail!("schedule is not orientation balanced");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn every_orientation_set_balances_positions() {
        for seed in 0..256 {
            let mut exposure = BTreeMap::new();
            for block in 0..2 {
                for execution in executions(orientation(seed, "workload", "tuple", block)) {
                    *exposure
                        .entry((execution.role, execution.position))
                        .or_insert(0) += 1;
                }
            }
            for role in [BinaryRole::Baseline, BinaryRole::Candidate] {
                for position in 1..=4 {
                    assert_eq!(exposure.get(&(role, position)), Some(&1));
                }
            }
        }
    }

    #[test]
    fn logical_index_makes_resume_deterministic() {
        let expected = orientation(42, "workload", "tuple", 6);
        assert_eq!(expected, orientation(42, "workload", "tuple", 6));
        assert_ne!(expected, orientation(42, "workload", "tuple", 7));
    }

    #[test]
    fn only_positive_even_schedules_are_valid() {
        assert!(validate(2, 1, "workload", "tuple").is_ok());
        assert!(validate(0, 1, "workload", "tuple").is_err());
        assert!(validate(3, 1, "workload", "tuple").is_err());
        assert!(validate_orientations(&[Orientation::Abba, Orientation::Abba]).is_err());
    }

    #[test]
    fn preconditioning_exposes_each_binary_once() {
        for orientation in [Orientation::Abba, Orientation::Baab] {
            let mut roles = preconditioning_order(orientation);
            roles.sort_by_key(|role| match role {
                BinaryRole::Baseline => 0,
                BinaryRole::Candidate => 1,
                BinaryRole::Single => 2,
            });
            assert_eq!(roles, [BinaryRole::Baseline, BinaryRole::Candidate]);
        }
    }
}

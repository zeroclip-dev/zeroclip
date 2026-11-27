//! Keyframe snapping.
//!
//! A stream copy can only start on a keyframe (IDR), so the in-point snaps *back*
//! to the nearest keyframe at or before it — never forward, which would cut off
//! the moment the user picked. The out-point needs no snapping: a stream copy can
//! stop after any frame.

use zc_core::Hns;

/// Where a trim starting at `requested` will really start, given the clip's
/// keyframe positions (sorted ascending). A request before the first keyframe
/// starts at the first keyframe. `None` if there are no keyframes at all.
#[must_use]
pub fn snap_in_point(keyframes: &[Hns], requested: Hns) -> Option<Hns> {
    // Index of the first keyframe after `requested`; the one before it is the snap point.
    let i = keyframes.partition_point(|&k| k <= requested);
    if i == 0 { keyframes.first().copied() } else { Some(keyframes[i - 1]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyframes() -> Vec<Hns> {
        (0..5).map(Hns::from_secs).collect()
    }

    #[test]
    fn snap_goes_back_to_the_previous_keyframe() {
        assert_eq!(snap_in_point(&keyframes(), Hns::from_millis(2_700)), Some(Hns::from_secs(2)));
    }

    #[test]
    fn snap_keeps_a_point_that_is_already_a_keyframe() {
        assert_eq!(snap_in_point(&keyframes(), Hns::from_secs(3)), Some(Hns::from_secs(3)));
    }

    #[test]
    fn snap_handles_the_edges() {
        let late_start = [Hns::from_secs(1), Hns::from_secs(2)];
        assert_eq!(snap_in_point(&late_start, Hns::ZERO), Some(Hns::from_secs(1)));
        assert_eq!(snap_in_point(&keyframes(), Hns::from_secs(99)), Some(Hns::from_secs(4)));
        assert_eq!(snap_in_point(&[], Hns::from_secs(1)), None);
    }
}

use crate::position::{Position, Transform};

/// Spatial audio processor
pub struct SpatialProcessor;

impl SpatialProcessor {
    pub fn new() -> Self {
        Self
    }

    /// Initialize the spatial processor
    pub fn init(&mut self) -> anyhow::Result<()> {
        log::info!("Spatial processor initialized (using stereo panning)");
        Ok(())
    }

    /// Process mono audio to stereo with spatial positioning
    /// azimuth: horizontal angle in radians (0 = front, positive = right)
    /// distance: distance to source (used for volume attenuation)
    pub fn process(
        &mut self,
        mono_input: &[f32],
        azimuth: f32,
        distance: f32,
        min_distance: f32,
        max_distance: f32,
    ) -> (Vec<f32>, Vec<f32>) {
        let mut left = vec![0.0f32; mono_input.len()];
        let mut right = vec![0.0f32; mono_input.len()];

        // Calculate distance attenuation
        let attenuation = crate::position::transform::distance_attenuation(
            distance,
            min_distance,
            max_distance,
        );

        // Use simple stereo panning
        self.simple_pan(mono_input, azimuth, attenuation, &mut left, &mut right);

        (left, right)
    }

    /// Simple stereo panning
    fn simple_pan(
        &self,
        mono: &[f32],
        azimuth: f32,
        attenuation: f32,
        left: &mut [f32],
        right: &mut [f32],
    ) {
        // Convert azimuth to pan position (-1 to 1)
        let pan = (azimuth / std::f32::consts::FRAC_PI_2).clamp(-1.0, 1.0);

        // Equal-power panning scaled so a centered source plays at unity gain
        // per ear (rather than cos(π/4) ≈ 0.707). Off-center sources reach up to
        // sqrt(2) in the dominant ear; the mixer's soft-clipper handles overshoot.
        const CENTER_COMPENSATION: f32 = std::f32::consts::SQRT_2;
        let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        let left_gain = angle.cos() * attenuation * CENTER_COMPENSATION;
        let right_gain = angle.sin() * attenuation * CENTER_COMPENSATION;

        for (i, &sample) in mono.iter().enumerate() {
            left[i] = sample * left_gain;
            right[i] = sample * right_gain;
        }
    }

    /// Process audio with position relative to listener
    pub fn process_with_transform(
        &mut self,
        mono_input: &[f32],
        listener: &Transform,
        source_position: &Position,
        min_distance: f32,
        max_distance: f32,
    ) -> (Vec<f32>, Vec<f32>) {
        let distance = listener.position.distance_to(source_position);
        let azimuth = listener.azimuth_to(source_position);

        self.process(
            mono_input,
            azimuth,
            distance,
            min_distance,
            max_distance,
        )
    }

    /// Interleave left/right channels for output
    pub fn interleave(left: &[f32], right: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity(left.len() + right.len());
        for (l, r) in left.iter().zip(right.iter()) {
            output.push(*l);
            output.push(*r);
        }
        output
    }
}

impl Default for SpatialProcessor {
    fn default() -> Self {
        Self::new()
    }
}

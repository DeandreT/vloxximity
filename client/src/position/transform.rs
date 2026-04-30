//! Position and transform math utilities.

use serde::{Deserialize, Serialize};

/// 3D position in GW2 coordinate space
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Position {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn from_array(arr: [f32; 3]) -> Self {
        Self {
            x: arr[0],
            y: arr[1],
            z: arr[2],
        }
    }

    pub fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }

    /// Calculate distance to another position
    pub fn distance_to(&self, other: &Position) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        let dz = self.z - other.z;
        (dx * dx + dy * dy + dz * dz).sqrt()
    }

    /// Calculate horizontal (XZ plane) distance to another position
    pub fn horizontal_distance_to(&self, other: &Position) -> f32 {
        let dx = self.x - other.x;
        let dz = self.z - other.z;
        (dx * dx + dz * dz).sqrt()
    }

    /// Get direction vector to another position (normalized)
    pub fn direction_to(&self, other: &Position) -> Position {
        let dx = other.x - self.x;
        let dy = other.y - self.y;
        let dz = other.z - self.z;
        let len = (dx * dx + dy * dy + dz * dz).sqrt();
        if len > 0.0001 {
            Position::new(dx / len, dy / len, dz / len)
        } else {
            Position::new(0.0, 0.0, 1.0)
        }
    }

    /// Normalize this vector
    pub fn normalize(&self) -> Position {
        let len = self.length();
        if len > 0.0001 {
            Position::new(self.x / len, self.y / len, self.z / len)
        } else {
            Position::new(0.0, 0.0, 1.0)
        }
    }

    /// Get vector length
    pub fn length(&self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// Dot product
    pub fn dot(&self, other: &Position) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }

    /// Cross product
    pub fn cross(&self, other: &Position) -> Position {
        Position::new(
            self.y * other.z - self.z * other.y,
            self.z * other.x - self.x * other.z,
            self.x * other.y - self.y * other.x,
        )
    }
}

/// Player transform (position + orientation)
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Transform {
    pub position: Position,
    pub front: Position, // Forward direction
    pub top: Position,   // Up direction
}

impl Default for Transform {
    fn default() -> Self {
        Self {
            position: Position::default(),
            front: Position::new(0.0, 0.0, 1.0),
            top: Position::new(0.0, 1.0, 0.0),
        }
    }
}

impl Transform {
    pub fn basis(&self) -> (Position, Position, Position) {
        let front = self.front.normalize();
        let world_up = Position::new(0.0, 1.0, 0.0);
        let mut right_raw = world_up.cross(&front);
        if right_raw.length() < 1e-3 {
            // Camera looking straight up or down — world-Z is a safe fallback.
            right_raw = Position::new(0.0, 0.0, 1.0).cross(&front);
        }
        let right = right_raw.normalize();
        let up = front.cross(&right).normalize();
        (right, up, front)
    }

    /// Calculate the right vector (orthonormal basis, world-up derived).
    pub fn right(&self) -> Position {
        self.basis().0
    }

    /// Calculate relative position of another point in local space
    /// Returns (right, up, front) components for 3D audio positioning
    pub fn relative_position(&self, other: &Position) -> (f32, f32, f32) {
        let delta = Position::new(
            other.x - self.position.x,
            other.y - self.position.y,
            other.z - self.position.z,
        );

        let (right, up, front) = self.basis();

        let right_component = delta.dot(&right);
        let up_component = delta.dot(&up);
        let front_component = delta.dot(&front);

        (right_component, up_component, front_component)
    }

    /// Convert a listener-local offset `(right, up, front)` into world space.
    pub fn local_offset_to_world(
        &self,
        right_offset: f32,
        up_offset: f32,
        front_offset: f32,
    ) -> Position {
        let (right, up, front) = self.basis();

        Position::new(
            self.position.x + right.x * right_offset + up.x * up_offset + front.x * front_offset,
            self.position.y + right.y * right_offset + up.y * up_offset + front.y * front_offset,
            self.position.z + right.z * right_offset + up.z * up_offset + front.z * front_offset,
        )
    }

    /// Project a world-space point onto the 2D screen using this transform
    /// as the camera. `fov_v` is the vertical field of view in radians.
    /// Returns `None` if the point is behind the camera or outside a small
    /// margin around the viewport.
    pub fn world_to_screen(
        &self,
        target: &Position,
        fov_v: f32,
        display: [f32; 2],
    ) -> Option<[f32; 2]> {
        let [w, h] = display;
        if w <= 0.0 || h <= 0.0 || fov_v <= 0.0 {
            return None;
        }
        let (right, up, front) = self.basis();
        let delta = Position::new(
            target.x - self.position.x,
            target.y - self.position.y,
            target.z - self.position.z,
        );
        let depth = delta.dot(&front);
        if depth <= 0.5 {
            return None;
        }
        let side = delta.dot(&right);
        let up_amt = delta.dot(&up);
        let aspect = w / h;
        let f_y = 1.0 / (fov_v * 0.5).tan();
        let f_x = f_y / aspect;
        let x_ndc = side * f_x / depth;
        let y_ndc = up_amt * f_y / depth;
        if x_ndc.abs() > 1.2 || y_ndc.abs() > 1.2 {
            return None;
        }
        Some([(1.0 + x_ndc) * 0.5 * w, (1.0 - y_ndc) * 0.5 * h])
    }

    /// Calculate azimuth (horizontal angle) to another position
    /// Returns angle in radians, where 0 is forward, positive is right
    pub fn azimuth_to(&self, other: &Position) -> f32 {
        let (right, _, front) = self.relative_position(other);
        right.atan2(front)
    }

    /// Calculate elevation angle to another position
    /// Returns angle in radians, where 0 is level, positive is up
    pub fn elevation_to(&self, other: &Position) -> f32 {
        let (right, up, front) = self.relative_position(other);
        let horizontal_dist = (right * right + front * front).sqrt();
        up.atan2(horizontal_dist)
    }
}

/// Calculate volume attenuation based on distance
pub fn distance_attenuation(distance: f32, min_distance: f32, max_distance: f32) -> f32 {
    if distance <= min_distance {
        1.0
    } else if distance >= max_distance {
        0.0
    } else {
        // Linear falloff
        let range = max_distance - min_distance;
        let normalized = (distance - min_distance) / range;
        1.0 - normalized
    }
}

/// Calculate volume attenuation with rolloff curve
pub fn distance_attenuation_rolloff(
    distance: f32,
    min_distance: f32,
    max_distance: f32,
    rolloff: f32,
) -> f32 {
    if distance <= min_distance {
        1.0
    } else if distance >= max_distance {
        0.0
    } else {
        // Inverse distance rolloff
        let range = max_distance - min_distance;
        let normalized = (distance - min_distance) / range;
        (1.0 - normalized).powf(rolloff)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_distance() {
        let a = Position::new(0.0, 0.0, 0.0);
        let b = Position::new(3.0, 4.0, 0.0);
        assert!((a.distance_to(&b) - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_relative_position() {
        let transform = Transform {
            position: Position::new(0.0, 0.0, 0.0),
            front: Position::new(0.0, 0.0, 1.0),
            top: Position::new(0.0, 1.0, 0.0),
        };

        // Point directly in front.
        let front_point = Position::new(0.0, 0.0, 5.0);
        let (right, up, front) = transform.relative_position(&front_point);
        assert!(right.abs() < 0.001);
        assert!(up.abs() < 0.001);
        assert!((front - 5.0).abs() < 0.001);

        // Point at +X: listener's RIGHT.
        let right_point = Position::new(3.0, 0.0, 0.0);
        let (right, up, front) = transform.relative_position(&right_point);
        assert!((right - 3.0).abs() < 0.001);
        assert!(up.abs() < 0.001);
        assert!(front.abs() < 0.001);

        // Point at -X: listener's LEFT.
        let left_point = Position::new(-3.0, 0.0, 0.0);
        let (right, up, front) = transform.relative_position(&left_point);
        assert!((right + 3.0).abs() < 0.001);
        assert!(up.abs() < 0.001);
        assert!(front.abs() < 0.001);
    }

    #[test]
    fn test_attenuation() {
        assert!((distance_attenuation(0.0, 100.0, 1000.0) - 1.0).abs() < 0.001);
        assert!((distance_attenuation(100.0, 100.0, 1000.0) - 1.0).abs() < 0.001);
        assert!((distance_attenuation(550.0, 100.0, 1000.0) - 0.5).abs() < 0.001);
        assert!((distance_attenuation(1000.0, 100.0, 1000.0) - 0.0).abs() < 0.001);
    }

    #[test]
    fn basis_is_orthonormal_with_bogus_top() {
        let transform = Transform {
            position: Position::new(0.0, 0.0, 0.0),
            front: Position::new(0.879, 0.004, -0.478),
            top: Position::new(0.0, 0.0, 1.0),
        };
        let (r, u, f) = transform.basis();
        // Orthonormal: each vector length 1, pairwise dot products 0.
        assert!((r.length() - 1.0).abs() < 1e-4, "|r| = {}", r.length());
        assert!((u.length() - 1.0).abs() < 1e-4, "|u| = {}", u.length());
        assert!((f.length() - 1.0).abs() < 1e-4, "|f| = {}", f.length());
        assert!(r.dot(&u).abs() < 1e-4, "r·u = {}", r.dot(&u));
        assert!(r.dot(&f).abs() < 1e-4, "r·f = {}", r.dot(&f));
        assert!(u.dot(&f).abs() < 1e-4, "u·f = {}", u.dot(&f));

        // Projecting a known delta onto this basis must preserve the
        // squared norm (up to float epsilon).
        let delta = Position::new(20.1, -0.1, 0.8);
        let (rc, uc, fc) = transform.relative_position(&delta);
        let projected_norm_sq = rc * rc + uc * uc + fc * fc;
        let delta_norm_sq = delta.length() * delta.length();
        assert!(
            (projected_norm_sq - delta_norm_sq).abs() < 0.01,
            "norm mismatch: projected={} delta={}",
            projected_norm_sq,
            delta_norm_sq
        );
    }

    #[test]
    fn test_local_offset_to_world() {
        // DirectX LH: listener facing +Z has right = +X, up = +Y, front = +Z.
        // An offset of (right=3, up=2, front=7) from (10, 5, 20) lands at
        // (13, 7, 27).
        let transform = Transform {
            position: Position::new(10.0, 5.0, 20.0),
            front: Position::new(0.0, 0.0, 1.0),
            top: Position::new(0.0, 1.0, 0.0),
        };

        let point = transform.local_offset_to_world(3.0, 2.0, 7.0);
        assert!((point.x - 13.0).abs() < 0.001, "x = {}", point.x);
        assert!((point.y - 7.0).abs() < 0.001, "y = {}", point.y);
        assert!((point.z - 27.0).abs() < 0.001, "z = {}", point.z);
    }
}

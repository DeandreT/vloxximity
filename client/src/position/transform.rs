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
    pub front: Position,  // Forward direction
    pub top: Position,    // Up direction
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
    /// Calculate the right vector (perpendicular to front and top)
    pub fn right(&self) -> Position {
        // GW2 uses a coordinate system where a player facing +X (east) has
        // their right side along +Z (south). That's the opposite handedness
        // of `top × front`, so we use `front × top` to get right-ward panning
        // that matches the in-game world.
        self.front.cross(&self.top).normalize()
    }

    /// Calculate relative position of another point in local space
    /// Returns (right, up, front) components for 3D audio positioning
    pub fn relative_position(&self, other: &Position) -> (f32, f32, f32) {
        let delta = Position::new(
            other.x - self.position.x,
            other.y - self.position.y,
            other.z - self.position.z,
        );

        let right = self.right();
        let front = self.front.normalize();
        let top = self.top.normalize();

        // Project delta onto local axes
        let right_component = delta.dot(&right);
        let up_component = delta.dot(&top);
        let front_component = delta.dot(&front);

        (right_component, up_component, front_component)
    }

    /// Convert a listener-local offset `(right, up, front)` into world space.
    pub fn local_offset_to_world(&self, right_offset: f32, up_offset: f32, front_offset: f32) -> Position {
        let right = self.right();
        let front = self.front.normalize();
        let top = self.top.normalize();

        Position::new(
            self.position.x + right.x * right_offset + top.x * up_offset + front.x * front_offset,
            self.position.y + right.y * right_offset + top.y * up_offset + front.y * front_offset,
            self.position.z + right.z * right_offset + top.z * up_offset + front.z * front_offset,
        )
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

        // Point directly in front
        let front_point = Position::new(0.0, 0.0, 5.0);
        let (right, up, front) = transform.relative_position(&front_point);
        assert!(right.abs() < 0.001);
        assert!(up.abs() < 0.001);
        assert!((front - 5.0).abs() < 0.001);

        // Point to the right
        let right_point = Position::new(3.0, 0.0, 0.0);
        let (right, up, front) = transform.relative_position(&right_point);
        assert!((right - 3.0).abs() < 0.001);
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
    fn test_local_offset_to_world() {
        let transform = Transform {
            position: Position::new(10.0, 5.0, 20.0),
            front: Position::new(0.0, 0.0, 1.0),
            top: Position::new(0.0, 1.0, 0.0),
        };

        let point = transform.local_offset_to_world(3.0, 2.0, 7.0);
        assert!((point.x - 13.0).abs() < 0.001);
        assert!((point.y - 7.0).abs() < 0.001);
        assert!((point.z - 27.0).abs() < 0.001);
    }
}

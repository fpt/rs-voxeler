//! The image being drawn into: colour and depth, both CPU-side.

/// A colour buffer of packed `0RGB` u32s and a matching depth buffer.
///
/// `0RGB` because that is exactly what `softbuffer` presents, so the window
/// layer can hand the slice straight to the surface with no per-pixel repack.
/// Depth is normalized device z in `[-1, 1]`, which interpolates linearly in
/// screen space — that is the property that makes a barycentric depth
/// interpolation correct rather than merely close.
pub struct Framebuffer {
    width: u32,
    height: u32,
    color: Vec<u32>,
    depth: Vec<f32>,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let n = (width as usize) * (height as usize);
        Self {
            width,
            height,
            color: vec![0; n],
            depth: vec![f32::INFINITY; n],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn color(&self) -> &[u32] {
        &self.color
    }

    /// Grow or shrink to a new size, reusing the allocation. Contents are not
    /// preserved: every caller clears before drawing anyway, and a resize is
    /// followed by a full redraw by definition.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == self.width && height == self.height {
            return;
        }
        self.width = width;
        self.height = height;
        let n = (width as usize) * (height as usize);
        self.color.resize(n, 0);
        self.depth.resize(n, f32::INFINITY);
    }

    /// Reset for a new frame. Depth goes to infinity so the first fragment at
    /// any pixel always wins.
    pub fn clear(&mut self, color: u32) {
        self.color.fill(color);
        self.depth.fill(f32::INFINITY);
    }

    /// Fill with a vertical gradient — the editor's backdrop. A flat colour
    /// makes it hard to tell which way is up when the model is off-screen.
    pub fn clear_gradient(&mut self, top: u32, bottom: u32) {
        for y in 0..self.height {
            let t = if self.height > 1 {
                y as f32 / (self.height - 1) as f32
            } else {
                0.0
            };
            let c = lerp_rgb(top, bottom, t);
            let row = (y * self.width) as usize;
            self.color[row..row + self.width as usize].fill(c);
        }
        self.depth.fill(f32::INFINITY);
    }

    /// Depth-tested write. Returns whether the fragment survived.
    #[inline]
    pub fn test_and_set(&mut self, x: u32, y: u32, z: f32, color: u32) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let i = (y * self.width + x) as usize;
        if z >= self.depth[i] {
            return false;
        }
        self.depth[i] = z;
        self.color[i] = color;
        true
    }

    /// Write ignoring depth — for the 2D overlay, which is always on top.
    #[inline]
    pub fn set(&mut self, x: u32, y: u32, color: u32) {
        if x < self.width && y < self.height {
            self.color[(y * self.width + x) as usize] = color;
        }
    }

    /// The depth at a pixel, for tests and for hit-free debugging.
    pub fn depth_at(&self, x: u32, y: u32) -> f32 {
        if x >= self.width || y >= self.height {
            return f32::INFINITY;
        }
        self.depth[(y * self.width + x) as usize]
    }

    pub fn color_at(&self, x: u32, y: u32) -> u32 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.color[(y * self.width + x) as usize]
    }
}

fn lerp_rgb(a: u32, b: u32, t: f32) -> u32 {
    let ch = |shift: u32| {
        let (a, b) = ((a >> shift) & 0xFF, (b >> shift) & 0xFF);
        (a as f32 + (b as f32 - a as f32) * t).round().clamp(0.0, 255.0) as u32
    };
    ch(16) << 16 | ch(8) << 8 | ch(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearer_fragment_wins_regardless_of_draw_order() {
        let mut fb = Framebuffer::new(4, 4);
        fb.clear(0);
        assert!(fb.test_and_set(1, 1, 0.5, 0xAA));
        assert!(!fb.test_and_set(1, 1, 0.9, 0xBB));
        assert_eq!(fb.color_at(1, 1), 0xAA);
        assert!(fb.test_and_set(1, 1, 0.1, 0xCC));
        assert_eq!(fb.color_at(1, 1), 0xCC);
    }

    #[test]
    fn writes_outside_the_buffer_are_dropped() {
        let mut fb = Framebuffer::new(4, 4);
        assert!(!fb.test_and_set(4, 0, 0.0, 0xFF));
        assert!(!fb.test_and_set(0, 99, 0.0, 0xFF));
        fb.set(99, 99, 0xFF); // must not panic
    }

    #[test]
    fn a_resize_leaves_the_buffers_the_same_length_as_the_image() {
        let mut fb = Framebuffer::new(4, 4);
        fb.resize(7, 3);
        assert_eq!(fb.width(), 7);
        assert_eq!(fb.color().len(), 21);
        fb.clear(0);
        assert!(fb.test_and_set(6, 2, 0.0, 1));
    }

    #[test]
    fn the_gradient_runs_top_to_bottom_and_clears_depth() {
        let mut fb = Framebuffer::new(2, 3);
        fb.test_and_set(0, 0, 0.5, 0);
        fb.clear_gradient(0x000000, 0xFFFFFF);
        assert_eq!(fb.color_at(0, 0), 0x000000);
        assert_eq!(fb.color_at(0, 2), 0xFFFFFF);
        assert_eq!(fb.depth_at(0, 0), f32::INFINITY);
    }
}

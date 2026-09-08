//! The 256-entry colour table a model's indices point into.

/// One palette colour. No alpha: a voxel is either present or air, and air is
/// index 0 — there is no third state for a renderer to blend.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb8 {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Pack into the `0RGB` u32 a CPU surface wants.
    pub const fn to_u32(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }

    /// Scale every channel by `f` (0.0–1.0+), saturating. This is the whole of
    /// flat shading: a face's colour is its palette colour times how much light
    /// reaches it.
    pub fn scaled(self, f: f32) -> Self {
        let c = |v: u8| (v as f32 * f).clamp(0.0, 255.0) as u8;
        Self::new(c(self.r), c(self.g), c(self.b))
    }
}

/// 256 colours indexed by a voxel's stored byte.
///
/// Entry 0 is air's slot. It is kept in the array so that indexing never needs
/// an offset, but nothing draws it and the editor never offers it as a colour —
/// which is why [`Palette::pickable`] starts at 1.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Palette {
    colors: [Rgb8; 256],
}

impl Palette {
    pub fn from_colors(colors: [Rgb8; 256]) -> Self {
        Self { colors }
    }

    pub fn get(&self, index: u8) -> Rgb8 {
        self.colors[index as usize]
    }

    pub fn set(&mut self, index: u8, color: Rgb8) {
        self.colors[index as usize] = color;
    }

    pub fn colors(&self) -> &[Rgb8; 256] {
        &self.colors
    }

    /// The indices a user may paint with — everything but air.
    pub fn pickable() -> std::ops::RangeInclusive<u8> {
        1..=255
    }
}

impl Default for Palette {
    /// The default ramp is MagicaVoxel's own: a 6×6×6 RGB cube in indices
    /// 1..=215 followed by three 5-step grey/primary ramps. Importing a `.vox`
    /// that omits its `RGBA` chunk means "I used the default palette", so the
    /// default here has to *be* that palette or such a file loads recoloured.
    fn default() -> Self {
        let mut colors = [Rgb8::default(); 256];
        // MagicaVoxel's built-in table, expanded from its packed 0xAABBGGRR
        // form. Index i of this list is palette index i + 1.
        const DEFAULT: [u32; 255] = [
            0xffffff, 0xffccff, 0xff99ff, 0xff66ff, 0xff33ff, 0xff00ff, 0xffffcc, 0xffcccc,
            0xff99cc, 0xff66cc, 0xff33cc, 0xff00cc, 0xffff99, 0xffcc99, 0xff9999, 0xff6699,
            0xff3399, 0xff0099, 0xffff66, 0xffcc66, 0xff9966, 0xff6666, 0xff3366, 0xff0066,
            0xffff33, 0xffcc33, 0xff9933, 0xff6633, 0xff3333, 0xff0033, 0xffff00, 0xffcc00,
            0xff9900, 0xff6600, 0xff3300, 0xff0000, 0xccffff, 0xccccff, 0xcc99ff, 0xcc66ff,
            0xcc33ff, 0xcc00ff, 0xccffcc, 0xcccccc, 0xcc99cc, 0xcc66cc, 0xcc33cc, 0xcc00cc,
            0xccff99, 0xcccc99, 0xcc9999, 0xcc6699, 0xcc3399, 0xcc0099, 0xccff66, 0xcccc66,
            0xcc9966, 0xcc6666, 0xcc3366, 0xcc0066, 0xccff33, 0xcccc33, 0xcc9933, 0xcc6633,
            0xcc3333, 0xcc0033, 0xccff00, 0xcccc00, 0xcc9900, 0xcc6600, 0xcc3300, 0xcc0000,
            0x99ffff, 0x99ccff, 0x9999ff, 0x9966ff, 0x9933ff, 0x9900ff, 0x99ffcc, 0x99cccc,
            0x9999cc, 0x9966cc, 0x9933cc, 0x9900cc, 0x99ff99, 0x99cc99, 0x999999, 0x996699,
            0x993399, 0x990099, 0x99ff66, 0x99cc66, 0x999966, 0x996666, 0x993366, 0x990066,
            0x99ff33, 0x99cc33, 0x999933, 0x996633, 0x993333, 0x990033, 0x99ff00, 0x99cc00,
            0x999900, 0x996600, 0x993300, 0x990000, 0x66ffff, 0x66ccff, 0x6699ff, 0x6666ff,
            0x6633ff, 0x6600ff, 0x66ffcc, 0x66cccc, 0x6699cc, 0x6666cc, 0x6633cc, 0x6600cc,
            0x66ff99, 0x66cc99, 0x669999, 0x666699, 0x663399, 0x660099, 0x66ff66, 0x66cc66,
            0x669966, 0x666666, 0x663366, 0x660066, 0x66ff33, 0x66cc33, 0x669933, 0x666633,
            0x663333, 0x660033, 0x66ff00, 0x66cc00, 0x669900, 0x666600, 0x663300, 0x660000,
            0x33ffff, 0x33ccff, 0x3399ff, 0x3366ff, 0x3333ff, 0x3300ff, 0x33ffcc, 0x33cccc,
            0x3399cc, 0x3366cc, 0x3333cc, 0x3300cc, 0x33ff99, 0x33cc99, 0x339999, 0x336699,
            0x333399, 0x330099, 0x33ff66, 0x33cc66, 0x339966, 0x336666, 0x333366, 0x330066,
            0x33ff33, 0x33cc33, 0x339933, 0x336633, 0x333333, 0x330033, 0x33ff00, 0x33cc00,
            0x339900, 0x336600, 0x333300, 0x330000, 0x00ffff, 0x00ccff, 0x0099ff, 0x0066ff,
            0x0033ff, 0x0000ff, 0x00ffcc, 0x00cccc, 0x0099cc, 0x0066cc, 0x0033cc, 0x0000cc,
            0x00ff99, 0x00cc99, 0x009999, 0x006699, 0x003399, 0x000099, 0x00ff66, 0x00cc66,
            0x009966, 0x006666, 0x003366, 0x000066, 0x00ff33, 0x00cc33, 0x009933, 0x006633,
            0x003333, 0x000033, 0x00ff00, 0x00cc00, 0x009900, 0x006600, 0x003300, 0x0000ee,
            0x0000dd, 0x0000bb, 0x0000aa, 0x000088, 0x000077, 0x000055, 0x000044, 0x000022,
            0x000011, 0x00ee00, 0x00dd00, 0x00bb00, 0x00aa00, 0x008800, 0x007700, 0x005500,
            0x004400, 0x002200, 0x001100, 0xee0000, 0xdd0000, 0xbb0000, 0xaa0000, 0x880000,
            0x770000, 0x550000, 0x440000, 0x220000, 0x110000, 0xeeeeee, 0xdddddd, 0xbbbbbb,
            0xaaaaaa, 0x888888, 0x777777, 0x555555, 0x444444, 0x222222, 0x111111,
        ];
        for (i, rgb) in DEFAULT.iter().enumerate() {
            colors[i + 1] = Rgb8::new((rgb >> 16) as u8, (rgb >> 8) as u8, *rgb as u8);
        }
        Self { colors }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn air_is_black_and_index_one_is_white() {
        let p = Palette::default();
        assert_eq!(p.get(0), Rgb8::new(0, 0, 0));
        assert_eq!(p.get(1), Rgb8::new(255, 255, 255));
    }

    /// The last entry is the darkest grey, not an accidental zero — a truncated
    /// table would leave the tail black and only show up on an imported model.
    #[test]
    fn the_ramp_runs_to_the_last_entry() {
        assert_eq!(Palette::default().get(255), Rgb8::new(0x11, 0x11, 0x11));
    }

    #[test]
    fn scaling_saturates_rather_than_wrapping() {
        assert_eq!(
            Rgb8::new(200, 200, 200).scaled(4.0),
            Rgb8::new(255, 255, 255)
        );
    }
}

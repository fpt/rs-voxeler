//! What a write costs, by the shape of the writing.
//!
//! A layer's box grows to fit what is drawn on it, and growing means moving the
//! contents. That is invisible at the default 32³ and grows with the cube of the
//! scene, so the only honest way to reason about it is to run it:
//!
//! ```text
//! cargo run --release --example write-cost -p voxel-core
//! ```
//!
//! The four shapes are the ones the policy is actually optimising for. A bulk
//! fill and a scatter grow the box on nearly every write; a drag and a part
//! built off in a corner settle almost at once and are what the tight box is
//! *for*. `declared` sets the box up front and is the floor — what the same
//! writes cost with no growth at all.

use std::time::Instant;

use voxel_core::model::{Bounds, VoxelModel};

fn ms(f: impl FnOnce()) -> u128 {
    let t = Instant::now();
    f();
    t.elapsed().as_millis()
}

/// Deterministic, so two runs are comparable. Not a good generator; it does not
/// have to be, it only has to scatter.
fn spread(seed: u64) -> impl FnMut(i32) -> i32 {
    let mut s = seed;
    move |k| {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((s >> 33) as i32).rem_euclid(k)
    }
}

/// The whole scene, x-major: the shape `put_rect` writes.
fn bulk(n: u16, declared: bool) -> (u128, usize) {
    let mut m = VoxelModel::new(n, n, n);
    if declared {
        m.set_layer_bounds(0, Bounds::new([0, 0, 0], [n, n, n]));
    }
    let t = ms(|| {
        for z in 0..n as i32 {
            for y in 0..n as i32 {
                for x in 0..n as i32 {
                    m.set_in(0, x, y, z, 4);
                }
            }
        }
    });
    (t, m.allocated_cells())
}

/// A drag: a couple of thousand cells wandering in one small neighbourhood.
fn drag(n: u16) -> (u128, usize) {
    let mut m = VoxelModel::new(n, n, n);
    let c = (n / 2) as i32;
    let mut r = spread(12345);
    let t = ms(|| {
        for i in 0..2000 {
            m.set_in(0, c + r(24) - 12, c + r(24) - 12, c + r(24) - 12, (i % 200 + 1) as u8);
        }
    });
    (t, m.allocated_cells())
}

/// Cells thrown anywhere in the scene — the worst case for a box, which reaches
/// the whole range within a few dozen writes and then stops growing.
fn scatter(n: u16, count: usize) -> (u128, usize) {
    let mut m = VoxelModel::new(n, n, n);
    let mut r = spread(999);
    let t = ms(|| {
        for _ in 0..count {
            m.set_in(0, r(n as i32), r(n as i32), r(n as i32), 7);
        }
    });
    (t, m.allocated_cells())
}

/// A 16×32×16 part built off in a corner: what a per-layer box exists for. The
/// allocation here is the number the whole design is judged by.
fn part(n: u16) -> (u128, usize) {
    let mut m = VoxelModel::new(n, n, n);
    let t = ms(|| {
        for z in 0..16 {
            for y in 0..32 {
                for x in 0..16 {
                    m.set_in(0, 200 + x, 8 + y, 200 + z, 3);
                }
            }
        }
    });
    (t, m.allocated_cells())
}

fn main() {
    for n in [64u16, 128, 256] {
        let (grown, _) = bulk(n, false);
        let (declared, cells) = bulk(n, true);
        println!("bulk fill {n:>3}^3     {grown:>5} ms   (declared {declared:>4} ms)   {cells} cells");
    }
    let (t, cells) = drag(256);
    println!("drag 2 000 cells   {t:>5} ms                       {cells} cells");
    for c in [1_000usize, 10_000] {
        let (t, cells) = scatter(256, c);
        println!("scatter {c:>6}     {t:>5} ms                       {cells} cells");
    }
    let (t, cells) = part(256);
    println!("16x32x16 part      {t:>5} ms                       {cells} cells (8192 ideal)");
}

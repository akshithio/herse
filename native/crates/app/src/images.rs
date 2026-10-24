//! A memory budget for decoded output images (figures, charts, animation
//! frames, widget images).
//!
//! GPUI decodes an `Arc<Image>` once and keeps the pixels, and its GPU
//! texture, in a global cache until told otherwise, so scrolling through a
//! notebook of 300 figures used to hold all 300 decoded (1.2 GB; 0.6 GB with this). Views
//! [`touch`] the images they draw each frame; [`collect`] runs at the start
//! of the next frame and, once the decoded total passes [`BUDGET`], evicts
//! the images unseen the longest. One scrolled back into view just decodes
//! again (off the main thread). Images on screen are never evicted.

use std::{collections::HashMap, sync::Arc};

use gpui_kit::{App, Global, Image, Window};

/// Decoded bytes kept before evicting: about 20 large retina figures, several
/// screens of scrolling back. Each also has a GPU texture of about the same
/// size, so this is roughly half the memory images can hold.
const BUDGET: usize = 128 * 1024 * 1024;
/// Images drawn within this many frames stay, whatever the budget.
const PROTECTED_FRAMES: u64 = 2;

#[derive(Default)]
struct ImageBudget {
    frame: u64,
    entries: HashMap<u64, Entry>,
    /// Decoded bytes of the entries whose size is known.
    total: usize,
}

struct Entry {
    image: Arc<Image>,
    last_frame: u64,
    /// Decoded size; 0 until the decode has finished.
    bytes: usize,
}

impl Global for ImageBudget {}

/// Record that `image` is drawn this frame.
pub fn touch(image: &Arc<Image>, cx: &mut App) {
    let budget = cx.default_global::<ImageBudget>();
    let frame = budget.frame;
    budget
        .entries
        .entry(image.id)
        .and_modify(|e| e.last_frame = frame)
        .or_insert_with(|| Entry {
            image: image.clone(),
            last_frame: frame,
            bytes: 0,
        });
}

/// Start a frame: size newly decoded images, and evict the least recently
/// drawn ones while over budget.
pub fn collect(window: &mut Window, cx: &mut App) {
    let mut budget = std::mem::take(cx.default_global::<ImageBudget>());
    budget.frame += 1;
    for entry in budget.entries.values_mut().filter(|e| e.bytes == 0) {
        // Only already-touched images, whose load the `img` element started:
        // this never begins a decode of its own.
        if let Some(decoded) = entry.image.clone().get_render_image(window, cx) {
            let bytes: usize = (0..).map_while(|i| decoded.as_bytes(i).map(<[u8]>::len)).sum();
            entry.bytes = bytes.max(1);
            budget.total += entry.bytes;
        }
    }
    // Never decoded (still loading long after, failed, or replaced before it
    // was drawn twice): nothing to free, just forget it.
    let frame = budget.frame;
    budget.entries.retain(|_, e| e.bytes > 0 || e.last_frame + 600 > frame);
    if budget.total > BUDGET {
        let mut stale: Vec<(u64, u64)> = budget
            .entries
            .iter()
            .filter(|(_, e)| e.bytes > 0 && e.last_frame + PROTECTED_FRAMES < frame)
            .map(|(id, e)| (e.last_frame, *id))
            .collect();
        stale.sort_unstable();
        for (_, id) in stale {
            if budget.total <= BUDGET * 3 / 4 {
                break;
            }
            let Some(entry) = budget.entries.remove(&id) else { continue };
            budget.total -= entry.bytes;
            if let Some(decoded) = entry.image.clone().get_render_image(window, cx) {
                cx.drop_image(decoded, Some(window));
            }
            entry.image.remove_asset(cx);
        }
    }
    *cx.default_global::<ImageBudget>() = budget;
}

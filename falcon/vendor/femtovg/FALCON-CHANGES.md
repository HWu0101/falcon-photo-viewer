# Falcon changes to FemtoVG 0.25.1

Source: the published femtovg 0.25.1 crate, licensed under MIT OR Apache-2.0.
The original licence files and source notices are retained.

2026-10-04: keep cached wgpu scene pipelines across clear/target-only flushes.
Slint's rendering notifier performs a preparatory flush before drawing the scene.
The original per-flush cache eviction removes the scene pipelines at that point,
causing them to compile again on every frame. The patch executes/submits the clear
normally, and prunes/resets access bits only after actual scene drawing. It does
not retain photo textures, change shaders or alter rendering order.

Upstream report and maintainer confirmation:
https://github.com/slint-ui/slint/issues/12030

The production pruning helper has a CPU regression included in Falcon's native
test binary. This avoids upstream unit tests that require an example font absent
from the published crate. Metal execution remains a separate platform check.

2026-11-14: stop creating GPU binding objects for every draw call (wgpu renderer,
`src/renderer/wgpu.rs` and the new `src/renderer/wgpu/binding_cache.rs`).
FemtoVG 0.25.1 made a uniform buffer, two samplers, two texture views and a bind
group for almost every draw call: about 8,000 bind groups and 33,000 other objects
a frame for a 500-tile grid, most of the interface thread's frame time. Now:
- samplers are kept per filter/repeat combination (8 at most);
- each image keeps its default texture view, made when it is allocated or imported;
- a flush's fragment uniforms go into one shared buffer, bound with a dynamic
  offset (stride: the record rounded up to `min_uniform_buffer_offset_alignment`),
  filled through a per-flush staging buffer copied at the start of the same
  command buffer, so a command buffer submitted after a later flush still reads
  its own records (`queue.write_buffer` would not);
- bind groups are kept per (image, glyph image) pair until either image goes:
  at once when it is deleted, at the next deletion or flush when it is replaced
  by `realloc`; a grown uniform buffer starts a new set;
- viewport bind groups and stencil views are kept too.
Shaders, blending, render order, the image flags' effect and the 2026-10-04
pipeline-cache patch are unchanged. Output was identical byte for byte to the
original renderer in Falcon's drawing tests (a scene through every command type on
Vulkan and DirectX 12, and the grid test program's snapshots). Upstream FemtoVG made
the same kind of change in 0.27.0; Slint 1.17 pins 0.25.x. The bookkeeping logic has
CPU tests in Falcon's native test binary. Metal execution remains a separate
platform check.

---
name: Blocks Camera Orbit
overview: Fix the 3D blocks camera so it looks down onto block tops (not up from below), and add middle-mouse drag-on-empty-space orbit around the scene center, with yaw/pitch stored on BlocksState.
todos:
  - id: camera-above
    content: Negate default pitch; Camera from BlocksState yaw/pitch; clamp pitch; update center_of
    status: completed
  - id: mmb-orbit
    content: MMB empty-space orbit state + delta handling; don't fight LMB collect
    status: completed
  - id: cull-check
    content: Confirm top-face winding after pitch flip; flip cull if needed
    status: completed
  - id: verify-ui
    content: cargo check; smoke tops visible + MMB orbit
    status: completed
isProject: false
---

# Fix blocks camera (above + MMB orbit)

## Root cause

In [`src/ui/blocks.rs`](src/ui/blocks.rs), pitch `+0.82` rotates the scene so the eye sits **under** the city. Only side faces are emitted (no bottom face), so tops never appear — matching the screenshot. Labels sit on projected tops, so they are also unreadable from below.

## Camera look-from-above

- Change default pitch to **negative** (looking down), keep yaw ~`0.42`.
- Build `Camera` from stored angles each frame instead of hard-coding in `Camera::new`.
- Clamp pitch to about `[-1.25, -0.20]` so orbit cannot flip under the floor or go fully flat.
- Keep perspective/`distance` as-is; ground plane stays at `y=0` under the blocks.

```rust
// BlocksState defaults
yaw: 0.42,
pitch: -0.82, // look down onto tops
```

`center_of(viewport)` must use the same stored yaw/pitch so director aiming stays aligned after orbit.

## MMB orbit (empty space only)

Persist on [`BlocksState`](src/ui/blocks.rs):

- `yaw`, `pitch`
- `orbit: Option<Pos2>` — set when middle button presses on chart with **no** block hit; cleared on release

In `blocks_ui` interaction (before/alongside LMB collect drag):

1. On middle press inside `area`, if `hit` is `None` → start orbit (`orbit = Some(pointer)`).
2. While middle held and `orbit` is `Some`, apply `pointer.delta()`: `yaw += dx * sens`, `pitch = (pitch + dy * sens).clamp(...)`.
3. On middle release → clear `orbit`.
4. While orbiting: skip LMB collect-drag start, suppress click/navigate on that gesture, show `CursorIcon::Grabbing`.

Use egui `PointerButton::Middle` + `pointer.delta()` from input; do not rely on `Sense::click_and_drag()` (that tracks primary).

## Touch-ups after pitch flip

- Re-check projected winding cull (`cross >= 0` skip): with look-down, top faces must survive; if tops still vanish, flip the cull inequality once.
- Soften or keep the ground plane; after the camera fix it should read as a floor, not a wall.

## Verify

- Rebuild, open **3D Blocks**: tops and hover labels readable; free-space/ground sit under the city.
- MMB drag on empty void orbits; MMB on a block does nothing; LMB drag-to-collect unchanged.

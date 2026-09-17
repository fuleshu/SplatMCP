"""A procedural height field: the simplest useful "build something" recipe.

It is deliberately plain NumPy. Everything the executor adds is visible here: the seed
arrives in `ctx`, progress and cancellation are reported explicitly, and the result is a
`splatmcp.Batch` whose arrays follow the documented contract.

Run it from an MCP client:

    run_python_splat({
        "request_id": "terrain-1",
        "script_path": "examples/height_field.py",
        "params": {"size": 256},
        "seed": 11,
        "component_id": "terrain",
        "file_name": "terrain.ply"
    })
"""

import numpy as np

import splatmcp


def generate(ctx):
    size = int(ctx.params.get("size", 256))
    spacing = float(ctx.params.get("spacing", 0.02))

    # A grid in the XZ plane. Document space is Y-down, which matches the PLY format the
    # core model writes, so no axis conversion is needed here.
    axis = (np.arange(size) - size / 2.0) * spacing
    grid_x, grid_z = np.meshgrid(axis, axis)
    height = 0.15 * np.sin(grid_x * 3.0) * np.cos(grid_z * 3.0)

    positions = (
        np.stack([grid_x, height, grid_z], axis=-1).reshape(-1, 3).astype(np.float32)
    )
    count = len(positions)
    ctx.progress(0.5, "grid sampled")
    ctx.check_cancelled()

    # Colour by height, so the shape is visible in any viewer.
    shade = ((height.reshape(-1) + 0.15) / 0.3).astype(np.float32)
    colors = np.stack(
        [0.25 + 0.5 * shade, 0.4 + 0.2 * (1.0 - shade), 0.9 - 0.5 * shade], axis=1
    ).astype(np.float32)

    ctx.log("height field %dx%d, %d gaussians" % (size, size, count))
    return splatmcp.batch(
        positions=positions,
        scales=np.full((count, 3), spacing * 0.6, dtype=np.float32),
        rotations=np.tile(np.array([1.0, 0.0, 0.0, 0.0], dtype=np.float32), (count, 1)),
        colors=colors,
        opacity=np.ones(count, dtype=np.float32),
        component_id="terrain",
        recipe="height_field",
        seed=ctx.seed,
    )

"""Half a million anisotropic Gaussians, built from a seeded NumPy recipe.

This is the compact-submission example: the whole job is a few hundred bytes on the wire,
whatever the point count, and the same seed reproduces the same geometry exactly.

Run it from an MCP client:

    run_python_splat({
        "request_id": "noise-cloud-1",
        "script_path": "examples/noise_cloud.py",
        "params": {"count": 500000},
        "seed": 42,
        "file_name": "noise.ply",
        "export_path": ".tmp/noise.ply"
    })

or paste it into the desktop app's Python panel.
"""

import numpy as np

import splatmcp


def generate(ctx):
    """Builds a Gaussian cloud and returns it as one batch.

    `ctx` carries the job identity, seed, parameters, progress and cancellation hooks and
    the deterministic RNG, so the recipe needs no globals.
    """
    count = int(ctx.params.get("count", 500000))
    spread = float(ctx.params.get("spread", 0.35))
    rng = ctx.rng()

    # Positions: a normal cloud, one axis at a time, from the job's own RNG.
    positions = np.stack(
        [rng.normal_array(count, 0.0, spread) for _ in range(3)], axis=1
    ).astype(np.float32)
    ctx.progress(0.4, "positions sampled")
    ctx.check_cancelled()

    # Scales are activated radii, not PLY log-scales: every value is positive.
    scales = np.stack([rng.array(count, 0.002, 0.02) for _ in range(3)], axis=1).astype(
        np.float32
    )
    rotations = np.tile(np.array([1.0, 0.0, 0.0, 0.0], dtype=np.float32), (count, 1))
    colors = np.stack([rng.array(count, 0.0, 1.0) for _ in range(3)], axis=1).astype(
        np.float32
    )
    opacity = rng.array(count, 0.4, 1.0).astype(np.float32)

    ctx.progress(0.9, "arrays built")
    ctx.log("generated %d gaussians with seed %d" % (count, ctx.seed))
    ctx.check_cancelled()

    return splatmcp.batch(
        positions=positions,
        scales=scales,
        rotations=rotations,
        colors=colors,
        opacity=opacity,
        component_id="cloud",
        recipe="noise_cloud",
        seed=ctx.seed,
    )

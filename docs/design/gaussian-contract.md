# The Gaussian data and coordinate contract

**Version 1** — implemented by `splatmcp-core::contract` (`CONTRACT_VERSION = 1`) and consumed
by the MCP server, the desktop app and the Python generation adapter.

Everything that touches Gaussian data speaks this one contract: the core model, PLY import
and export, the edit operations, the Python array adapter and the viewer transform. The
contract exists because two sign errors - one in a script, one in the viewer - cancel out in
a symmetric model and only show up on an asymmetric one. Stating the conventions once, in
code, is what makes a disagreement a failing test instead of a wrong picture.

## Document space

| Property | Value |
| --- | --- |
| Handedness | right-handed |
| Axes | `+X` right, `+Y` **down**, `+Z` forward |
| Up / forward | up is `-Y`, forward is `+Z` |
| Length unit | metres |
| Precision | `f32`, serialized as PLY `float` (binary little endian) |
| Spherical harmonics | degree 0 only (fixed RGB colour) |

3DGS files are authored Y-down, so an imported PLY needs **no** axis conversion to become the
model, and the document is not Y-up. The viewer is the only place that changes space (see
[Viewer transform](#viewer-transform)).

## Attributes

| Attribute | Shape | Meaning | PLY property | Serialized value |
| --- | --- | --- | --- | --- |
| `position` | `(N, 3)` | world-space centre, metres | `x`, `y`, `z` | unchanged |
| `scale` | `(N, 3)` | **activated** radius per local axis, metres, `> 0` | `scale_0..2` | `ln(radius)` |
| `color` | `(N, 3)` | **linear** RGB in `0..=1` | `f_dc_0..2` | `(channel - 0.5) / SH_C0` |
| `opacity` | `(N)` | activated opacity in `0..=1` | `opacity` | logistic logit |
| `rotation` | `(N, 4)` | unit quaternion `(w, x, y, z)` | `rot_0..3` | unchanged |

The stored values are always the *activated* ones. A caller never writes a log-scale or a
logit, and a reader never leaves one in the model.

### Serialized endpoints

These are valid data, not damage, and are deliberately not reported as repairs:

- a log-scale of `0.0` is a radius of `1 m`;
- a logit of `+inf` / `-inf` is opacity `1` / `0`;
- an `f_dc_*` coefficient at the edge of the representable range is a black or white
  gaussian. The representable window is `[-0.5 / SH_C0, 0.5 / SH_C0]`, exposed as
  `contract::DC_MIN` / `DC_MAX`.

Opacity is written as a logit whose endpoints are pulled in by half a `1/255` step, so
`opacity = 0` round trips as `0.5/255` and `1` as `1 - 0.5/255`. Mid-range opacities round
trip to within `1e-6`.

## Rotation semantics

`rotation` is an **active** rotation of the gaussian's own frame: it maps the local axes onto
document axes. That is exactly what `contract::rotate_vector` does, and it is the rotation a
renderer uses when it builds the gaussian's covariance

```text
covariance = R * diag(scale^2) * R^T          (contract::covariance)
```

so a test that compares covariances checks scale **and** orientation, not just the centre of
the ellipsoid.

- Storage order is `(w, x, y, z)`. SciPy's `Rotation` uses `(x, y, z, w)`, so a script
  converts once at the boundary with `contract::to_scipy_quaternion` /
  `from_scipy_quaternion`.
- **Normalization policy**: a finite, non-degenerate quaternion is rescaled to unit length
  (its length carries no information). A quaternion that is `NaN`, all zero, or shorter than
  `QUATERNION_MIN_NORM` (`1e-6`) has no direction and is *refused*, never silently replaced
  by an identity rotation.

## Colour

`color` is linear RGB. No gamma conversion happens on import, export, editing or inspection:
the PLY field is an SH degree-0 coefficient that is linear in the same way. `linear_to_srgb`
and `srgb_to_linear` exist for images a caller writes or reads (an export, a swatch file) —
never for the model or the file format — so a value can never be gamma converted twice.

## PLY import: policy, attributes, defaults and diagnostics

Import is **strict by default**. A file whose values do not satisfy the contract is refused,
with bounded *indexed* diagnostics (`point 3 rotation [0, 0, 0, 0] must be a non-zero
(w, x, y, z) quaternion`), and the refusal says how to accept it deliberately. Silent repair
is never the default at an import boundary.

| Policy | Who selects it | Behaviour |
| --- | --- | --- |
| `PlyImportPolicy::Strict` | the default: `read_ply`, and every tool that was not asked otherwise | refuses a file that needs repair |
| `PlyImportPolicy::Repair` | `read_ply_repairing`, or `repair: true` on `load_splat`, `edit_splat`, `splat_info`, `viewer.load_ply` and `document.reload` | repairs, and reports every change with the index it happened at |

### What is refused, what is repaired, and what is neither

| Value | Strict | Repair |
| --- | --- | --- |
| position, `f_dc_*` coefficient, or an opacity logit that is `NaN`/infinite where infinite has no meaning | error | error: nothing sensible can be invented |
| radius that is zero, unreadable or overflowing (`exp` of an extreme log-scale) | error `point N scale` | `f32::MIN_POSITIVE`, reported |
| quaternion that is all zero, unreadable or shorter than `QUATERNION_MIN_NORM` | error `point N rotation` | identity rotation, reported |
| `f_dc_*` outside the representable window | error `point N color` | clamped to the endpoint, reported |
| finite quaternion whose length is not 1 (beyond `QUATERNION_LENGTH_TOLERANCE`) | accepted, rescaled, and **counted in the report** | same, reported |
| serialized endpoints: log-scale `0`, logit `+/-inf` | accepted, not reported | accepted, not reported |

One gaussian contributes at most one violation, so a report's counts are counts of gaussians
to fix rather than a pile of field errors.

- **Required properties**: the 14 canonical ones (`x y z f_dc_0..2 opacity scale_0..2
  rot_0..3`). A file missing one is refused; nothing is silently defaulted at the file
  boundary.
- **Written**: the canonical 17-property order, including `nx`/`ny`/`nz` as zeros, which
  other viewers expect. The model keeps no normals, so a re-import reports them as dropped.
- **Discarded, with a reason** (`contract::ply_attribute_use`): `f_rest_*` (higher
  spherical-harmonic bands), `nx`/`ny`/`nz`, and anything unknown to the contract.
- **Defaulted at the API boundary, not the file**: tool calls that omit an attribute get the
  documented defaults (`color = [0.85, 0.25, 0.2]`, `opacity = 0.9`, `radius = 0.02 m`,
  identity rotation), which are part of the tool schema rather than hidden in a parser.

`read_ply_with_policy` returns the gaussians the policy promises, plus a `PlyReport`:

| Report part | Contents |
| --- | --- |
| `policy` | `strict` or `repair` |
| `discarded` | every dropped attribute with the reason |
| `ignored_elements` | every non-`vertex` element that was stepped over |
| `repairs` / `total_repairs` | repaired values, the first 16 listed, all counted |
| `normalized` / `total_normalized` | quaternions rescaled to unit length, bounded the same way |

The tool and bridge layers turn that report into a bounded `PlyImportSummary` in the reply
whenever the import was not lossless (`load_splat`, `edit_splat`, `splat_info` on a path,
`viewer.load_ply`, `document.reload`), so whoever receives the geometry also receives what
the import did to the file.


**Errors** (nothing sensible can be invented): a non-finite position, a non-finite `f_dc`
coefficient, and a `NaN` opacity logit. `±inf` logits are the opacity endpoints above.

## Validation and structured errors

Validation is centralized in `splatmcp-core::validation` and shared by every adapter, so the
app, the MCP server and the Python job service cannot disagree about what "a valid gaussian"
is.

| Aspect | Rule |
| --- | --- |
| Order | raw values are checked **before** any clamping constructor |
| Location | the first failure of each gaussian is reported with its index and field |
| Boundedness | at most `MAX_REPORTED_ISSUES` (32) located issues, with the true totals kept |
| Reasons | `non_finite_value`, `non_positive_scale`, `degenerate_quaternion`, `color_out_of_range`, `opacity_out_of_range`, `empty` |
| Tolerance | colours and opacities within `RANGE_TOLERANCE` (`1e-3`) of the range are accepted (a float round trip may push `1.0` just past it); anything further out is refused |
| Policy vs mathematics | a maximum point count is reported through `ValidationReport::limits` / `within_limits` / `limit_message`, **never** mixed into the issue list, and the limit actually applied is reported back |

Entry points, and how each one fails:

| Entry point | Behaviour |
| --- | --- |
| `SplatPoint::try_new` / `try_new_at` | strict: refuses and names the reason |
| `validation::check_values` / `check_gaussian` / `IssueRecorder` | strict, indexed, bounded reporting for an adapter's own arrays |
| `Splat::check` / `check_strict` | full report, or a structured `ValidationError` |
| `Splat::validate` | the document invariant: the first problem as a terse message |
| `read_ply` / `read_ply_with_policy(.., Strict)` | refuses a file that needs repair, with indexed diagnostics |
| `read_ply_repairing` / `read_ply_with_policy(.., Repair)` | repairs and reports every change |
| MCP `load_splat`, `edit_splat`, `splat_info` | strict unless `repair: true`; the reply carries `import` |
| bridge `viewer.load_ply`, `document.reload` | strict unless `repair: true`; the reply carries `import` |
| MCP `create_splat` / `edit_splat` (`merge`) | explicit points are validated before they are clamped, with `points[i]`/`point i` in the message |
| Python `GaussianBatch::validate` | the same rules for the NumPy arrays (own error codes, same reasons) |

`SplatError::Invalid(ValidationError)` carries the structured issues, so an adapter can render
them however its surface expects without re-deriving anything.

## Inspection

`Splat::inspection(limits)` is one pass over the gaussians and returns a fixed-size
`InspectionReport`:

- `point_count`, `bounds` (padded by each gaussian's largest radius);
- `scale` per axis, `largest_radius`, `opacity` and `color` per channel as min/max/mean
  distributions, each with the number of finite values it was built from (non-finite values
  are excluded from the distributions and reported instead);
- `all_finite` and the full `ValidationReport`;
- `attributes` from the contract and `owned` buffer sizes.

The size of the report does not depend on the number of points: inspecting 500 000 gaussians
returns the same shape as inspecting three, and no point array is serialized. The MCP
`splat_info` tool uses the bridge's `document.inspect` method to inspect the displayed
document *where it lives*, so an inspection never transfers a PLY to the MCP server; a
requested sample of points, or a `.ply` path, still reads the file.

## Viewer transform

The PlayCanvas viewer is Y-up, so `ui/viewer.js` rotates an imported splat by
`VIEWER_X_FLIP_DEGREES` (180°) about X when it attaches the PLY to the scene:

```text
viewer = flip_about_x(document):  (x, y, z) -> (x, -y, -z)
```

The flip is its own inverse, so the same formula converts both ways
(`contract::to_viewer_space` / `from_viewer_space`), and it is applied **exactly once** — a
model that is already Y-up must not be flipped again.

## Fixtures

`crates/splatmcp-core/src/fixtures.rs` holds the asymmetric models that make a mistake
visible:

- `axis_fixture()`: three short arrows along `+X` (red), `+Y` (green) and `+Z` (blue), each a
  chain of tapering gaussians, plus one grey marker further along `+Y`. `labelled_axis` reads
  the axis from the colour, `positioned_axis` from the position, and
  `labels_match_positions` asserts they agree — so a swap or a double flip is a reported
  mismatch rather than a judgement call.
- `rotated_gaussian()`: one non-spherical gaussian (radii `0.3, 0.1, 0.05 m`) turned 90°
  about Z, so its longest axis points along document `+Y`. Comparing covariance after a PLY
  round trip proves the rotation survived.

## Compatibility and migration notes

Version 1 records conventions the core already implemented (metres, activated scale and
opacity, `(w, x, y, z)`, linear RGB, SH degree 0, the 180° X viewer flip). What changed with
this task:

1. **Explicit point input is now validated before clamping.** An MCP `create_splat` with
   `points`, or an `edit_splat` `merge`, that carries a non-finite value, a zero radius, a
   colour/opacity outside `0..=1` (beyond the `1e-3` round-trip tolerance) or a degenerate
   quaternion now fails with an indexed message instead of being silently clamped. In-range
   values behave exactly as before, and a non-unit but usable quaternion is still normalised.
2. **PLY import is strict by default and reports what it did.** `read_ply` now refuses a file
   that needs repair instead of repairing it in silence; `read_ply_with_policy` returns the
   report, and `repair: true` (or `PlyImportPolicy::Repair`) is the explicit opt-in. A file
   that an earlier build loaded *and repaired* now fails until the caller asks for repair,
   which is the point: the repair becomes a decision the caller makes and sees.
3. **A `NaN` opacity logit is an import error** (previously it entered the document as a
   `NaN` opacity and failed later, on write). Infinite logits remain valid endpoints.
4. **`splat_info` returns bounded metadata.** The reply gained an `inspection` object and, on
   the displayed document, is answered from the app's own state. Every field it had before is
   still there with the same meaning. If the app cannot serve `document.inspect` (an older
   build), the tool falls back to reading the PLY, which is what it did before.
5. **`Splat::stats()` and `Splat::inspection()` share one implementation**, so the compact
   summary and the diagnostic one can never disagree.

Out of scope: spherical harmonics above degree 0, normals as model data, and SPZ (an explicit
earlier decision — PLY is the only file format).

## Evidence

Recorded for task #11, run on the development machine:

```sh
tools\cargo_env.cmd test -p splatmcp-core     # 81 unit + 8 contract + 3 interop, 0 failed
tools\cargo_env.cmd test -p splatmcp-bridge   # 20 unit + 8 round-trip, 0 failed
tools\cargo_env.cmd test -p splatmcp-mcp      # 54 unit + 1 app-link, 0 failed
tools\cargo_env.cmd test -p splatmcp          # 24 app tests, 0 failed
tools\cargo_env.cmd clippy -p splatmcp-core --all-targets
```

The core contract test suite covers the acceptance criteria that can be checked without a
GPU: the labelled axis fixture and the rotated non-spherical gaussian through
core → write → read (including covariance orientation), activated scale/opacity/RGB round
trips with tolerances and endpoints, indexed diagnostics from every core entry point, and a
500 000 gaussian inspection whose reply stays bounded.

Deliberately **not** run in this task: a native PlayCanvas capture (needs the app built and a
window; the transform itself is pinned by `contract::to_viewer_space`, the fixture labels and
`ui/viewer.js`), the installer build, and Adashi QA jobs.

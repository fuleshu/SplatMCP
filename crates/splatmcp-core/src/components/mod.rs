//! Named components, stable point identities, selections and local transforms.
//!
//! Task #14 in one module: a document is still a flat gaussian buffer, and everything that
//! describes *which* gaussians belong together lives beside it, not inside it.
//!
//! # Why membership is separate from the buffer
//!
//! [`crate::Splat`] stays a plain array of gaussians, so PLY import/export, edits and the
//! Python bindings keep working on it unchanged. An [`AuthoringSet`] holds the authoring
//! layer for one document revision:
//!
//! - a **stable point identity** per row ([`PointId`]), minted once and never reused, so a
//!   saved selection cannot be redirected by a row shift;
//! - **components** ([`Component`]) with an opaque [`ComponentId`], an editable display name
//!   (names are *not* identities), optional authoring metadata and explicit membership.
//!
//! # Identity behaviour
//!
//! | event | identity rule |
//! |-------|----------------|
//! | new points (merge, duplicate, recipe output) | new `PointId`s |
//! | surviving points (translate, rotate, colour, ...) | keep their `PointId` |
//! | removed points | their `PointId` is retired and never re-issued |
//! | component replaced | the replacement's points are new ids; other components are untouched |
//! | cross-document import | ids are re-minted from the importing document's namespace |
//!
//! # Local frames and anisotropic gaussians
//!
//! A [`LocalTransform`] is translation, rotation and positive per-axis scale (`A = R · S`).
//! Rotating or scaling a gaussian is *not* "multiply the radii": the covariance
//! `C = R_g · diag(scale²) · R_gᵀ` is transformed as `C' = A · C · Aᵀ` and then decomposed back
//! into a valid scale/orientation pair, which is the only way an anisotropic, rotated gaussian
//! keeps its shape. Reflections and singular transforms are refused rather than repaired.
//!
//! # Selections
//!
//! A [`SelectionQuery`] composes, deterministically and in a fixed order: component membership,
//! explicit point ids, spatial predicates in a chosen [`Frame`] (box inside/outside, sphere) and
//! the attribute filters the edit layer already had. Result rows are always ascending; box and
//! sphere boundaries are inclusive. A resolved selection is stored as a [`SelectionHandle`]
//! bound to one document revision, so a later request can act on exactly the points that were
//! selected - and a stale handle is detectable instead of silently matching other points.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};

mod transform;

pub use transform::{LocalTransform, decompose_covariance};

use crate::document::DocumentId;
use crate::edit::Box3;
use crate::{Bounds, Splat};

/// Largest number of point ids a selection handle retains before it stops growing.
///
/// A handle that exceeds it reports `truncated`, so a caller learns that it must narrow the
/// query instead of receiving an unbounded list.
pub const MAX_HANDLE_IDS: usize = 200_000;

/// Ids echoed in a selection reply by default.
pub const SELECTION_SAMPLE: usize = 32;

/// Handles kept per process before the oldest is evicted.
pub const MAX_SELECTION_HANDLES: usize = 32;

/// Opaque identity of one component.
///
/// Names are editable and not identities; this is the value a caller stores and quotes back.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(String);

impl ComponentId {
    /// Mints the `serial`-th component identity of a document.
    pub fn mint(serial: u64) -> Self {
        Self(format!("cmp-{serial}"))
    }

    /// Reads an identity back from text, or `None` when it is not one.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("cmp-")?;
        if rest.is_empty() || !rest.chars().all(|character| character.is_ascii_digit()) {
            return None;
        }
        Some(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ComponentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Opaque identity of one gaussian, stable across edits of the document.
///
/// Never reused: a retired id does not name a different row later, so an old selection fails
/// explicitly instead of silently matching new geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PointId(u64);

impl PointId {
    /// Wraps an explicit serial, for tests and for a decoded record.
    pub fn new(serial: u64) -> Self {
        Self(serial)
    }

    /// Reads an identity back from text (`pt-7`), or `None` when it is not one.
    ///
    /// The round trip matters at every boundary: an id that travelled through JSON must come
    /// back as *that* point or not at all, never as a row index that happens to parse.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("pt-")?;
        if rest.is_empty() || !rest.chars().all(|character| character.is_ascii_digit()) {
            return None;
        }
        rest.parse::<u64>().ok().map(Self)
    }

    /// Mints the next identity of this process.
    ///
    /// The counter is process-wide on purpose: two documents therefore never mint the same
    /// identity, so importing a document's geometry cannot inherit its ids by accident.
    fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::SeqCst))
    }

    pub fn serial(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for PointId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "pt-{}", self.0)
    }
}

/// Why an authoring operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// The request itself does not describe something that can be evaluated.
    Invalid(String),
    /// The named component is not in this document's authoring set.
    UnknownComponent(String),
    /// The named point is not in this document's authoring set.
    UnknownPoint(String),
    /// The transform is not in the supported class (singular, reflecting or degenerate).
    UnsupportedTransform(String),
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::UnknownComponent(id) => write!(formatter, "unknown component '{id}'"),
            Self::UnknownPoint(id) => write!(formatter, "unknown point '{id}'"),
            Self::UnsupportedTransform(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for SelectionError {}

impl SelectionError {
    /// Stable machine readable code, for structured replies.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_selection",
            Self::UnknownComponent(_) => "unknown_component",
            Self::UnknownPoint(_) => "unknown_point",
            Self::UnsupportedTransform(_) => "unsupported_transform",
        }
    }
}

/// The coordinate frame a spatial predicate is evaluated in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Frame {
    /// Document space: predicates compare gaussian positions as stored.
    #[default]
    World,
    /// The frame of the queried component: positions are mapped through the inverse of its
    /// local transform before the predicate is evaluated.
    Local,
}

/// A sphere predicate, in metres, with an inclusive boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sphere {
    pub center: [f32; 3],
    pub radius: f32,
}

impl Sphere {
    pub fn new(center: [f32; 3], radius: f32) -> Self {
        Self { center, radius }
    }

    pub fn contains(&self, point: [f32; 3]) -> bool {
        let distance: f32 = (0..3)
            .map(|axis| (point[axis] - self.center[axis]).powi(2))
            .sum();
        distance <= self.radius * self.radius
    }

    pub fn is_finite(&self) -> bool {
        self.center.iter().all(|value| value.is_finite()) && self.radius.is_finite()
    }
}

/// One named group of gaussians.
#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    /// Opaque identity; stable for the life of the document.
    pub id: ComponentId,
    /// Editable display name. Two components may share a name.
    pub name: String,
    /// Explicit local frame, when this component is edited in its own coordinates.
    pub transform: Option<LocalTransform>,
    /// Free-form authoring metadata (a recipe name, a note); never parsed here.
    pub metadata: Option<String>,
    /// Member gaussians, by stable identity, ascending.
    pub point_ids: Vec<PointId>,
}

impl Component {
    pub fn len(&self) -> usize {
        self.point_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.point_ids.is_empty()
    }
}

/// The authoring layer of one document revision: point identities and components.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthoringSet {
    /// Document this layer describes; `None` for geometry that has no document yet.
    pub document: Option<DocumentId>,
    /// Revision the membership was resolved against.
    pub revision: u64,
    ids: Vec<PointId>,
    components: Vec<Component>,
}

impl AuthoringSet {
    /// A fresh layer for a document revision holding `points` ungrouped gaussians.
    ///
    /// Every row gets a new identity: this is the "new document" case, where nothing can be
    /// inherited from another document, which is what keeps cross-document imports from
    /// reusing ids by accident.
    pub fn new(document: Option<DocumentId>, revision: u64, points: usize) -> Self {
        let mut set = Self {
            document,
            revision,
            ids: Vec::with_capacity(points),
            components: Vec::new(),
        };
        let ids = set.mint_points(points);
        set.ids = ids;
        set
    }

    /// An identity-free layer, for geometry with no authoring metadata at all.
    pub fn ungrouped(document: Option<DocumentId>, revision: u64, points: usize) -> Self {
        Self::new(document, revision, points)
    }

    /// Mints `count` fresh point identities.
    pub fn mint_points(&mut self, count: usize) -> Vec<PointId> {
        (0..count).map(|_| PointId::mint()).collect()
    }

    /// Mints a fresh component identity.
    pub fn mint_component(&mut self, name: impl Into<String>) -> ComponentId {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = ComponentId::mint(NEXT.fetch_add(1, Ordering::SeqCst));
        self.components.push(Component {
            id: id.clone(),
            name: name.into(),
            transform: None,
            metadata: None,
            point_ids: Vec::new(),
        });
        id
    }

    /// Identities of every row, in row order.
    pub fn ids(&self) -> &[PointId] {
        &self.ids
    }

    /// Replaces the row/identity mapping after a revision was produced.
    pub fn set_rows(&mut self, revision: u64, ids: Vec<PointId>) {
        self.revision = revision;
        self.ids = ids;
    }

    /// Row of a stable identity, when the point still exists.
    pub fn row_of(&self, id: PointId) -> Option<usize> {
        self.ids.iter().position(|candidate| *candidate == id)
    }

    /// Identity of a row.
    pub fn id_of(&self, row: usize) -> Option<PointId> {
        self.ids.get(row).copied()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn components(&self) -> &[Component] {
        &self.components
    }

    /// The named component, when it exists.
    pub fn component(&self, id: &ComponentId) -> Option<&Component> {
        self.components.iter().find(|component| &component.id == id)
    }

    /// Mutable access to one component, for the app-side editor.
    pub fn component_mut(&mut self, id: &ComponentId) -> Option<&mut Component> {
        self.components
            .iter_mut()
            .find(|component| &component.id == id)
    }

    /// Renames a component. Names are not identities, so this never changes the id.
    pub fn rename_component(
        &mut self,
        id: &ComponentId,
        name: impl Into<String>,
    ) -> Result<(), SelectionError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(SelectionError::Invalid(
                "a component name must not be blank".to_owned(),
            ));
        }
        let component = self
            .component_mut(id)
            .ok_or_else(|| SelectionError::UnknownComponent(id.to_string()))?;
        component.name = name;
        Ok(())
    }

    /// Sets a component's explicit local frame.
    pub fn set_component_transform(
        &mut self,
        id: &ComponentId,
        transform: Option<LocalTransform>,
    ) -> Result<(), SelectionError> {
        let transform = match transform {
            Some(transform) => Some(transform.validate()?),
            None => None,
        };
        let component = self
            .component_mut(id)
            .ok_or_else(|| SelectionError::UnknownComponent(id.to_string()))?;
        component.transform = transform;
        Ok(())
    }

    /// Declares a component's members. Every id must be a live point of this document, and
    /// declaring membership does not move or copy any geometry.
    pub fn set_membership(
        &mut self,
        id: &ComponentId,
        point_ids: &[PointId],
    ) -> Result<(), SelectionError> {
        for point in point_ids {
            if self.row_of(*point).is_none() {
                return Err(SelectionError::UnknownPoint(point.to_string()));
            }
        }
        let mut sorted = point_ids.to_vec();
        sorted.sort();
        sorted.dedup();
        let component = self
            .component_mut(id)
            .ok_or_else(|| SelectionError::UnknownComponent(id.to_string()))?;
        component.point_ids = sorted;
        Ok(())
    }

    /// Removes a component. Its points and their identities survive; only the grouping goes.
    pub fn remove_component(&mut self, id: &ComponentId) -> Option<Component> {
        let index = self
            .components
            .iter()
            .position(|component| &component.id == id)?;
        Some(self.components.remove(index))
    }

    /// Retires the given rows from the authoring layer: identities disappear (never reused)
    /// and membership is cleaned up so no component points at a removed row.
    pub fn remove_rows(&mut self, removed: &[PointId]) {
        self.ids.retain(|id| !removed.contains(id));
        for component in &mut self.components {
            component.point_ids.retain(|id| !removed.contains(id));
        }
    }

    /// Appends rows produced by an append (merge, duplicate or a recipe) to the mapping.
    pub fn append_rows(&mut self, ids: Vec<PointId>) {
        self.ids.extend(ids);
    }

    /// Transforms the member gaussians of a component through its own local frame.
    ///
    /// The frame is applied to each member's *world* position and covariance, so the stored
    /// geometry keeps one coordinate system and a component with a transform is still a plain
    /// set of document-space gaussians.
    pub fn apply_component_transform(
        &self,
        splat: &mut Splat,
        id: &ComponentId,
    ) -> Result<usize, SelectionError> {
        let component = self
            .component(id)
            .ok_or_else(|| SelectionError::UnknownComponent(id.to_string()))?;
        let Some(transform) = component.transform else {
            return Ok(0);
        };
        let transform = transform.validate()?;
        let mut affected = 0;
        for point in &component.point_ids {
            let Some(row) = self.row_of(*point) else {
                continue;
            };
            if row >= splat.len() {
                continue;
            }
            splat.points[row] = transform.apply_point(&splat.points[row])?;
            affected += 1;
        }
        Ok(affected)
    }

    /// Re-mints every identity, for a cross-document import.
    ///
    /// Returns a summary of what moved, so a caller can report it instead of silently
    /// accepting ids that mean something else in the importing document.
    pub fn reimport(&mut self, document: Option<DocumentId>, revision: u64) -> ImportSummary {
        let points = self.ids.len();
        let components = self.components.len();
        let mut fresh = AuthoringSet::new(document, revision, points);
        // Old row -> new row: the mapping is positional because the geometry is unchanged,
        // but no identity survives, which is the point of an import.
        let mapping: std::collections::HashMap<PointId, PointId> = self
            .ids
            .iter()
            .copied()
            .zip(fresh.ids.iter().copied())
            .collect();
        for component in &self.components {
            let id = fresh.mint_component(component.name.clone());
            let mut members: Vec<PointId> = component
                .point_ids
                .iter()
                .filter_map(|old| mapping.get(old).copied())
                .collect();
            members.sort();
            if let Some(created) = fresh.component_mut(&id) {
                created.transform = component.transform;
                created.metadata = component.metadata.clone();
                created.point_ids = members;
            }
        }
        *self = fresh;
        ImportSummary {
            points,
            components,
            remapped: true,
        }
    }
}

/// What a cross-document import did to point identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportSummary {
    pub points: usize,
    pub components: usize,
    /// True when identities were re-minted rather than inherited.
    pub remapped: bool,
}

/// A selection request: the composition of every predicate a caller may combine.
///
/// Composition is fixed and documented: start from all rows, then component membership, then
/// explicit point ids, then spatial predicates, then attribute filters, and finally `first`
/// keeps the lowest-numbered rows. Every predicate narrows; nothing widens.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SelectionQuery {
    /// Restrict to a component's members.
    pub component: Option<ComponentId>,
    /// Restrict to these exact gaussians, by stable identity.
    pub point_ids: Vec<PointId>,
    /// Keep points inside this box (inclusive).
    pub within: Option<Box3>,
    /// Keep points outside this box.
    pub outside: Option<Box3>,
    /// Keep points inside this sphere (inclusive).
    pub sphere: Option<Sphere>,
    /// Frame the box/sphere predicates are evaluated in. `Local` needs `component`.
    pub frame: Frame,
    /// Per-channel minimum mean colour.
    pub color_min: Option<[f32; 3]>,
    /// Per-channel maximum mean colour.
    pub color_max: Option<[f32; 3]>,
    /// Minimum opacity.
    pub opacity_min: Option<f32>,
    /// Maximum largest radius.
    pub max_radius: Option<f32>,
    /// Keep the first N matching rows.
    pub first: Option<usize>,
}

impl SelectionQuery {
    /// A query that selects every point.
    pub fn all() -> Self {
        Self::default()
    }

    /// True when no predicate narrows anything.
    pub fn is_all(&self) -> bool {
        self.component.is_none()
            && self.point_ids.is_empty()
            && self.within.is_none()
            && self.outside.is_none()
            && self.sphere.is_none()
            && self.color_min.is_none()
            && self.color_max.is_none()
            && self.opacity_min.is_none()
            && self.max_radius.is_none()
            && self.first.is_none()
    }

    /// Checks the query itself, before any point is looked at.
    pub fn validate(&self) -> Result<(), SelectionError> {
        for (name, box3) in [("within", self.within), ("outside", self.outside)] {
            if let Some(box3) = box3 {
                if !box3.is_finite() {
                    return Err(SelectionError::Invalid(format!(
                        "{name} must hold finite corner values"
                    )));
                }
            }
        }
        if let Some(sphere) = self.sphere {
            if !sphere.is_finite() || sphere.radius < 0.0 {
                return Err(SelectionError::Invalid(
                    "sphere must hold a finite centre and a non-negative radius".to_owned(),
                ));
            }
        }
        for (name, color) in [("color_min", self.color_min), ("color_max", self.color_max)] {
            if let Some(color) = color {
                if color.iter().any(|value| !value.is_finite()) {
                    return Err(SelectionError::Invalid(format!("{name} must be finite")));
                }
            }
        }
        if let Some(opacity) = self.opacity_min {
            if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
                return Err(SelectionError::Invalid(
                    "opacity_min must be in 0..=1".to_owned(),
                ));
            }
        }
        if let Some(radius) = self.max_radius {
            if !radius.is_finite() || radius <= 0.0 {
                return Err(SelectionError::Invalid(
                    "max_radius must be a positive number".to_owned(),
                ));
            }
        }
        if self.frame == Frame::Local && self.component.is_none() {
            return Err(SelectionError::Invalid(
                "a local-frame query needs a component to take the frame from".to_owned(),
            ));
        }
        Ok(())
    }

    /// Resolves the query to ascending point rows.
    pub fn resolve(
        &self,
        splat: &Splat,
        authoring: &AuthoringSet,
    ) -> Result<Vec<usize>, SelectionError> {
        self.validate()?;
        let frame = self.frame_of(authoring)?;

        let mut rows: Vec<usize> = if let Some(component_id) = &self.component {
            let component = authoring
                .component(component_id)
                .ok_or_else(|| SelectionError::UnknownComponent(component_id.to_string()))?;
            component
                .point_ids
                .iter()
                .filter_map(|id| authoring.row_of(*id))
                .filter(|row| *row < splat.len())
                .collect()
        } else {
            (0..splat.len()).collect()
        };
        rows.sort_unstable();
        rows.dedup();

        if !self.point_ids.is_empty() {
            let mut wanted: Vec<usize> = Vec::with_capacity(self.point_ids.len());
            for id in &self.point_ids {
                match authoring.row_of(*id) {
                    Some(row) if row < splat.len() => wanted.push(row),
                    _ => return Err(SelectionError::UnknownPoint(id.to_string())),
                }
            }
            wanted.sort_unstable();
            wanted.dedup();
            rows.retain(|row| wanted.contains(row));
        }

        rows.retain(|row| {
            let point = &splat.points[*row];
            let position = match frame {
                Some(transform) => match transform.to_local(point.position) {
                    Ok(local) => local,
                    Err(_) => return false,
                },
                None => point.position,
            };
            if let Some(within) = self.within {
                if !within.contains(position) {
                    return false;
                }
            }
            if let Some(outside) = self.outside {
                if outside.contains(position) {
                    return false;
                }
            }
            if let Some(sphere) = self.sphere {
                if !sphere.contains(position) {
                    return false;
                }
            }
            if let Some(min) = self.color_min {
                if (0..3).any(|axis| point.color[axis] < min[axis]) {
                    return false;
                }
            }
            if let Some(max) = self.color_max {
                if (0..3).any(|axis| point.color[axis] > max[axis]) {
                    return false;
                }
            }
            if let Some(min) = self.opacity_min {
                if point.opacity < min {
                    return false;
                }
            }
            if let Some(max) = self.max_radius {
                if point
                    .scale
                    .iter()
                    .fold(0.0f32, |acc, value| acc.max(*value))
                    > max
                {
                    return false;
                }
            }
            true
        });

        if let Some(limit) = self.first {
            rows.truncate(limit);
        }
        Ok(rows)
    }

    /// The transform a local-frame query evaluates in, when one is needed.
    fn frame_of(&self, authoring: &AuthoringSet) -> Result<Option<LocalTransform>, SelectionError> {
        if self.frame != Frame::Local {
            return Ok(None);
        }
        let component_id = self
            .component
            .as_ref()
            .ok_or_else(|| SelectionError::Invalid("a local frame needs a component".to_owned()))?;
        let component = authoring
            .component(component_id)
            .ok_or_else(|| SelectionError::UnknownComponent(component_id.to_string()))?;
        // A component without an explicit frame *is* its own frame for querying, with the
        // identity transform: that keeps "select locally" meaningful without inventing one.
        match component.transform {
            Some(transform) => Ok(Some(transform.validate()?)),
            None => Ok(Some(LocalTransform::identity())),
        }
    }
}

/// A resolved selection, bound to the revision it was resolved against.
///
/// The point ids are resolved *once*: a later operation on this handle acts on exactly these
/// gaussians, and if the document moved on the handle still names its own revision, so a
/// caller can compare and refuse instead of editing shifted rows.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionHandle {
    pub id: u64,
    pub document: Option<DocumentId>,
    /// Revision the selection was resolved against.
    pub revision: u64,
    pub count: usize,
    /// Bounds of the selected gaussians.
    pub bounds: Option<Bounds>,
    /// A bounded sample of the selected ids, for a reply.
    pub sample: Vec<PointId>,
    /// True when more ids exist than the handle retains.
    pub truncated: bool,
    ids: Vec<PointId>,
}

impl SelectionHandle {
    /// Every selected identity, in ascending row order at resolution time.
    pub fn ids(&self) -> &[PointId] {
        &self.ids
    }

    /// Builds a handle from resolved rows.
    fn from_rows(
        id: u64,
        document: Option<DocumentId>,
        revision: u64,
        splat: &Splat,
        authoring: &AuthoringSet,
        rows: &[usize],
    ) -> Self {
        let truncated = rows.len() > MAX_HANDLE_IDS;
        let ids: Vec<PointId> = rows
            .iter()
            .take(MAX_HANDLE_IDS)
            .filter_map(|row| authoring.id_of(*row))
            .collect();
        let sample = ids.iter().take(SELECTION_SAMPLE).copied().collect();
        let bounds = bounds_of_rows(splat, rows);
        Self {
            id,
            document,
            revision,
            count: rows.len(),
            bounds,
            sample,
            truncated,
            ids,
        }
    }
}

/// Bounds of a set of rows, padded by each gaussian's largest radius like [`Splat::bounds`].
fn bounds_of_rows(splat: &Splat, rows: &[usize]) -> Option<Bounds> {
    let first = rows.first().and_then(|row| splat.points.get(*row))?;
    let mut min = first.position;
    let mut max = first.position;
    for row in rows {
        let Some(point) = splat.points.get(*row) else {
            continue;
        };
        let pad = point.scale.into_iter().fold(0.0f32, f32::max);
        for axis in 0..3 {
            min[axis] = min[axis].min(point.position[axis] - pad);
            max[axis] = max[axis].max(point.position[axis] + pad);
        }
    }
    let center = [
        (min[0] + max[0]) * 0.5,
        (min[1] + max[1]) * 0.5,
        (min[2] + max[2]) * 0.5,
    ];
    let radius = (max[0] - min[0]).max(max[1] - min[1]).max(max[2] - min[2]) * 0.5;
    Some(Bounds {
        min,
        max,
        center,
        radius: radius.max(0.0),
    })
}

/// Bounded retention of resolved selection handles.
#[derive(Debug, Default)]
pub struct SelectionHandles {
    handles: VecDeque<SelectionHandle>,
    next: u64,
    limit: usize,
}

impl SelectionHandles {
    /// A store keeping at most `limit` handles, oldest first.
    pub fn new(limit: usize) -> Self {
        Self {
            handles: VecDeque::new(),
            next: 1,
            limit: limit.max(1),
        }
    }

    /// Resolves a query and retains the result under a new handle id.
    pub fn capture(
        &mut self,
        splat: &Splat,
        authoring: &AuthoringSet,
        query: &SelectionQuery,
    ) -> Result<SelectionHandle, SelectionError> {
        let rows = query.resolve(splat, authoring)?;
        let id = self.next;
        self.next += 1;
        let handle = SelectionHandle::from_rows(
            id,
            authoring.document.clone(),
            authoring.revision,
            splat,
            authoring,
            &rows,
        );
        if self.handles.len() >= self.limit {
            self.handles.pop_front();
        }
        self.handles.push_back(handle.clone());
        Ok(handle)
    }

    /// The exact handle, when it is still retained.
    pub fn get(&self, id: u64) -> Option<&SelectionHandle> {
        self.handles.iter().find(|handle| handle.id == id)
    }

    /// How many handles are retained.
    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Drops handles resolved against a revision that is no longer current.
    ///
    /// A selection handle is a promise about exact gaussians, so it is not "re-resolved" after
    /// the document changes: it either still matches its revision or it is gone.
    pub fn drop_stale(&mut self, document: &DocumentId, revision: u64) {
        self.handles.retain(|handle| {
            handle.document.as_ref() != Some(document) || handle.revision == revision
        });
    }

    /// Forgets every handle of a document, e.g. when it is closed or evicted.
    pub fn forget_document(&mut self, document: &DocumentId) {
        self.handles
            .retain(|handle| handle.document.as_ref() != Some(document));
    }
}

#[cfg(test)]
mod tests;

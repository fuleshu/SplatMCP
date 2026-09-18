//! Unit tests for [`super`], kept beside the module they exercise so the
//! implementation files stay readable.

use super::*;
use crate::SplatPoint;
use crate::contract::IDENTITY_QUATERNION;

fn point(x: f32) -> SplatPoint {
    SplatPoint::new([x, 0.0, 0.0], [0.1; 3], [0.5; 3], 0.8, IDENTITY_QUATERNION)
}

fn splat(count: usize) -> Splat {
    Splat::from_points((0..count).map(|index| point(index as f32)).collect())
}

fn authoring(count: usize) -> AuthoringSet {
    AuthoringSet::new(Some(DocumentId::mint(1, 1)), 1, count)
}

#[test]
fn ids_are_opaque_and_parse_only_their_own_format() {
    let component = ComponentId::mint(3);
    assert_eq!(component.as_str(), "cmp-3");
    assert_eq!(ComponentId::parse("cmp-3"), Some(component));
    assert_eq!(ComponentId::parse("roof"), None);
    assert_eq!(ComponentId::parse("cmp-"), None);
    assert_eq!(PointId::new(7).to_string(), "pt-7");
}

#[test]
fn a_new_layer_mints_one_identity_per_row_and_keeps_them_stable() {
    let mut set = authoring(4);
    assert_eq!(set.len(), 4);
    let ids = set.ids().to_vec();
    assert_eq!(set.row_of(ids[2]), Some(2));
    assert_eq!(set.id_of(0), Some(ids[0]));

    // Removing a row retires its identity; the survivors keep theirs.
    set.remove_rows(&[ids[1]]);
    assert_eq!(set.len(), 3);
    assert_eq!(set.row_of(ids[1]), None, "a retired id must not resolve");
    assert_eq!(set.row_of(ids[2]), Some(1));
    assert_eq!(set.id_of(1), Some(ids[2]));

    // New geometry gets new ids, never a retired one.
    let fresh = set.mint_points(2);
    assert!(!ids.contains(&fresh[0]));
    set.append_rows(fresh.clone());
    assert_eq!(set.row_of(fresh[0]), Some(3));
}

#[test]
fn membership_is_separate_from_the_buffer_and_survives_a_rename() {
    let mut set = authoring(6);
    let ids = set.ids().to_vec();
    let hair = set.mint_component("hair");
    let face = set.mint_component("face");
    set.set_membership(&hair, &ids[0..2]).unwrap();
    set.set_membership(&face, &ids[2..4]).unwrap();

    set.rename_component(&hair, "hair-back").unwrap();
    assert_eq!(set.component(&hair).unwrap().name, "hair-back");
    assert_eq!(set.component(&hair).unwrap().id, hair);
    assert_eq!(set.component(&face).unwrap().len(), 2);

    // Removing a component keeps every point and its identity.
    let removed = set.remove_component(&hair).unwrap();
    assert_eq!(removed.point_ids.len(), 2);
    assert_eq!(set.len(), 6);
    set.set_membership(&face, &ids[0..4]).unwrap();
    assert_eq!(set.component(&face).unwrap().len(), 4);

    // Membership cannot point at a point that does not exist.
    let foreign = PointId::new(9_999);
    assert_eq!(
        set.set_membership(&face, &[foreign]).unwrap_err().code(),
        "unknown_point"
    );
    assert_eq!(
        set.rename_component(&ComponentId::mint(99), "x")
            .unwrap_err()
            .code(),
        "unknown_component"
    );
}

#[test]
fn removing_rows_cleans_membership_so_a_component_cannot_point_at_nothing() {
    let mut set = authoring(4);
    let ids = set.ids().to_vec();
    let component = set.mint_component("sleeve");
    set.set_membership(&component, &ids[1..3]).unwrap();
    set.remove_rows(&[ids[2]]);
    assert_eq!(set.component(&component).unwrap().point_ids, vec![ids[1]]);
    assert_eq!(set.row_of(ids[2]), None);
}

#[test]
fn a_cross_document_import_re_mints_every_identity() {
    let mut set = authoring(3);
    let old = set.ids().to_vec();
    let component = set.mint_component("hair");
    set.set_membership(&component, &old[0..2]).unwrap();

    let summary = set.reimport(Some(DocumentId::mint(2, 1)), 1);
    assert!(summary.remapped);
    assert_eq!(summary.points, 3);
    assert_eq!(summary.components, 1);
    assert!(
        set.ids().iter().all(|id| !old.contains(id)),
        "an import must not inherit another document's ids"
    );
    let imported = set.components()[0].clone();
    assert_eq!(imported.name, "hair");
    assert_eq!(imported.len(), 2);
    assert!(
        imported
            .point_ids
            .iter()
            .all(|id| set.row_of(*id).is_some())
    );
}

#[test]
fn a_query_composes_component_ids_spatial_and_attribute_filters() {
    let splat = Splat::from_points(vec![
        SplatPoint::new(
            [0.0; 3],
            [0.1; 3],
            [1.0, 0.0, 0.0],
            1.0,
            IDENTITY_QUATERNION,
        ),
        SplatPoint::new(
            [1.0, 0.0, 0.0],
            [0.1; 3],
            [0.0, 1.0, 0.0],
            0.2,
            IDENTITY_QUATERNION,
        ),
        SplatPoint::new(
            [2.0, 0.0, 0.0],
            [0.5; 3],
            [0.0, 0.0, 1.0],
            0.9,
            IDENTITY_QUATERNION,
        ),
        SplatPoint::new(
            [3.0, 0.0, 0.0],
            [0.1; 3],
            [0.0, 1.0, 0.0],
            0.9,
            IDENTITY_QUATERNION,
        ),
    ]);
    let mut set = authoring(4);
    let ids = set.ids().to_vec();
    let group = set.mint_component("middle");
    set.set_membership(&group, &ids[1..4]).unwrap();

    // Component membership alone.
    let query = SelectionQuery {
        component: Some(group.clone()),
        ..SelectionQuery::default()
    };
    assert_eq!(query.resolve(&splat, &set).unwrap(), vec![1, 2, 3]);

    // Component AND an explicit id AND an opacity filter: every predicate narrows.
    let query = SelectionQuery {
        component: Some(group.clone()),
        point_ids: vec![ids[1], ids[3]],
        opacity_min: Some(0.5),
        ..SelectionQuery::default()
    };
    assert_eq!(
        query.resolve(&splat, &set).unwrap(),
        vec![3],
        "only the opaque member of the requested pair survives"
    );

    // A box is inclusive on its boundary.
    let query = SelectionQuery {
        within: Some(Box3::from_corners([1.0, -1.0, -1.0], [2.0, 1.0, 1.0])),
        ..SelectionQuery::default()
    };
    assert_eq!(query.resolve(&splat, &set).unwrap(), vec![1, 2]);

    // `first` keeps the lowest rows of whatever matched.
    let query = SelectionQuery {
        opacity_min: Some(0.5),
        first: Some(1),
        ..SelectionQuery::default()
    };
    assert_eq!(query.resolve(&splat, &set).unwrap(), vec![0]);

    // A query naming an unknown point fails instead of silently matching others.
    let query = SelectionQuery {
        point_ids: vec![PointId::new(9_999)],
        ..SelectionQuery::default()
    };
    assert_eq!(
        query.resolve(&splat, &set).unwrap_err().code(),
        "unknown_point"
    );
}

#[test]
fn a_local_frame_query_evaluates_in_the_components_coordinates() {
    // Geometry sits at x = 10..12; the component's frame moves its own origin to x = 10.
    let splat = Splat::from_points(vec![
        SplatPoint::new(
            [10.0, 0.0, 0.0],
            [0.1; 3],
            [0.5; 3],
            1.0,
            IDENTITY_QUATERNION,
        ),
        SplatPoint::new(
            [12.0, 0.0, 0.0],
            [0.1; 3],
            [0.5; 3],
            1.0,
            IDENTITY_QUATERNION,
        ),
    ]);
    let mut set = authoring(2);
    let component = set.mint_component("local");
    set.set_membership(&component, &set.ids().to_vec()).unwrap();
    set.set_component_transform(
        &component,
        Some(LocalTransform::translation([10.0, 0.0, 0.0])),
    )
    .unwrap();

    let query = SelectionQuery {
        component: Some(component.clone()),
        frame: Frame::Local,
        within: Some(Box3::from_corners([-0.5, -1.0, -1.0], [0.5, 1.0, 1.0])),
        ..SelectionQuery::default()
    };
    assert_eq!(query.resolve(&splat, &set).unwrap(), vec![0]);

    // The same box in world space selects the other end.
    let query = SelectionQuery {
        component: Some(component.clone()),
        frame: Frame::World,
        within: Some(Box3::from_corners([11.5, -1.0, -1.0], [12.5, 1.0, 1.0])),
        ..SelectionQuery::default()
    };
    assert_eq!(query.resolve(&splat, &set).unwrap(), vec![1]);

    // A local frame without a component cannot be evaluated.
    let query = SelectionQuery {
        frame: Frame::Local,
        ..SelectionQuery::default()
    };
    assert_eq!(
        query.resolve(&splat, &set).unwrap_err().code(),
        "invalid_selection"
    );
}

#[test]
fn a_sphere_is_inclusive_and_composes_with_colour() {
    let splat = Splat::from_points(vec![
        SplatPoint::new(
            [1.0, 0.0, 0.0],
            [0.1; 3],
            [1.0, 0.0, 0.0],
            1.0,
            IDENTITY_QUATERNION,
        ),
        SplatPoint::new(
            [3.0, 0.0, 0.0],
            [0.1; 3],
            [1.0, 0.0, 0.0],
            1.0,
            IDENTITY_QUATERNION,
        ),
    ]);
    let set = authoring(2);
    let query = SelectionQuery {
        sphere: Some(Sphere::new([0.0; 3], 1.0)),
        ..SelectionQuery::default()
    };
    assert_eq!(query.resolve(&splat, &set).unwrap(), vec![0]);

    let query = SelectionQuery {
        sphere: Some(Sphere::new([0.0; 3], 3.0)),
        color_min: Some([0.9, 0.0, 0.0]),
        ..SelectionQuery::default()
    };
    assert_eq!(query.resolve(&splat, &set).unwrap(), vec![0, 1]);
}

#[test]
fn a_selection_handle_is_revision_bound_bounded_and_evicts_oldest_first() {
    let splat = splat(5);
    let set = authoring(5);
    let mut handles = SelectionHandles::new(2);

    let all = handles
        .capture(&splat, &set, &SelectionQuery::all())
        .unwrap();
    assert_eq!(all.count, 5);
    assert_eq!(all.ids().len(), 5);
    assert_eq!(all.sample.len(), 5);
    assert!(!all.truncated);
    assert_eq!(all.revision, 1);
    assert!(all.bounds.is_some());

    let first = handles
        .capture(
            &splat,
            &set,
            &SelectionQuery {
                first: Some(2),
                ..SelectionQuery::default()
            },
        )
        .unwrap();
    assert_eq!(first.count, 2);
    assert_eq!(handles.len(), 2);

    let third = handles
        .capture(&splat, &set, &SelectionQuery::all())
        .unwrap();
    assert_eq!(handles.len(), 2, "retention is bounded");
    assert!(
        handles.get(all.id).is_none(),
        "the oldest handle is evicted"
    );
    assert!(handles.get(third.id).is_some());

    // A new revision invalidates handles resolved against the old one.
    handles.drop_stale(&DocumentId::mint(1, 1), 2);
    assert!(handles.is_empty());
}

#[test]
fn a_component_transform_moves_only_its_own_members() {
    let mut splat = splat(4);
    let mut set = authoring(4);
    let ids = set.ids().to_vec();
    let hair = set.mint_component("hair");
    let face = set.mint_component("face");
    set.set_membership(&hair, &ids[0..2]).unwrap();
    set.set_membership(&face, &ids[2..4]).unwrap();
    set.set_component_transform(&hair, Some(LocalTransform::translation([0.0, 5.0, 0.0])))
        .unwrap();

    assert_eq!(set.apply_component_transform(&mut splat, &hair).unwrap(), 2);
    assert_eq!(set.apply_component_transform(&mut splat, &face).unwrap(), 0);
    assert_eq!(splat.points[0].position[1], 5.0);
    assert_eq!(splat.points[1].position[1], 5.0);
    assert_eq!(splat.points[2].position[1], 0.0);
    assert_eq!(splat.points[3].position[1], 0.0);
    // The face members kept their identities and their numbers.
    assert_eq!(
        set.component(&face).unwrap().point_ids,
        vec![ids[2], ids[3]]
    );
}

#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::data::arena::Arena;
use crate::dynamics::{BodyStatus, Joint, JointSet, RigidBody, RigidBodyChanges};
use crate::geometry::{ColliderSet, InteractionGraph, NarrowPhase};
use parry::partitioning::IndexedData;
use std::ops::{Index, IndexMut};

/// The unique handle of a rigid body added to a `RigidBodySet`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[repr(transparent)]
pub struct RigidBodyHandle(pub(crate) crate::data::arena::Index);

impl RigidBodyHandle {
    /// Converts this handle into its (index, generation) components.
    pub fn into_raw_parts(self) -> (usize, u64) {
        self.0.into_raw_parts()
    }

    /// Reconstructs an handle from its (index, generation) components.
    pub fn from_raw_parts(id: usize, generation: u64) -> Self {
        Self(crate::data::arena::Index::from_raw_parts(id, generation))
    }

    /// An always-invalid rigid-body handle.
    pub fn invalid() -> Self {
        Self(crate::data::arena::Index::from_raw_parts(
            crate::INVALID_USIZE,
            crate::INVALID_U64,
        ))
    }
}

impl IndexedData for RigidBodyHandle {
    fn default() -> Self {
        Self(IndexedData::default())
    }

    fn index(&self) -> usize {
        self.0.index()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
/// A pair of rigid body handles.
pub struct BodyPair {
    /// The first rigid body handle.
    pub body1: RigidBodyHandle,
    /// The second rigid body handle.
    pub body2: RigidBodyHandle,
}

impl BodyPair {
    /// Builds a new pair of rigid-body handles.
    pub fn new(body1: RigidBodyHandle, body2: RigidBodyHandle) -> Self {
        BodyPair { body1, body2 }
    }
}

#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Clone)]
/// A set of rigid bodies that can be handled by a physics pipeline.
pub struct RigidBodySet {
    // FIXME: Could we avoid this?
    // NOTE: the pub(crate) are needed by the broad phase
    // to avoid borrowing issues. It is also needed for
    // parallelism because the `Receiver` breaks the Sync impl.
    pub(crate) bodies: Arena<RigidBody>,
    pub(crate) active_dynamic_set: Vec<RigidBodyHandle>,
    pub(crate) active_kinematic_set: Vec<RigidBodyHandle>,
    // Set of inactive bodies which have been modified.
    // This typically include static bodies which have been modified.
    pub(crate) modified_inactive_set: Vec<RigidBodyHandle>,
    pub(crate) active_islands: Vec<usize>,
    active_set_timestamp: u32,
    pub(crate) modified_bodies: Vec<RigidBodyHandle>,
    pub(crate) modified_all_bodies: bool,
    #[cfg_attr(feature = "serde-serialize", serde(skip))]
    can_sleep: Vec<RigidBodyHandle>, // Workspace.
    #[cfg_attr(feature = "serde-serialize", serde(skip))]
    stack: Vec<RigidBodyHandle>, // Workspace.
}

impl RigidBodySet {
    /// Create a new empty set of rigid bodies.
    pub fn new() -> Self {
        RigidBodySet {
            bodies: Arena::new(),
            active_dynamic_set: Vec::new(),
            active_kinematic_set: Vec::new(),
            modified_inactive_set: Vec::new(),
            active_islands: Vec::new(),
            active_set_timestamp: 0,
            modified_bodies: Vec::new(),
            modified_all_bodies: false,
            can_sleep: Vec::new(),
            stack: Vec::new(),
        }
    }

    /// The number of rigid bodies on this set.
    pub fn len(&self) -> usize {
        self.bodies.len()
    }

    /// `true` if there are no rigid bodies in this set.
    pub fn is_empty(&self) -> bool {
        self.bodies.is_empty()
    }

    /// Is the given body handle valid?
    pub fn contains(&self, handle: RigidBodyHandle) -> bool {
        self.bodies.contains(handle.0)
    }

    /// Insert a rigid body into this set and retrieve its handle.
    pub fn insert(&mut self, mut rb: RigidBody) -> RigidBodyHandle {
        // Make sure the internal links are reset, they may not be
        // if this rigid-body was obtained by cloning another one.
        rb.reset_internal_references();
        rb.changes.set(RigidBodyChanges::all(), true);

        let handle = RigidBodyHandle(self.bodies.insert(rb));
        self.modified_bodies.push(handle);

        let rb = &mut self.bodies[handle.0];

        if rb.is_kinematic() {
            rb.active_set_id = self.active_kinematic_set.len();
            self.active_kinematic_set.push(handle);
        }

        handle
    }

    /// Removes a rigid-body, and all its attached colliders and joints, from these sets.
    pub fn remove(
        &mut self,
        handle: RigidBodyHandle,
        colliders: &mut ColliderSet,
        joints: &mut JointSet,
    ) -> Option<RigidBody> {
        let rb = self.bodies.remove(handle.0)?;
        /*
         * Update active sets.
         */
        let mut active_sets = [&mut self.active_kinematic_set, &mut self.active_dynamic_set];

        for active_set in &mut active_sets {
            if active_set.get(rb.active_set_id) == Some(&handle) {
                active_set.swap_remove(rb.active_set_id);

                if let Some(replacement) = active_set.get(rb.active_set_id) {
                    self.bodies[replacement.0].active_set_id = rb.active_set_id;
                }
            }
        }

        /*
         * Remove colliders attached to this rigid-body.
         */
        for collider in &rb.colliders {
            colliders.remove(*collider, self, false);
        }

        /*
         * Remove joints attached to this rigid-body.
         */
        joints.remove_rigid_body(rb.joint_graph_index, self);

        Some(rb)
    }

    pub(crate) fn num_islands(&self) -> usize {
        self.active_islands.len() - 1
    }

    /// Forces the specified rigid-body to wake up if it is dynamic.
    ///
    /// If `strong` is `true` then it is assured that the rigid-body will
    /// remain awake during multiple subsequent timesteps.
    pub fn wake_up(&mut self, handle: RigidBodyHandle, strong: bool) {
        if let Some(rb) = self.bodies.get_mut(handle.0) {
            // TODO: what about kinematic bodies?
            if rb.is_dynamic() {
                rb.wake_up(strong);

                if self.active_dynamic_set.get(rb.active_set_id) != Some(&handle) {
                    rb.active_set_id = self.active_dynamic_set.len();
                    self.active_dynamic_set.push(handle);
                }
            }
        }
    }

    /// Gets the rigid-body with the given handle without a known generation.
    ///
    /// This is useful for finding the generation number when only the rigid-body position `i` is known.
    ///
    /// Generation numbers are used to protect from the ABA problem because the rigid-body position `i`
    /// is recycled between two insertions and a removal.
    ///
    /// Using this is discouraged in favor of `self.get(handle)` which does not
    /// suffer from the ABA problem.
    pub fn get_unknown_gen(&self, i: usize) -> Option<(&RigidBody, RigidBodyHandle)> {
        self.bodies
            .get_unknown_gen(i)
            .map(|(b, h)| (b, RigidBodyHandle(h)))
    }

    /// Gets a mutable reference to a rigid-body with the given handle without a known generation.
    ///
    /// This is useful for finding the generation number when only the rigid-body position `i` is known.
    ///
    /// Generation numbers are used to protect from the ABA problem because the rigid-body position `i`
    /// is recycled between two insertions and a removal.
    ///
    /// Using this is discouraged in favor of `self.get_mut(handle)` which does not
    /// suffer from the ABA problem.
    #[cfg(not(feature = "dev-remove-slow-accessors"))]
    pub fn get_unknown_gen_mut(&mut self, i: usize) -> Option<(&mut RigidBody, RigidBodyHandle)> {
        let (rb, handle) = self.bodies.get_unknown_gen_mut(i)?;
        let handle = RigidBodyHandle(handle);
        Self::mark_as_modified(
            handle,
            rb,
            &mut self.modified_bodies,
            self.modified_all_bodies,
        );
        Some((rb, handle))
    }

    /// Gets the rigid-body with the given handle.
    pub fn get(&self, handle: RigidBodyHandle) -> Option<&RigidBody> {
        self.bodies.get(handle.0)
    }

    fn mark_as_modified(
        handle: RigidBodyHandle,
        rb: &mut RigidBody,
        modified_bodies: &mut Vec<RigidBodyHandle>,
        modified_all_bodies: bool,
    ) {
        if !modified_all_bodies && !rb.changes.contains(RigidBodyChanges::MODIFIED) {
            rb.changes = RigidBodyChanges::MODIFIED;
            modified_bodies.push(handle);
        }
    }

    /// Gets a mutable reference to the rigid-body with the given handle.
    #[cfg(not(feature = "dev-remove-slow-accessors"))]
    pub fn get_mut(&mut self, handle: RigidBodyHandle) -> Option<&mut RigidBody> {
        let result = self.bodies.get_mut(handle.0)?;
        Self::mark_as_modified(
            handle,
            result,
            &mut self.modified_bodies,
            self.modified_all_bodies,
        );
        Some(result)
    }

    pub(crate) fn get_mut_internal(&mut self, handle: RigidBodyHandle) -> Option<&mut RigidBody> {
        self.bodies.get_mut(handle.0)
    }

    // Just a very long name instead of `.get_mut` to make sure
    // this is really the method we wanted to use instead of `get_mut_internal`.
    pub(crate) fn get_mut_internal_with_modification_tracking(
        &mut self,
        handle: RigidBodyHandle,
    ) -> Option<&mut RigidBody> {
        let result = self.bodies.get_mut(handle.0)?;
        Self::mark_as_modified(
            handle,
            result,
            &mut self.modified_bodies,
            self.modified_all_bodies,
        );
        Some(result)
    }

    pub(crate) fn get2_mut_internal(
        &mut self,
        h1: RigidBodyHandle,
        h2: RigidBodyHandle,
    ) -> (Option<&mut RigidBody>, Option<&mut RigidBody>) {
        self.bodies.get2_mut(h1.0, h2.0)
    }

    /// Iterates through all the rigid-bodies on this set.
    pub fn iter(&self) -> impl Iterator<Item = (RigidBodyHandle, &RigidBody)> {
        self.bodies.iter().map(|(h, b)| (RigidBodyHandle(h), b))
    }

    /// Iterates mutably through all the rigid-bodies on this set.
    #[cfg(not(feature = "dev-remove-slow-accessors"))]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (RigidBodyHandle, &mut RigidBody)> {
        self.modified_bodies.clear();
        self.modified_all_bodies = true;
        self.bodies.iter_mut().map(|(h, b)| (RigidBodyHandle(h), b))
    }

    /// Iter through all the active kinematic rigid-bodies on this set.
    pub fn iter_active_kinematic<'a>(
        &'a self,
    ) -> impl Iterator<Item = (RigidBodyHandle, &'a RigidBody)> {
        let bodies: &'a _ = &self.bodies;
        self.active_kinematic_set
            .iter()
            .filter_map(move |h| Some((*h, bodies.get(h.0)?)))
    }

    /// Iter through all the active dynamic rigid-bodies on this set.
    pub fn iter_active_dynamic<'a>(
        &'a self,
    ) -> impl Iterator<Item = (RigidBodyHandle, &'a RigidBody)> {
        let bodies: &'a _ = &self.bodies;
        self.active_dynamic_set
            .iter()
            .filter_map(move |h| Some((*h, bodies.get(h.0)?)))
    }

    #[cfg(not(feature = "parallel"))]
    pub(crate) fn iter_active_island<'a>(
        &'a self,
        island_id: usize,
    ) -> impl Iterator<Item = (RigidBodyHandle, &'a RigidBody)> {
        let island_range = self.active_islands[island_id]..self.active_islands[island_id + 1];
        let bodies: &'a _ = &self.bodies;
        self.active_dynamic_set[island_range]
            .iter()
            .filter_map(move |h| Some((*h, bodies.get(h.0)?)))
    }

    /// Applies the given function on all the active dynamic rigid-bodies
    /// contained by this set.
    #[inline(always)]
    #[cfg(not(feature = "dev-remove-slow-accessors"))]
    pub fn foreach_active_dynamic_body_mut(
        &mut self,
        mut f: impl FnMut(RigidBodyHandle, &mut RigidBody),
    ) {
        for handle in &self.active_dynamic_set {
            if let Some(rb) = self.bodies.get_mut(handle.0) {
                Self::mark_as_modified(
                    *handle,
                    rb,
                    &mut self.modified_bodies,
                    self.modified_all_bodies,
                );
                f(*handle, rb)
            }
        }
    }

    #[inline(always)]
    pub(crate) fn foreach_active_body_mut_internal(
        &mut self,
        mut f: impl FnMut(RigidBodyHandle, &mut RigidBody),
    ) {
        for handle in &self.active_dynamic_set {
            if let Some(rb) = self.bodies.get_mut(handle.0) {
                f(*handle, rb)
            }
        }

        for handle in &self.active_kinematic_set {
            if let Some(rb) = self.bodies.get_mut(handle.0) {
                f(*handle, rb)
            }
        }
    }

    #[inline(always)]
    pub(crate) fn foreach_active_dynamic_body_mut_internal(
        &mut self,
        mut f: impl FnMut(RigidBodyHandle, &mut RigidBody),
    ) {
        for handle in &self.active_dynamic_set {
            if let Some(rb) = self.bodies.get_mut(handle.0) {
                f(*handle, rb)
            }
        }
    }

    #[inline(always)]
    pub(crate) fn foreach_active_kinematic_body_mut_internal(
        &mut self,
        mut f: impl FnMut(RigidBodyHandle, &mut RigidBody),
    ) {
        for handle in &self.active_kinematic_set {
            if let Some(rb) = self.bodies.get_mut(handle.0) {
                f(*handle, rb)
            }
        }
    }

    #[inline(always)]
    #[cfg(not(feature = "parallel"))]
    pub(crate) fn foreach_active_island_body_mut_internal(
        &mut self,
        island_id: usize,
        mut f: impl FnMut(RigidBodyHandle, &mut RigidBody),
    ) {
        let island_range = self.active_islands[island_id]..self.active_islands[island_id + 1];
        for handle in &self.active_dynamic_set[island_range] {
            if let Some(rb) = self.bodies.get_mut(handle.0) {
                f(*handle, rb)
            }
        }
    }

    #[cfg(feature = "parallel")]
    #[inline(always)]
    #[allow(dead_code)]
    pub(crate) fn foreach_active_island_body_mut_internal_parallel(
        &mut self,
        island_id: usize,
        f: impl Fn(RigidBodyHandle, &mut RigidBody) + Send + Sync,
    ) {
        use std::sync::atomic::Ordering;

        let island_range = self.active_islands[island_id]..self.active_islands[island_id + 1];
        let bodies = std::sync::atomic::AtomicPtr::new(&mut self.bodies as *mut _);
        self.active_dynamic_set[island_range]
            .par_iter()
            .for_each_init(
                || bodies.load(Ordering::Relaxed),
                |bodies, handle| {
                    let bodies: &mut Arena<RigidBody> = unsafe { std::mem::transmute(*bodies) };
                    if let Some(rb) = bodies.get_mut(handle.0) {
                        f(*handle, rb)
                    }
                },
            );
    }

    // pub(crate) fn active_dynamic_set(&self) -> &[RigidBodyHandle] {
    //     &self.active_dynamic_set
    // }

    pub(crate) fn active_island_range(&self, island_id: usize) -> std::ops::Range<usize> {
        self.active_islands[island_id]..self.active_islands[island_id + 1]
    }

    pub(crate) fn active_island(&self, island_id: usize) -> &[RigidBodyHandle] {
        &self.active_dynamic_set[self.active_island_range(island_id)]
    }

    // Utility function to avoid some borrowing issue in the `maintain` method.
    fn maintain_one(
        bodies: &mut Arena<RigidBody>,
        colliders: &mut ColliderSet,
        handle: RigidBodyHandle,
        modified_inactive_set: &mut Vec<RigidBodyHandle>,
        active_kinematic_set: &mut Vec<RigidBodyHandle>,
        active_dynamic_set: &mut Vec<RigidBodyHandle>,
    ) {
        enum FinalAction {
            UpdateActiveKinematicSetId,
            UpdateActiveDynamicSetId,
        }

        if let Some(rb) = bodies.get_mut(handle.0) {
            let mut final_action = None;

            // The body's status changed. We need to make sure
            // it is on the correct active set.
            if rb.changes.contains(RigidBodyChanges::BODY_STATUS) {
                match rb.body_status() {
                    BodyStatus::Dynamic => {
                        // Remove from the active kinematic set if it was there.
                        if active_kinematic_set.get(rb.active_set_id) == Some(&handle) {
                            active_kinematic_set.swap_remove(rb.active_set_id);
                            final_action =
                                Some((FinalAction::UpdateActiveKinematicSetId, rb.active_set_id));
                        }

                        // Add to the active dynamic set.
                        rb.wake_up(true);
                        // Make sure the sleep change flag is set (even if for some
                        // reasons the rigid-body was already awake) to make
                        // sure the code handling sleeping change adds the body to
                        // the active_dynamic_set.
                        rb.changes.set(RigidBodyChanges::SLEEP, true);
                    }
                    BodyStatus::Kinematic => {
                        // Remove from the active dynamic set if it was there.
                        if active_dynamic_set.get(rb.active_set_id) == Some(&handle) {
                            active_dynamic_set.swap_remove(rb.active_set_id);
                            final_action =
                                Some((FinalAction::UpdateActiveDynamicSetId, rb.active_set_id));
                        }

                        // Add to the active kinematic set.
                        if active_kinematic_set.get(rb.active_set_id) != Some(&handle) {
                            rb.active_set_id = active_kinematic_set.len();
                            active_kinematic_set.push(handle);
                        }
                    }
                    BodyStatus::Static => {}
                }
            }

            // Update the positions of the colliders.
            if rb.changes.contains(RigidBodyChanges::POSITION)
                || rb.changes.contains(RigidBodyChanges::COLLIDERS)
            {
                rb.update_colliders_positions(colliders);

                if rb.is_static() {
                    modified_inactive_set.push(handle);
                }

                if rb.is_kinematic() && active_kinematic_set.get(rb.active_set_id) != Some(&handle)
                {
                    rb.active_set_id = active_kinematic_set.len();
                    active_kinematic_set.push(handle);
                }
            }

            // Push the body to the active set if it is not
            // sleeping and if it is not already inside of the active set.
            if rb.changes.contains(RigidBodyChanges::SLEEP)
                && !rb.is_sleeping() // May happen if the body was put to sleep manually.
                && rb.is_dynamic() // Only dynamic bodies are in the active dynamic set.
                && active_dynamic_set.get(rb.active_set_id) != Some(&handle)
            {
                rb.active_set_id = active_dynamic_set.len(); // This will handle the case where the activation_channel contains duplicates.
                active_dynamic_set.push(handle);
            }

            rb.changes = RigidBodyChanges::empty();

            // Adjust some ids, if needed.
            if let Some((action, id)) = final_action {
                let active_set = match action {
                    FinalAction::UpdateActiveKinematicSetId => active_kinematic_set,
                    FinalAction::UpdateActiveDynamicSetId => active_dynamic_set,
                };

                if id < active_set.len() {
                    if let Some(rb2) = bodies.get_mut(active_set[id].0) {
                        rb2.active_set_id = id;
                    }
                }
            }
        }
    }

    pub(crate) fn handle_user_changes(&mut self, colliders: &mut ColliderSet) {
        if self.modified_all_bodies {
            // Unfortunately, we have to push all the bodies to `modified_bodies`
            // instead of just calling `maintain_one` on each element i
            // `self.bodies.iter_mut()` because otherwise it would be difficult to
            // handle the final  change of active_set_id in Self::maintain_one
            // (because it has  to modify another rigid-body because of the swap-remove.
            // So this causes borrowing problems if we do this while iterating
            // through self.bodies.iter_mut()).
            for (handle, _) in self.bodies.iter_mut() {
                self.modified_bodies.push(RigidBodyHandle(handle));
            }
        }

        for handle in self.modified_bodies.drain(..) {
            Self::maintain_one(
                &mut self.bodies,
                colliders,
                handle,
                &mut self.modified_inactive_set,
                &mut self.active_kinematic_set,
                &mut self.active_dynamic_set,
            )
        }

        if self.modified_all_bodies {
            self.modified_bodies.shrink_to_fit(); // save some memory.
            self.modified_all_bodies = false;
        }
    }

    pub(crate) fn update_active_set_with_contacts(
        &mut self,
        colliders: &ColliderSet,
        narrow_phase: &NarrowPhase,
        joint_graph: &InteractionGraph<RigidBodyHandle, Joint>,
        min_island_size: usize,
    ) {
        assert!(
            min_island_size > 0,
            "The minimum island size must be at least 1."
        );

        // Update the energy of every rigid body and
        // keep only those that may not sleep.
        //        let t = instant::now();
        self.active_set_timestamp += 1;
        self.stack.clear();
        self.can_sleep.clear();

        // NOTE: the `.rev()` is here so that two successive timesteps preserve
        // the order of the bodies in the `active_dynamic_set` vec. This reversal
        // does not seem to affect performances nor stability. However it makes
        // debugging slightly nicer so we keep this rev.
        for h in self.active_dynamic_set.drain(..).rev() {
            let rb = &mut self.bodies[h.0];
            rb.update_energy();
            if rb.activation.energy <= rb.activation.threshold {
                // Mark them as sleeping for now. This will
                // be set to false during the graph traversal
                // if it should not be put to sleep.
                rb.activation.sleeping = true;
                self.can_sleep.push(h);
            } else {
                self.stack.push(h);
            }
        }

        // Read all the contacts and push objects touching touching this rigid-body.
        #[inline(always)]
        fn push_contacting_bodies(
            rb: &RigidBody,
            colliders: &ColliderSet,
            narrow_phase: &NarrowPhase,
            stack: &mut Vec<RigidBodyHandle>,
        ) {
            for collider_handle in &rb.colliders {
                if let Some(contacts) = narrow_phase.contacts_with(*collider_handle) {
                    for inter in contacts {
                        for manifold in &inter.2.manifolds {
                            if !manifold.data.solver_contacts.is_empty() {
                                let other = crate::utils::select_other(
                                    (inter.0, inter.1),
                                    *collider_handle,
                                );
                                let other_body = colliders[other].parent;
                                stack.push(other_body);
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Now iterate on all active kinematic bodies and push all the bodies
        // touching them to the stack so they can be woken up.
        for h in self.active_kinematic_set.iter() {
            let rb = &self.bodies[h.0];

            if !rb.is_moving() {
                // If the kinematic body does not move, it does not have
                // to wake up any dynamic body.
                continue;
            }

            push_contacting_bodies(rb, colliders, narrow_phase, &mut self.stack);
        }

        //        println!("Selection: {}", instant::now() - t);

        //        let t = instant::now();
        // Propagation of awake state and awake island computation through the
        // traversal of the interaction graph.
        self.active_islands.clear();
        self.active_islands.push(0);

        // The max avoid underflow when the stack is empty.
        let mut island_marker = self.stack.len().max(1) - 1;

        while let Some(handle) = self.stack.pop() {
            let rb = &mut self.bodies[handle.0];

            if rb.active_set_timestamp == self.active_set_timestamp || !rb.is_dynamic() {
                // We already visited this body and its neighbors.
                // Also, we don't propagate awake state through static bodies.
                continue;
            }

            if self.stack.len() < island_marker {
                if self.active_dynamic_set.len() - *self.active_islands.last().unwrap()
                    >= min_island_size
                {
                    // We are starting a new island.
                    self.active_islands.push(self.active_dynamic_set.len());
                }

                island_marker = self.stack.len();
            }

            rb.wake_up(false);
            rb.active_island_id = self.active_islands.len() - 1;
            rb.active_set_id = self.active_dynamic_set.len();
            rb.active_set_offset = rb.active_set_id - self.active_islands[rb.active_island_id];
            rb.active_set_timestamp = self.active_set_timestamp;
            self.active_dynamic_set.push(handle);

            // Transmit the active state to all the rigid-bodies with colliders
            // in contact or joined with this collider.
            push_contacting_bodies(rb, colliders, narrow_phase, &mut self.stack);

            for inter in joint_graph.interactions_with(rb.joint_graph_index) {
                let other = crate::utils::select_other((inter.0, inter.1), handle);
                self.stack.push(other);
            }
        }

        self.active_islands.push(self.active_dynamic_set.len());
        //        println!(
        //            "Extraction: {}, num islands: {}",
        //            instant::now() - t,
        //            self.active_islands.len() - 1
        //        );

        // Actually put to sleep bodies which have not been detected as awake.
        //        let t = instant::now();
        for h in &self.can_sleep {
            let b = &mut self.bodies[h.0];
            if b.activation.sleeping {
                b.sleep();
            }
        }
        //        println!("Activation: {}", instant::now() - t);
    }
}

impl Index<RigidBodyHandle> for RigidBodySet {
    type Output = RigidBody;

    fn index(&self, index: RigidBodyHandle) -> &RigidBody {
        &self.bodies[index.0]
    }
}

#[cfg(not(feature = "dev-remove-slow-accessors"))]
impl IndexMut<RigidBodyHandle> for RigidBodySet {
    fn index_mut(&mut self, handle: RigidBodyHandle) -> &mut RigidBody {
        let rb = &mut self.bodies[handle.0];
        Self::mark_as_modified(
            handle,
            rb,
            &mut self.modified_bodies,
            self.modified_all_bodies,
        );
        rb
    }
}

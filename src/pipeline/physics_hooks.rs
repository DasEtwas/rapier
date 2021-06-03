use crate::dynamics::RigidBody;
use crate::geometry::{Collider, ColliderHandle, ContactManifold, SolverContact, SolverFlags};
use crate::math::{Real, Vector};
use na::ComplexField;

/// Context given to custom collision filters to filter-out collisions.
pub struct PairFilterContext<'a> {
    /// The first rigid-body involved in the potential collision.
    pub rigid_body1: &'a RigidBody,
    /// The second rigid-body involved in the potential collision.
    pub rigid_body2: &'a RigidBody,
    /// The handle of the first collider involved in the potential collision.
    pub collider_handle1: ColliderHandle,
    /// The handle of the second collider involved in the potential collision.
    pub collider_handle2: ColliderHandle,
    /// The first collider involved in the potential collision.
    pub collider1: &'a Collider,
    /// The second collider involved in the potential collision.
    pub collider2: &'a Collider,
}

/// Context given to custom contact modifiers to modify the contacts seen by the constraints solver.
pub struct ContactModificationContext<'a> {
    /// The first rigid-body involved in the potential collision.
    pub rigid_body1: &'a RigidBody,
    /// The second rigid-body involved in the potential collision.
    pub rigid_body2: &'a RigidBody,
    /// The handle of the first collider involved in the potential collision.
    pub collider_handle1: ColliderHandle,
    /// The handle of the second collider involved in the potential collision.
    pub collider_handle2: ColliderHandle,
    /// The first collider involved in the potential collision.
    pub collider1: &'a Collider,
    /// The second collider involved in the potential collision.
    pub collider2: &'a Collider,
    /// The contact manifold.
    pub manifold: &'a ContactManifold,
    /// The solver contacts that can be modified.
    pub solver_contacts: &'a mut Vec<SolverContact>,
    /// The contact normal that can be modified.
    pub normal: &'a mut Vector<Real>,
    /// User-defined data attached to the manifold.
    // NOTE: we keep this a &'a mut u32 to emphasize the
    // fact that this can be modified.
    pub user_data: &'a mut u32,
}

impl<'a> ContactModificationContext<'a> {
    /// Helper function to update `self` to emulate a oneway-platform.
    ///
    /// The "oneway" behavior will only allow contacts between two colliders
    /// if the local contact normal of the first collider involved in the contact
    /// is almost aligned with the provided `allowed_local_n1` direction.
    ///
    /// To make this method work properly it must be called as part of the
    /// `PhysicsHooks::modify_solver_contacts` method at each timestep, for each
    /// contact manifold involving a one-way platform. The `self.user_data` field
    /// must not be modified from the outside of this method.
    pub fn update_as_oneway_platform(
        &mut self,
        allowed_local_n1: &Vector<Real>,
        allowed_angle: Real,
    ) {
        const CONTACT_CONFIGURATION_UNKNOWN: u32 = 0;
        const CONTACT_CURRENTLY_ALLOWED: u32 = 1;
        const CONTACT_CURRENTLY_FORBIDDEN: u32 = 2;

        let cang = ComplexField::cos(allowed_angle);

        // Test the allowed normal with the local-space contact normal that
        // points towards the exterior of context.collider1.
        let contact_is_ok = self.manifold.local_n1.dot(&allowed_local_n1) >= cang;

        match *self.user_data {
            CONTACT_CONFIGURATION_UNKNOWN => {
                if contact_is_ok {
                    // The contact is close enough to the allowed normal.
                    *self.user_data = CONTACT_CURRENTLY_ALLOWED;
                } else {
                    // The contact normal isn't close enough to the allowed
                    // normal, so remove all the contacts and mark further contacts
                    // as forbidden.
                    self.solver_contacts.clear();

                    // NOTE: in some very rare cases `local_n1` will be
                    // zero if the objects are exactly touching at one point.
                    // So in this case we can't really conclude.
                    // If the norm is non-zero, then we can tell we need to forbid
                    // further contacts. Otherwise we have to wait for the next frame.
                    if self.manifold.local_n1.norm_squared() > 0.1 {
                        *self.user_data = CONTACT_CURRENTLY_FORBIDDEN;
                    }
                }
            }
            CONTACT_CURRENTLY_FORBIDDEN => {
                // Contacts are forbidden so we need to continue forbidding contacts
                // until all the contacts are non-penetrating again. In that case, if
                // the contacts are OK wrt. the contact normal, then we can mark them as allowed.
                if contact_is_ok && self.solver_contacts.iter().all(|c| c.dist > 0.0) {
                    *self.user_data = CONTACT_CURRENTLY_ALLOWED;
                } else {
                    // Discard all the contacts.
                    self.solver_contacts.clear();
                }
            }
            CONTACT_CURRENTLY_ALLOWED => {
                // We allow all the contacts right now. The configuration becomes
                // uncertain again when the contact manifold no longer contains any contact.
                if self.solver_contacts.is_empty() {
                    *self.user_data = CONTACT_CONFIGURATION_UNKNOWN;
                }
            }
            _ => unreachable!(),
        }
    }
}

bitflags::bitflags! {
    #[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
    /// Flags affecting the behavior of the constraints solver for a given contact manifold.
    pub struct PhysicsHooksFlags: u32 {
        /// If set, Rapier will call `PhysicsHooks::filter_contact_pair` whenever relevant.
        const FILTER_CONTACT_PAIR = 0b0001;
        /// If set, Rapier will call `PhysicsHooks::filter_intersection_pair` whenever relevant.
        const FILTER_INTERSECTION_PAIR = 0b0010;
        /// If set, Rapier will call `PhysicsHooks::modify_solver_contact` whenever relevant.
        const MODIFY_SOLVER_CONTACTS = 0b0100;
    }
}

/// User-defined functions called by the physics engines during one timestep in order to customize its behavior.
pub trait PhysicsHooks: Send + Sync {
    /// The sets of hooks that must be taken into account.
    fn active_hooks(&self) -> PhysicsHooksFlags;

    /// Applies the contact pair filter.
    ///
    /// Note that this method will only be called if `self.active_hooks()`
    /// contains the `PhysicsHooksFlags::FILTER_CONTACT_PAIR` flags.
    ///
    /// User-defined filter for potential contact pairs detected by the broad-phase.
    /// This can be used to apply custom logic in order to decide whether two colliders
    /// should have their contact computed by the narrow-phase, and if these contact
    /// should be solved by the constraints solver
    ///
    /// Note that using a contact pair filter will replace the default contact filtering
    /// which consists of preventing contact computation between two non-dynamic bodies.
    ///
    /// This filtering method is called after taking into account the colliders collision groups.
    ///
    /// If this returns `None`, then the narrow-phase will ignore this contact pair and
    /// not compute any contact manifolds for it.
    /// If this returns `Some`, then the narrow-phase will compute contact manifolds for
    /// this pair of colliders, and configure them with the returned solver flags. For
    /// example, if this returns `Some(SolverFlags::COMPUTE_IMPULSES)` then the contacts
    /// will be taken into account by the constraints solver. If this returns
    /// `Some(SolverFlags::empty())` then the constraints solver will ignore these
    /// contacts.
    fn filter_contact_pair(&self, _context: &PairFilterContext) -> Option<SolverFlags> {
        None
    }

    /// Applies the intersection pair filter.
    ///
    /// Note that this method will only be called if `self.active_hooks()`
    /// contains the `PhysicsHooksFlags::FILTER_INTERSECTION_PAIR` flags.
    ///
    /// User-defined filter for potential intersection pairs detected by the broad-phase.
    ///
    /// This can be used to apply custom logic in order to decide whether two colliders
    /// should have their intersection computed by the narrow-phase.
    ///
    /// Note that using an intersection pair filter will replace the default intersection filtering
    /// which consists of preventing intersection computation between two non-dynamic bodies.
    ///
    /// This filtering method is called after taking into account the colliders collision groups.
    ///
    /// If this returns `false`, then the narrow-phase will ignore this pair and
    /// not compute any intersection information for it.
    /// If this return `true` then the narrow-phase will compute intersection
    /// information for this pair.
    fn filter_intersection_pair(&self, _context: &PairFilterContext) -> bool {
        false
    }

    /// Modifies the set of contacts seen by the constraints solver.
    ///
    /// Note that this method will only be called if `self.active_hooks()`
    /// contains the `PhysicsHooksFlags::MODIFY_SOLVER_CONTACTS` flags.
    ///
    /// By default, the content of `solver_contacts` is computed from `manifold.points`.
    /// This method will be called on each contact manifold which have the flag `SolverFlags::modify_solver_contacts` set.
    /// This method can be used to modify the set of solver contacts seen by the constraints solver: contacts
    /// can be removed and modified.
    ///
    /// Note that if all the contacts have to be ignored by the constraint solver, you may simply
    /// do `context.solver_contacts.clear()`.
    ///
    /// Modifying the solver contacts allow you to achieve various effects, including:
    /// - Simulating conveyor belts by setting the `surface_velocity` of a solver contact.
    /// - Simulating shapes with multiply materials by modifying the friction and restitution
    ///   coefficient depending of the features in contacts.
    /// - Simulating one-way platforms depending on the contact normal.
    ///
    /// Each contact manifold is given a `u32` user-defined data that is persistent between
    /// timesteps (as long as the contact manifold exists). This user-defined data is initialized
    /// as 0 and can be modified in `context.user_data`.
    ///
    /// The world-space contact normal can be modified in `context.normal`.
    fn modify_solver_contacts(&self, _context: &mut ContactModificationContext) {}
}

impl PhysicsHooks for () {
    fn active_hooks(&self) -> PhysicsHooksFlags {
        PhysicsHooksFlags::empty()
    }

    fn filter_contact_pair(&self, _: &PairFilterContext) -> Option<SolverFlags> {
        None
    }

    fn filter_intersection_pair(&self, _: &PairFilterContext) -> bool {
        false
    }

    fn modify_solver_contacts(&self, _: &mut ContactModificationContext) {}
}

use super::{DeltaVel, ParallelInteractionGroups, ParallelVelocitySolver};
use crate::dynamics::solver::{
    AnyJointPositionConstraint, AnyJointVelocityConstraint, AnyPositionConstraint,
    AnyVelocityConstraint, ParallelPositionSolver, ParallelSolverConstraints,
};
use crate::dynamics::{IntegrationParameters, JointGraphEdge, JointIndex, RigidBodySet};
use crate::geometry::{ContactManifold, ContactManifoldIndex};
use crate::math::{Isometry, Real};
use crate::utils::WAngularInertia;
use rayon::Scope;
use std::sync::atomic::{AtomicUsize, Ordering};

#[macro_export]
#[doc(hidden)]
macro_rules! concurrent_loop {
    (let batch_size = $batch_size: expr;
     for $elt: ident in $array: ident[$index_stream:expr,$index_count:expr] $f: expr) => {
        let max_index = $array.len();

        if max_index > 0 {
            loop {
                let start_index = $index_stream.fetch_add($batch_size, Ordering::SeqCst);
                if start_index > max_index {
                    break;
                }

                let end_index = (start_index + $batch_size).min(max_index);
                for $elt in &$array[start_index..end_index] {
                    $f
                }

                $index_count.fetch_add(end_index - start_index, Ordering::SeqCst);
            }
        }
    };

    (let batch_size = $batch_size: expr;
     for $elt: ident in $array: ident[$index_stream:expr] $f: expr) => {
        let max_index = $array.len();

        if max_index > 0 {
            loop {
                let start_index = $index_stream.fetch_add($batch_size, Ordering::SeqCst);
                if start_index > max_index {
                    break;
                }

                let end_index = (start_index + $batch_size).min(max_index);
                for $elt in &$array[start_index..end_index] {
                    $f
                }
            }
        }
    };
}

pub(crate) struct ThreadContext {
    pub batch_size: usize,
    // Velocity solver.
    pub constraint_initialization_index: AtomicUsize,
    pub num_initialized_constraints: AtomicUsize,
    pub joint_constraint_initialization_index: AtomicUsize,
    pub num_initialized_joint_constraints: AtomicUsize,
    pub warmstart_contact_index: AtomicUsize,
    pub num_warmstarted_contacts: AtomicUsize,
    pub warmstart_joint_index: AtomicUsize,
    pub num_warmstarted_joints: AtomicUsize,
    pub solve_interaction_index: AtomicUsize,
    pub num_solved_interactions: AtomicUsize,
    pub impulse_writeback_index: AtomicUsize,
    pub joint_writeback_index: AtomicUsize,
    pub body_integration_index: AtomicUsize,
    pub body_force_integration_index: AtomicUsize,
    pub num_force_integrated_bodies: AtomicUsize,
    pub num_integrated_bodies: AtomicUsize,
    // Position solver.
    pub position_constraint_initialization_index: AtomicUsize,
    pub num_initialized_position_constraints: AtomicUsize,
    pub position_joint_constraint_initialization_index: AtomicUsize,
    pub num_initialized_position_joint_constraints: AtomicUsize,
    pub solve_position_interaction_index: AtomicUsize,
    pub num_solved_position_interactions: AtomicUsize,
    pub position_writeback_index: AtomicUsize,
}

impl ThreadContext {
    pub fn new(batch_size: usize) -> Self {
        ThreadContext {
            // TODO perhaps there is some optimal value we can compute depending on the island size?
            batch_size,
            constraint_initialization_index: AtomicUsize::new(0),
            num_initialized_constraints: AtomicUsize::new(0),
            joint_constraint_initialization_index: AtomicUsize::new(0),
            num_initialized_joint_constraints: AtomicUsize::new(0),
            num_warmstarted_contacts: AtomicUsize::new(0),
            warmstart_contact_index: AtomicUsize::new(0),
            num_warmstarted_joints: AtomicUsize::new(0),
            warmstart_joint_index: AtomicUsize::new(0),
            solve_interaction_index: AtomicUsize::new(0),
            num_solved_interactions: AtomicUsize::new(0),
            impulse_writeback_index: AtomicUsize::new(0),
            joint_writeback_index: AtomicUsize::new(0),
            body_force_integration_index: AtomicUsize::new(0),
            num_force_integrated_bodies: AtomicUsize::new(0),
            body_integration_index: AtomicUsize::new(0),
            num_integrated_bodies: AtomicUsize::new(0),
            position_constraint_initialization_index: AtomicUsize::new(0),
            num_initialized_position_constraints: AtomicUsize::new(0),
            position_joint_constraint_initialization_index: AtomicUsize::new(0),
            num_initialized_position_joint_constraints: AtomicUsize::new(0),
            solve_position_interaction_index: AtomicUsize::new(0),
            num_solved_position_interactions: AtomicUsize::new(0),
            position_writeback_index: AtomicUsize::new(0),
        }
    }

    pub fn lock_until_ge(val: &AtomicUsize, target: usize) {
        if target > 0 {
            // let backoff = crossbeam::utils::Backoff::new();
            std::sync::atomic::fence(Ordering::SeqCst);
            while val.load(Ordering::Relaxed) < target {
                // backoff.spin();
                // std::thread::yield_now();
            }
        }
    }
}

pub struct ParallelIslandSolver {
    mj_lambdas: Vec<DeltaVel<Real>>,
    positions: Vec<Isometry<Real>>,
    parallel_groups: ParallelInteractionGroups,
    parallel_joint_groups: ParallelInteractionGroups,
    parallel_contact_constraints:
        ParallelSolverConstraints<AnyVelocityConstraint, AnyPositionConstraint>,
    parallel_joint_constraints:
        ParallelSolverConstraints<AnyJointVelocityConstraint, AnyJointPositionConstraint>,
    thread: ThreadContext,
}

impl ParallelIslandSolver {
    pub fn new() -> Self {
        Self {
            mj_lambdas: Vec::new(),
            positions: Vec::new(),
            parallel_groups: ParallelInteractionGroups::new(),
            parallel_joint_groups: ParallelInteractionGroups::new(),
            parallel_contact_constraints: ParallelSolverConstraints::new(),
            parallel_joint_constraints: ParallelSolverConstraints::new(),
            thread: ThreadContext::new(8),
        }
    }

    pub fn solve_position_constraints<'s>(
        &'s mut self,
        scope: &Scope<'s>,
        island_id: usize,
        params: &'s IntegrationParameters,
        bodies: &'s mut RigidBodySet,
    ) {
        let num_threads = rayon::current_num_threads();
        // TODO: not sure this is the best value. Also, perhaps it is better to interleave tasks of each island?
        let num_task_per_island = num_threads; // (num_threads / num_islands).max(1);
        // TODO: could we compute some kind of optimal value here?
        self.thread = ThreadContext::new(8);
        self.positions.clear();
        self.positions
            .resize(bodies.active_island(island_id).len(), Isometry::identity());

        for _ in 0..num_task_per_island {
            // We use AtomicPtr because it is Send+Sync while *mut is not.
            // See https://internals.rust-lang.org/t/shouldnt-pointers-be-send-sync-or/8818
            let thread = &self.thread;
            let positions = std::sync::atomic::AtomicPtr::new(&mut self.positions as *mut _);
            let bodies = std::sync::atomic::AtomicPtr::new(bodies as *mut _);
            let parallel_contact_constraints =
                std::sync::atomic::AtomicPtr::new(&mut self.parallel_contact_constraints as *mut _);
            let parallel_joint_constraints =
                std::sync::atomic::AtomicPtr::new(&mut self.parallel_joint_constraints as *mut _);

            scope.spawn(move |_| {
                // Transmute *mut -> &mut
                let positions: &mut Vec<Isometry<Real>> =
                    unsafe { std::mem::transmute(positions.load(Ordering::Relaxed)) };
                let bodies: &mut RigidBodySet =
                    unsafe { std::mem::transmute(bodies.load(Ordering::Relaxed)) };
                let parallel_contact_constraints: &mut ParallelSolverConstraints<AnyVelocityConstraint, AnyPositionConstraint> = unsafe {
                    std::mem::transmute(parallel_contact_constraints.load(Ordering::Relaxed))
                };
                let parallel_joint_constraints: &mut ParallelSolverConstraints<AnyJointVelocityConstraint, AnyJointPositionConstraint> = unsafe {
                    std::mem::transmute(parallel_joint_constraints.load(Ordering::Relaxed))
                };

                enable_flush_to_zero!(); // Ensure this is enabled on each thread.

                // Write results back to rigid bodies and integrate velocities.
                let island_range = bodies.active_island_range(island_id);
                let active_bodies = &bodies.active_dynamic_set[island_range];
                let bodies = &mut bodies.bodies;

                concurrent_loop! {
                    let batch_size = thread.batch_size;
                    for handle in active_bodies[thread.body_integration_index, thread.num_integrated_bodies] {
                        let rb = &mut bodies[handle.0];
                        positions[rb.active_set_offset] = rb.next_position;
                    }
                }

                ThreadContext::lock_until_ge(&thread.num_integrated_bodies, active_bodies.len());

                ParallelPositionSolver::solve(
                    &thread,
                    params,
                    positions,
                    parallel_contact_constraints,
                    parallel_joint_constraints
                );

                // Write results back to rigid bodies.
                concurrent_loop! {
                    let batch_size = thread.batch_size;
                    for handle in active_bodies[thread.position_writeback_index] {
                        let rb = &mut bodies[handle.0];
                        rb.set_next_position(positions[rb.active_set_offset]);
                    }
                }
            })
        }
    }

    pub fn init_constraints_and_solve_velocity_constraints<'s>(
        &'s mut self,
        scope: &Scope<'s>,
        island_id: usize,
        params: &'s IntegrationParameters,
        bodies: &'s mut RigidBodySet,
        manifolds: &'s mut Vec<&'s mut ContactManifold>,
        manifold_indices: &'s [ContactManifoldIndex],
        joints: &'s mut Vec<JointGraphEdge>,
        joint_indices: &[JointIndex],
    ) {
        let num_threads = rayon::current_num_threads();
        // TODO: not sure this is the best value. Also, perhaps it is better to interleave tasks of each island?
        let num_task_per_island = num_threads; // (num_threads / num_islands).max(1);
        // TODO: could we compute some kind of optimal value here?
        self.thread = ThreadContext::new(8);
        self.parallel_groups
            .group_interactions(island_id, bodies, manifolds, manifold_indices);
        self.parallel_joint_groups
            .group_interactions(island_id, bodies, joints, joint_indices);
        self.parallel_contact_constraints.init_constraint_groups(
            island_id,
            bodies,
            manifolds,
            &self.parallel_groups,
        );
        self.parallel_joint_constraints.init_constraint_groups(
            island_id,
            bodies,
            joints,
            &self.parallel_joint_groups,
        );

        self.mj_lambdas.clear();
        self.mj_lambdas
            .resize(bodies.active_island(island_id).len(), DeltaVel::zero());
        self.positions.clear();
        self.positions
            .resize(bodies.active_island(island_id).len(), Isometry::identity());

        for _ in 0..num_task_per_island {
            // We use AtomicPtr because it is Send+Sync while *mut is not.
            // See https://internals.rust-lang.org/t/shouldnt-pointers-be-send-sync-or/8818
            let thread = &self.thread;
            let mj_lambdas = std::sync::atomic::AtomicPtr::new(&mut self.mj_lambdas as *mut _);
            let bodies = std::sync::atomic::AtomicPtr::new(bodies as *mut _);
            let manifolds = std::sync::atomic::AtomicPtr::new(manifolds as *mut _);
            let joints = std::sync::atomic::AtomicPtr::new(joints as *mut _);
            let parallel_contact_constraints =
                std::sync::atomic::AtomicPtr::new(&mut self.parallel_contact_constraints as *mut _);
            let parallel_joint_constraints =
                std::sync::atomic::AtomicPtr::new(&mut self.parallel_joint_constraints as *mut _);

            scope.spawn(move |_| {
                // Transmute *mut -> &mut
                let mj_lambdas: &mut Vec<DeltaVel<Real>> =
                    unsafe { std::mem::transmute(mj_lambdas.load(Ordering::Relaxed)) };
                let bodies: &mut RigidBodySet =
                    unsafe { std::mem::transmute(bodies.load(Ordering::Relaxed)) };
                let manifolds: &mut Vec<&mut ContactManifold> =
                    unsafe { std::mem::transmute(manifolds.load(Ordering::Relaxed)) };
                let joints: &mut Vec<JointGraphEdge> =
                    unsafe { std::mem::transmute(joints.load(Ordering::Relaxed)) };
                let parallel_contact_constraints: &mut ParallelSolverConstraints<AnyVelocityConstraint, AnyPositionConstraint> = unsafe {
                    std::mem::transmute(parallel_contact_constraints.load(Ordering::Relaxed))
                };
                let parallel_joint_constraints: &mut ParallelSolverConstraints<AnyJointVelocityConstraint, AnyJointPositionConstraint> = unsafe {
                    std::mem::transmute(parallel_joint_constraints.load(Ordering::Relaxed))
                };

                enable_flush_to_zero!(); // Ensure this is enabled on each thread.

                // Initialize `mj_lambdas` (per-body velocity deltas) with external accelerations (gravity etc):
                {
                    let island_range = bodies.active_island_range(island_id);
                    let active_bodies = &bodies.active_dynamic_set[island_range];
                    let bodies = &mut bodies.bodies;

                    concurrent_loop! {
                        let batch_size = thread.batch_size;
                        for handle in active_bodies[thread.body_force_integration_index, thread.num_force_integrated_bodies] {
                            let rb = &mut bodies[handle.0];
                            let dvel = &mut mj_lambdas[rb.active_set_offset];

                            // NOTE: `dvel.angular` is actually storing angular velocity delta multiplied
                            //       by the square root of the inertia tensor:
                            dvel.angular += rb.effective_world_inv_inertia_sqrt * rb.torque * params.dt;
                            dvel.linear += rb.force * (rb.effective_inv_mass * params.dt);
                        }
                    }

                    // We need to wait for every body to be force-integrated because their
                    // angular and linear velocities are needed by the constraints initialization.
                    ThreadContext::lock_until_ge(&thread.num_force_integrated_bodies, active_bodies.len());
                }


                parallel_contact_constraints.fill_constraints(&thread, params, bodies, manifolds);
                parallel_joint_constraints.fill_constraints(&thread, params, bodies, joints);
                ThreadContext::lock_until_ge(
                    &thread.num_initialized_constraints,
                    parallel_contact_constraints.constraint_descs.len(),
                );
                ThreadContext::lock_until_ge(
                    &thread.num_initialized_joint_constraints,
                    parallel_joint_constraints.constraint_descs.len(),
                );

                ParallelVelocitySolver::solve(
                        &thread,
                        params,
                        manifolds,
                        joints,
                        mj_lambdas,
                        parallel_contact_constraints,
                        parallel_joint_constraints
                );

                // Write results back to rigid bodies and integrate velocities.
                let island_range = bodies.active_island_range(island_id);
                let active_bodies = &bodies.active_dynamic_set[island_range];
                let bodies = &mut bodies.bodies;

                concurrent_loop! {
                    let batch_size = thread.batch_size;
                    for handle in active_bodies[thread.body_integration_index, thread.num_integrated_bodies] {
                        let rb = &mut bodies[handle.0];
                        let dvel = mj_lambdas[rb.active_set_offset];
                        rb.linvel += dvel.linear;
                        rb.angvel += rb.effective_world_inv_inertia_sqrt.transform_vector(dvel.angular);
                        rb.apply_damping(params.dt);
                        rb.integrate_next_position(params.dt);
                    }
                }
            })
        }
    }
}

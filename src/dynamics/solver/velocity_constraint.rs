use crate::dynamics::solver::VelocityGroundConstraint;
#[cfg(feature = "simd-is-enabled")]
use crate::dynamics::solver::{WVelocityConstraint, WVelocityGroundConstraint};
use crate::dynamics::{IntegrationParameters, RigidBodySet};
use crate::geometry::{ContactManifold, ContactManifoldIndex};
use crate::math::{Real, Vector, DIM, MAX_MANIFOLD_POINTS};
use crate::utils::{WAngularInertia, WBasis, WCross, WDot};

use super::{DeltaVel, VelocityConstraintElement, VelocityConstraintNormalPart};

//#[repr(align(64))]
#[derive(Copy, Clone, Debug)]
pub(crate) enum AnyVelocityConstraint {
    NongroupedGround(VelocityGroundConstraint),
    Nongrouped(VelocityConstraint),
    #[cfg(feature = "simd-is-enabled")]
    GroupedGround(WVelocityGroundConstraint),
    #[cfg(feature = "simd-is-enabled")]
    Grouped(WVelocityConstraint),
    #[allow(dead_code)]
    // The Empty variant is only used with parallel code.
    Empty,
}

impl AnyVelocityConstraint {
    #[cfg(target_arch = "wasm32")]
    pub fn as_nongrouped_mut(&mut self) -> Option<&mut VelocityConstraint> {
        if let AnyVelocityConstraint::Nongrouped(c) = self {
            Some(c)
        } else {
            None
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn as_nongrouped_ground_mut(&mut self) -> Option<&mut VelocityGroundConstraint> {
        if let AnyVelocityConstraint::NongroupedGround(c) = self {
            Some(c)
        } else {
            None
        }
    }

    pub fn warmstart(&self, mj_lambdas: &mut [DeltaVel<Real>]) {
        match self {
            AnyVelocityConstraint::NongroupedGround(c) => c.warmstart(mj_lambdas),
            AnyVelocityConstraint::Nongrouped(c) => c.warmstart(mj_lambdas),
            #[cfg(feature = "simd-is-enabled")]
            AnyVelocityConstraint::GroupedGround(c) => c.warmstart(mj_lambdas),
            #[cfg(feature = "simd-is-enabled")]
            AnyVelocityConstraint::Grouped(c) => c.warmstart(mj_lambdas),
            AnyVelocityConstraint::Empty => unreachable!(),
        }
    }

    pub fn solve(&mut self, mj_lambdas: &mut [DeltaVel<Real>]) {
        match self {
            AnyVelocityConstraint::NongroupedGround(c) => c.solve(mj_lambdas),
            AnyVelocityConstraint::Nongrouped(c) => c.solve(mj_lambdas),
            #[cfg(feature = "simd-is-enabled")]
            AnyVelocityConstraint::GroupedGround(c) => c.solve(mj_lambdas),
            #[cfg(feature = "simd-is-enabled")]
            AnyVelocityConstraint::Grouped(c) => c.solve(mj_lambdas),
            AnyVelocityConstraint::Empty => unreachable!(),
        }
    }

    pub fn writeback_impulses(&self, manifold_all: &mut [&mut ContactManifold]) {
        match self {
            AnyVelocityConstraint::NongroupedGround(c) => c.writeback_impulses(manifold_all),
            AnyVelocityConstraint::Nongrouped(c) => c.writeback_impulses(manifold_all),
            #[cfg(feature = "simd-is-enabled")]
            AnyVelocityConstraint::GroupedGround(c) => c.writeback_impulses(manifold_all),
            #[cfg(feature = "simd-is-enabled")]
            AnyVelocityConstraint::Grouped(c) => c.writeback_impulses(manifold_all),
            AnyVelocityConstraint::Empty => unreachable!(),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct VelocityConstraint {
    // Non-penetration force direction for the first body.
    pub dir1: Vector<Real>,
    #[cfg(feature = "dim3")]
    // One of the friction force directions.
    pub tangent1: Vector<Real>,
    #[cfg(feature = "dim3")]
    // Orientation of the tangent basis wrt. the reference basis.
    pub tangent_rot1: na::UnitComplex<Real>,
    pub im1: Real,
    pub im2: Real,
    pub limit: Real,
    pub mj_lambda1: usize,
    pub mj_lambda2: usize,
    pub manifold_id: ContactManifoldIndex,
    pub manifold_contact_id: [u8; MAX_MANIFOLD_POINTS],
    pub num_contacts: u8,
    pub elements: [VelocityConstraintElement<Real>; MAX_MANIFOLD_POINTS],
}

impl VelocityConstraint {
    #[cfg(feature = "parallel")]
    pub fn num_active_constraints(manifold: &ContactManifold) -> usize {
        let rest = manifold.data.solver_contacts.len() % MAX_MANIFOLD_POINTS != 0;
        manifold.data.solver_contacts.len() / MAX_MANIFOLD_POINTS + rest as usize
    }

    pub fn generate(
        params: &IntegrationParameters,
        manifold_id: ContactManifoldIndex,
        manifold: &ContactManifold,
        bodies: &RigidBodySet,
        out_constraints: &mut Vec<AnyVelocityConstraint>,
        push: bool,
    ) {
        assert_eq!(manifold.data.relative_dominance, 0);

        let inv_dt = params.inv_dt();
        let velocity_based_erp_inv_dt = params.velocity_based_erp_inv_dt();

        let rb1 = &bodies[manifold.data.body_pair.body1];
        let rb2 = &bodies[manifold.data.body_pair.body2];
        let mj_lambda1 = rb1.active_set_offset;
        let mj_lambda2 = rb2.active_set_offset;
        let force_dir1 = -manifold.data.normal;
        let warmstart_coeff = manifold.data.warmstart_multiplier * params.warmstart_coeff;

        #[cfg(feature = "dim2")]
        let tangents1 = force_dir1.orthonormal_basis();
        #[cfg(feature = "dim3")]
        let (tangents1, tangent_rot1) =
            super::compute_tangent_contact_directions(&force_dir1, &rb1.linvel, &rb2.linvel);

        for (_l, manifold_points) in manifold
            .data
            .solver_contacts
            .chunks(MAX_MANIFOLD_POINTS)
            .enumerate()
        {
            #[cfg(not(target_arch = "wasm32"))]
            let mut constraint = VelocityConstraint {
                dir1: force_dir1,
                #[cfg(feature = "dim3")]
                tangent1: tangents1[0],
                #[cfg(feature = "dim3")]
                tangent_rot1,
                elements: [VelocityConstraintElement::zero(); MAX_MANIFOLD_POINTS],
                im1: rb1.effective_inv_mass,
                im2: rb2.effective_inv_mass,
                limit: 0.0,
                mj_lambda1,
                mj_lambda2,
                manifold_id,
                manifold_contact_id: [0; MAX_MANIFOLD_POINTS],
                num_contacts: manifold_points.len() as u8,
            };

            // TODO: this is a WIP optimization for WASM platforms.
            // For some reasons, the compiler does not inline the `Vec::push` method
            // in this method. This generates two memset and one memcpy which are both very
            // expansive.
            // This would likely be solved by some kind of "placement-push" (like emplace in C++).
            // In the mean time, a workaround is to "push" using `.resize_with` and `::uninit()` to
            // avoid spurious copying.
            // Is this optimization beneficial when targeting non-WASM platforms?
            //
            // NOTE: joints have the same problem, but it is not easy to refactor the code that way
            // for the moment.
            #[cfg(target_arch = "wasm32")]
            let constraint = if push {
                let new_len = out_constraints.len() + 1;
                unsafe {
                    out_constraints.resize_with(new_len, || {
                        AnyVelocityConstraint::Nongrouped(
                            std::mem::MaybeUninit::uninit().assume_init(),
                        )
                    });
                }
                out_constraints
                    .last_mut()
                    .unwrap()
                    .as_nongrouped_mut()
                    .unwrap()
            } else {
                unreachable!(); // We don't have parallelization on WASM yet, so this is unreachable.
            };

            #[cfg(target_arch = "wasm32")]
            {
                constraint.dir1 = force_dir1;
                #[cfg(feature = "dim3")]
                {
                    constraint.tangent1 = tangents1[0];
                    constraint.tangent_rot1 = tangent_rot1;
                }
                constraint.im1 = rb1.effective_inv_mass;
                constraint.im2 = rb2.effective_inv_mass;
                constraint.limit = 0.0;
                constraint.mj_lambda1 = mj_lambda1;
                constraint.mj_lambda2 = mj_lambda2;
                constraint.manifold_id = manifold_id;
                constraint.manifold_contact_id = [0; MAX_MANIFOLD_POINTS];
                constraint.num_contacts = manifold_points.len() as u8;
            }

            for k in 0..manifold_points.len() {
                let manifold_point = &manifold_points[k];
                let dp1 = manifold_point.point - rb1.world_com;
                let dp2 = manifold_point.point - rb2.world_com;

                let vel1 = rb1.linvel + rb1.angvel.gcross(dp1);
                let vel2 = rb2.linvel + rb2.angvel.gcross(dp2);

                let warmstart_correction;

                constraint.limit = manifold_point.friction;
                constraint.manifold_contact_id[k] = manifold_point.contact_id;

                // Normal part.
                {
                    let gcross1 = rb1
                        .effective_world_inv_inertia_sqrt
                        .transform_vector(dp1.gcross(force_dir1));
                    let gcross2 = rb2
                        .effective_world_inv_inertia_sqrt
                        .transform_vector(dp2.gcross(-force_dir1));

                    let r = 1.0
                        / (rb1.effective_inv_mass
                            + rb2.effective_inv_mass
                            + gcross1.gdot(gcross1)
                            + gcross2.gdot(gcross2));

                    let is_bouncy = manifold_point.is_bouncy() as u32 as Real;
                    let is_resting = 1.0 - is_bouncy;

                    let mut rhs = (1.0 + is_bouncy * manifold_point.restitution)
                        * (vel1 - vel2).dot(&force_dir1);
                    rhs += manifold_point.dist.max(0.0) * inv_dt;
                    rhs *= is_bouncy + is_resting * params.velocity_solve_fraction;
                    rhs += is_resting * velocity_based_erp_inv_dt * manifold_point.dist.min(0.0);
                    warmstart_correction = (params.warmstart_correction_slope
                        / (rhs - manifold_point.prev_rhs).abs())
                    .min(warmstart_coeff);

                    constraint.elements[k].normal_part = VelocityConstraintNormalPart {
                        gcross1,
                        gcross2,
                        rhs,
                        impulse: manifold_point.warmstart_impulse * warmstart_correction,
                        r,
                    };
                }

                // Tangent parts.
                {
                    #[cfg(feature = "dim3")]
                    let impulse = tangent_rot1
                        * manifold_points[k].warmstart_tangent_impulse
                        * warmstart_correction;
                    #[cfg(feature = "dim2")]
                    let impulse =
                        [manifold_points[k].warmstart_tangent_impulse * warmstart_correction];
                    constraint.elements[k].tangent_part.impulse = impulse;

                    for j in 0..DIM - 1 {
                        let gcross1 = rb1
                            .effective_world_inv_inertia_sqrt
                            .transform_vector(dp1.gcross(tangents1[j]));
                        let gcross2 = rb2
                            .effective_world_inv_inertia_sqrt
                            .transform_vector(dp2.gcross(-tangents1[j]));
                        let r = 1.0
                            / (rb1.effective_inv_mass
                                + rb2.effective_inv_mass
                                + gcross1.gdot(gcross1)
                                + gcross2.gdot(gcross2));
                        let rhs =
                            (vel1 - vel2 + manifold_point.tangent_velocity).dot(&tangents1[j]);

                        constraint.elements[k].tangent_part.gcross1[j] = gcross1;
                        constraint.elements[k].tangent_part.gcross2[j] = gcross2;
                        constraint.elements[k].tangent_part.rhs[j] = rhs;
                        constraint.elements[k].tangent_part.r[j] = r;
                    }
                }
            }

            #[cfg(not(target_arch = "wasm32"))]
            if push {
                out_constraints.push(AnyVelocityConstraint::Nongrouped(constraint));
            } else {
                out_constraints[manifold.data.constraint_index + _l] =
                    AnyVelocityConstraint::Nongrouped(constraint);
            }
        }
    }

    pub fn warmstart(&self, mj_lambdas: &mut [DeltaVel<Real>]) {
        let mut mj_lambda1 = DeltaVel::zero();
        let mut mj_lambda2 = DeltaVel::zero();

        VelocityConstraintElement::warmstart_group(
            &self.elements[..self.num_contacts as usize],
            &self.dir1,
            #[cfg(feature = "dim3")]
            &self.tangent1,
            self.im1,
            self.im2,
            &mut mj_lambda1,
            &mut mj_lambda2,
        );

        mj_lambdas[self.mj_lambda1 as usize] += mj_lambda1;
        mj_lambdas[self.mj_lambda2 as usize] += mj_lambda2;
    }

    pub fn solve(&mut self, mj_lambdas: &mut [DeltaVel<Real>]) {
        let mut mj_lambda1 = mj_lambdas[self.mj_lambda1 as usize];
        let mut mj_lambda2 = mj_lambdas[self.mj_lambda2 as usize];

        VelocityConstraintElement::solve_group(
            &mut self.elements[..self.num_contacts as usize],
            &self.dir1,
            #[cfg(feature = "dim3")]
            &self.tangent1,
            self.im1,
            self.im2,
            self.limit,
            &mut mj_lambda1,
            &mut mj_lambda2,
        );

        mj_lambdas[self.mj_lambda1 as usize] = mj_lambda1;
        mj_lambdas[self.mj_lambda2 as usize] = mj_lambda2;
    }

    pub fn writeback_impulses(&self, manifolds_all: &mut [&mut ContactManifold]) {
        let manifold = &mut manifolds_all[self.manifold_id];

        for k in 0..self.num_contacts as usize {
            let contact_id = self.manifold_contact_id[k];
            let active_contact = &mut manifold.points[contact_id as usize];
            active_contact.data.impulse = self.elements[k].normal_part.impulse;
            active_contact.data.rhs = self.elements[k].normal_part.rhs;

            #[cfg(feature = "dim2")]
            {
                active_contact.data.tangent_impulse = self.elements[k].tangent_part.impulse[0];
            }
            #[cfg(feature = "dim3")]
            {
                active_contact.data.tangent_impulse = self
                    .tangent_rot1
                    .inverse_transform_vector(&self.elements[k].tangent_part.impulse);
            }
        }
    }
}

#[inline(always)]
#[cfg(feature = "dim3")]
pub(crate) fn compute_tangent_contact_directions<N>(
    force_dir1: &Vector<N>,
    linvel1: &Vector<N>,
    linvel2: &Vector<N>,
) -> ([Vector<N>; DIM - 1], na::UnitComplex<N>)
where
    N: na::SimdRealField,
    N::Element: na::RealField,
    Vector<N>: WBasis,
{
    use na::SimdValue;

    // Compute the tangent direction. Pick the direction of
    // the linear relative velocity, if it is not too small.
    // Otherwise use a fallback direction.
    let relative_linvel = linvel1 - linvel2;
    let mut tangent_relative_linvel =
        relative_linvel - force_dir1 * (force_dir1.dot(&relative_linvel));
    let tangent_linvel_norm = tangent_relative_linvel.normalize_mut();
    let threshold: N::Element = na::convert(1.0e-4);
    let use_fallback = tangent_linvel_norm.simd_lt(N::splat(threshold));
    let tangent_fallback = force_dir1.orthonormal_vector();

    let tangent1 = tangent_fallback.select(use_fallback, tangent_relative_linvel);
    let bitangent1 = force_dir1.cross(&tangent1);

    // Compute a rotation such that: rot * tangent_fallback = tangent1 (when projected in the tangent plane.)
    //
    // This is needed to ensure the warmstart impulse has the correct orientation.
    // Indeed, at frame n + 1, we need to reapply the same impulse as we did in frame n.
    // However, the basis on which the tangent impulse is expressed may change in each frame
    // (because the the relative linvel may change direction at each frame).
    //
    // So, we need this rotation to:
    // - Project the impulse back to the "reference" basis at after friction is resolved.
    // - Project the old impulse on the new basis before the friction is resolved.
    let rot = na::UnitComplex::new_unchecked(na::Complex::new(
        tangent1.dot(&tangent_fallback),
        bitangent1.dot(&tangent_fallback),
    ));
    ([tangent1, bitangent1], rot)
}

use super::{
    AnyVelocityConstraint, DeltaVel, VelocityGroundConstraintElement,
    VelocityGroundConstraintNormalPart,
};
use crate::math::{Real, Vector, DIM, MAX_MANIFOLD_POINTS};
#[cfg(feature = "dim2")]
use crate::utils::WBasis;
use crate::utils::{WAngularInertia, WCross, WDot};

use crate::dynamics::{IntegrationParameters, RigidBodySet};
use crate::geometry::{ContactManifold, ContactManifoldIndex};

#[derive(Copy, Clone, Debug)]
pub(crate) struct VelocityGroundConstraint {
    pub mj_lambda2: usize,
    // Non-penetration force direction for the first body.
    pub dir1: Vector<Real>,
    #[cfg(feature = "dim3")]
    // One of the friction force directions.
    pub tangent1: Vector<Real>,
    pub im2: Real,
    pub limit: Real,
    pub elements: [VelocityGroundConstraintElement<Real>; MAX_MANIFOLD_POINTS],

    #[cfg(feature = "dim3")]
    // Orientation of the tangent basis wrt. the reference basis.
    pub tangent_rot1: na::UnitComplex<Real>,
    pub manifold_id: ContactManifoldIndex,
    pub manifold_contact_id: [u8; MAX_MANIFOLD_POINTS],
    pub num_contacts: u8,
}

impl VelocityGroundConstraint {
    pub fn generate(
        params: &IntegrationParameters,
        manifold_id: ContactManifoldIndex,
        manifold: &ContactManifold,
        bodies: &RigidBodySet,
        out_constraints: &mut Vec<AnyVelocityConstraint>,
        push: bool,
    ) {
        let inv_dt = params.inv_dt();
        let velocity_based_erp_inv_dt = params.velocity_based_erp_inv_dt();

        let mut rb1 = &bodies[manifold.data.body_pair.body1];
        let mut rb2 = &bodies[manifold.data.body_pair.body2];
        let flipped = manifold.data.relative_dominance < 0;

        let (force_dir1, flipped_multiplier) = if flipped {
            std::mem::swap(&mut rb1, &mut rb2);
            (manifold.data.normal, -1.0)
        } else {
            (-manifold.data.normal, 1.0)
        };

        #[cfg(feature = "dim2")]
        let tangents1 = force_dir1.orthonormal_basis();
        #[cfg(feature = "dim3")]
        let (tangents1, tangent_rot1) =
            super::compute_tangent_contact_directions(&force_dir1, &rb1.linvel, &rb2.linvel);

        let mj_lambda2 = rb2.active_set_offset;
        let warmstart_coeff = manifold.data.warmstart_multiplier * params.warmstart_coeff;

        for (_l, manifold_points) in manifold
            .data
            .solver_contacts
            .chunks(MAX_MANIFOLD_POINTS)
            .enumerate()
        {
            #[cfg(not(target_arch = "wasm32"))]
            let mut constraint = VelocityGroundConstraint {
                dir1: force_dir1,
                #[cfg(feature = "dim3")]
                tangent1: tangents1[0],
                #[cfg(feature = "dim3")]
                tangent_rot1,
                elements: [VelocityGroundConstraintElement::zero(); MAX_MANIFOLD_POINTS],
                im2: rb2.effective_inv_mass,
                limit: 0.0,
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
                        AnyVelocityConstraint::NongroupedGround(
                            std::mem::MaybeUninit::uninit().assume_init(),
                        )
                    });
                }
                out_constraints
                    .last_mut()
                    .unwrap()
                    .as_nongrouped_ground_mut()
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
                constraint.im2 = rb2.effective_inv_mass;
                constraint.limit = 0.0;
                constraint.mj_lambda2 = mj_lambda2;
                constraint.manifold_id = manifold_id;
                constraint.manifold_contact_id = [0; MAX_MANIFOLD_POINTS];
                constraint.num_contacts = manifold_points.len() as u8;
            }

            for k in 0..manifold_points.len() {
                let manifold_point = &manifold_points[k];
                let dp2 = manifold_point.point - rb2.world_com;
                let dp1 = manifold_point.point - rb1.world_com;
                let vel1 = rb1.linvel + rb1.angvel.gcross(dp1);
                let vel2 = rb2.linvel + rb2.angvel.gcross(dp2);
                let warmstart_correction;

                constraint.limit = manifold_point.friction;
                constraint.manifold_contact_id[k] = manifold_point.contact_id;

                // Normal part.
                {
                    let gcross2 = rb2
                        .effective_world_inv_inertia_sqrt
                        .transform_vector(dp2.gcross(-force_dir1));

                    let r = 1.0 / (rb2.effective_inv_mass + gcross2.gdot(gcross2));

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

                    constraint.elements[k].normal_part = VelocityGroundConstraintNormalPart {
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
                        let gcross2 = rb2
                            .effective_world_inv_inertia_sqrt
                            .transform_vector(dp2.gcross(-tangents1[j]));
                        let r = 1.0 / (rb2.effective_inv_mass + gcross2.gdot(gcross2));
                        let rhs = (vel1 - vel2
                            + flipped_multiplier * manifold_point.tangent_velocity)
                            .dot(&tangents1[j]);

                        constraint.elements[k].tangent_part.gcross2[j] = gcross2;
                        constraint.elements[k].tangent_part.rhs[j] = rhs;
                        constraint.elements[k].tangent_part.r[j] = r;
                    }
                }
            }

            #[cfg(not(target_arch = "wasm32"))]
            if push {
                out_constraints.push(AnyVelocityConstraint::NongroupedGround(constraint));
            } else {
                out_constraints[manifold.data.constraint_index + _l] =
                    AnyVelocityConstraint::NongroupedGround(constraint);
            }
        }
    }

    pub fn warmstart(&self, mj_lambdas: &mut [DeltaVel<Real>]) {
        let mut mj_lambda2 = DeltaVel::zero();

        VelocityGroundConstraintElement::warmstart_group(
            &self.elements[..self.num_contacts as usize],
            &self.dir1,
            #[cfg(feature = "dim3")]
            &self.tangent1,
            self.im2,
            &mut mj_lambda2,
        );

        mj_lambdas[self.mj_lambda2 as usize].linear += mj_lambda2.linear;
        mj_lambdas[self.mj_lambda2 as usize].angular += mj_lambda2.angular;
    }

    pub fn solve(&mut self, mj_lambdas: &mut [DeltaVel<Real>]) {
        let mut mj_lambda2 = mj_lambdas[self.mj_lambda2 as usize];

        VelocityGroundConstraintElement::solve_group(
            &mut self.elements[..self.num_contacts as usize],
            &self.dir1,
            #[cfg(feature = "dim3")]
            &self.tangent1,
            self.im2,
            self.limit,
            &mut mj_lambda2,
        );

        mj_lambdas[self.mj_lambda2 as usize] = mj_lambda2;
    }

    // FIXME: duplicated code. This is exactly the same as in the non-ground velocity constraint.
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

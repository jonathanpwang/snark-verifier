use crate::{
    util::{
        arithmetic::{CurveAffine, FieldOps, PrimeField},
        Itertools,
    },
    Error,
};
use std::{fmt::Debug, iter};

pub mod native;

#[cfg(feature = "loader_evm")]
pub mod evm;

#[cfg(feature = "loader_halo2")]
pub mod halo2;

pub trait LoadedEcPoint<C: CurveAffine>: Clone + Debug + PartialEq {
    type Loader: Loader<C, LoadedEcPoint = Self>;

    fn loader(&self) -> &Self::Loader;

    fn multi_scalar_multiplication(
        pairs: impl IntoIterator<Item = (<Self::Loader as ScalarLoader<C::Scalar>>::LoadedScalar, Self)>,
    ) -> Self;
}

pub trait LoadedScalar<F: PrimeField>: Clone + Debug + PartialEq + FieldOps {
    type Loader: ScalarLoader<F, LoadedScalar = Self>;

    fn loader(&self) -> &Self::Loader;

    fn square(&self) -> Self {
        self.clone() * self
    }

    fn mul_add(a: &Self, b: &Self, c: &Self) -> Self;

    fn mul_add_constant(a: &Self, b: &Self, c: &F) -> Self;

    fn invert(&self) -> Option<Self> {
        FieldOps::invert(self)
    }

    fn batch_invert<'a>(values: impl IntoIterator<Item = &'a mut Self>)
    where
        Self: 'a,
    {
        values
            .into_iter()
            .for_each(|value| *value = LoadedScalar::invert(value).unwrap_or_else(|| value.clone()))
    }

    fn pow_const(&self, mut exp: u64) -> Self {
        assert!(exp > 0);

        let mut base = self.clone();

        while exp & 1 == 0 {
            base = base.square();
            exp >>= 1;
        }

        let mut acc = base.clone();
        while exp > 1 {
            exp >>= 1;
            base = base.square();
            if exp & 1 == 1 {
                acc *= &base;
            }
        }
        acc
    }

    fn powers(&self, n: usize) -> Vec<Self> {
        iter::once(self.loader().load_one())
            .chain(
                iter::successors(Some(self.clone()), |power| Some(power.clone() * self))
                    .take(n - 1),
            )
            .collect_vec()
    }
}

pub trait EcPointLoader<C: CurveAffine> {
    type LoadedEcPoint: LoadedEcPoint<C, Loader = Self>;

    fn ec_point_load_const(&self, value: &C) -> Self::LoadedEcPoint;

    fn ec_point_load_zero(&self) -> Self::LoadedEcPoint {
        self.ec_point_load_const(&C::identity())
    }

    fn ec_point_load_one(&self) -> Self::LoadedEcPoint {
        self.ec_point_load_const(&C::generator())
    }

    fn ec_point_assert_eq(
        &self,
        annotation: &str,
        lhs: &Self::LoadedEcPoint,
        rhs: &Self::LoadedEcPoint,
    ) -> Result<(), Error>;
}

pub trait ScalarLoader<F: PrimeField> {
    type LoadedScalar: LoadedScalar<F, Loader = Self>;

    fn load_const(&self, value: &F) -> Self::LoadedScalar;

    fn load_zero(&self) -> Self::LoadedScalar {
        self.load_const(&F::zero())
    }

    fn load_one(&self) -> Self::LoadedScalar {
        self.load_const(&F::one())
    }

    fn assert_eq(
        &self,
        annotation: &str,
        lhs: &Self::LoadedScalar,
        rhs: &Self::LoadedScalar,
    ) -> Result<(), Error>;

    fn sum_with_coeff_and_constant(
        &self,
        values: &[(F, &Self::LoadedScalar)],
        constant: F,
    ) -> Self::LoadedScalar {
        if values.is_empty() {
            return self.load_const(&constant);
        }

        let loader = values.first().unwrap().1.loader();
        iter::empty()
            .chain(if constant == F::zero() { None } else { Some(loader.load_const(&constant)) })
            .chain(values.iter().map(|&(coeff, value)| {
                if coeff == F::one() {
                    value.clone()
                } else {
                    loader.load_const(&coeff) * value
                }
            }))
            .reduce(|acc, term| acc + term)
            .unwrap()
    }

    fn sum_products_with_coeff_and_constant(
        &self,
        values: &[(F, &Self::LoadedScalar, &Self::LoadedScalar)],
        constant: F,
    ) -> Self::LoadedScalar {
        if values.is_empty() {
            return self.load_const(&constant);
        }

        let loader = values.first().unwrap().1.loader();
        iter::empty()
            .chain(if constant == F::zero() { None } else { Some(loader.load_const(&constant)) })
            .chain(values.iter().map(|&(coeff, lhs, rhs)| {
                if coeff == F::one() {
                    lhs.clone() * rhs
                } else {
                    loader.load_const(&coeff) * lhs * rhs
                }
            }))
            .reduce(|acc, term| acc + term)
            .unwrap()
    }

    fn sum_with_coeff(&self, values: &[(F, &Self::LoadedScalar)]) -> Self::LoadedScalar {
        self.sum_with_coeff_and_constant(values, F::zero())
    }

    fn sum_products_with_coeff(
        &self,
        values: &[(F, &Self::LoadedScalar, &Self::LoadedScalar)],
    ) -> Self::LoadedScalar {
        self.sum_products_with_coeff_and_constant(values, F::zero())
    }

    fn sum_products(
        &self,
        values: &[(&Self::LoadedScalar, &Self::LoadedScalar)],
    ) -> Self::LoadedScalar {
        self.sum_products_with_coeff_and_constant(
            &values.iter().map(|&(lhs, rhs)| (F::one(), lhs, rhs)).collect_vec(),
            F::zero(),
        )
    }

    fn sum_with_const(&self, values: &[&Self::LoadedScalar], constant: F) -> Self::LoadedScalar {
        self.sum_with_coeff_and_constant(
            &values.iter().map(|&value| (F::one(), value)).collect_vec(),
            constant,
        )
    }

    fn sum(&self, values: &[&Self::LoadedScalar]) -> Self::LoadedScalar {
        self.sum_with_const(values, F::zero())
    }

    fn product(&self, values: &[&Self::LoadedScalar]) -> Self::LoadedScalar {
        values.iter().fold(self.load_one(), |acc, value| acc * *value)
    }
}

pub trait Loader<C: CurveAffine>:
    EcPointLoader<C> + ScalarLoader<C::ScalarExt> + Clone + Debug
{
    fn ec_point_select(
        &self,
        _a: &Self::LoadedEcPoint,
        _b: &Self::LoadedEcPoint,
        _sel: &Self::LoadedScalar,
    ) -> Result<Self::LoadedEcPoint, Error> {
        todo!()
    }

    fn start_cost_metering(&self, _: &str) {}

    fn end_cost_metering(&self) {}
}

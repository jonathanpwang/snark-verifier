#![allow(clippy::type_complexity)]

use common::*;
use halo2_curves::{
    bn256::{Bn256, Fq, Fr, G1Affine},
    group::ff::Field,
    CurveAffine,
};
use halo2_proofs::{
    circuit::{AssignedCell, Layouter, SimpleFloorPlanner, Value},
    dev::MockProver,
    plonk::{
        self, create_proof, keygen_pk, keygen_vk, Circuit, ConstraintSystem, Error, ProvingKey,
        Selector, VerifyingKey,
    },
    poly::{
        commitment::ParamsProver,
        kzg::{
            commitment::{KZGCommitmentScheme, ParamsKZG},
            multiopen::{ProverGWC, VerifierGWC},
            strategy::AccumulatorStrategy,
        },
        Rotation, VerificationStrategy,
    },
};
use itertools::Itertools;
use plonk_verifier::{
    loader::{self, native::NativeLoader, ScalarLoader},
    pcs::{
        kzg::{Gwc19, Kzg, KzgAccumulator, KzgAs, KzgSuccinctVerifyingKey, LimbsEncoding},
        AccumulationScheme, AccumulationSchemeProver,
    },
    system::{
        self,
        halo2::{compile, Config},
    },
    util::{
        arithmetic::{fe_to_fe, fe_to_limbs},
        hash,
    },
    verifier::{self, PlonkProof, PlonkVerifier},
    Protocol,
};
use rand::{rngs::OsRng, SeedableRng};
use rand_chacha::ChaCha20Rng;
use std::{fs, iter, marker::PhantomData, rc::Rc};

const LIMBS: usize = 4;
const BITS: usize = 68;
const T: usize = 5;
const RATE: usize = 4;
const R_F: usize = 8;
const R_P: usize = 60;

type Pcs = Kzg<Bn256, Gwc19>;
type Svk = KzgSuccinctVerifyingKey<G1Affine>;
type As = KzgAs<Pcs>;
type Plonk = verifier::Plonk<Pcs, LimbsEncoding<LIMBS, BITS>>;
type Poseidon<L> = hash::Poseidon<Fr, L, T, RATE>;
type PoseidonTranscript<L, S> =
    system::halo2::transcript::halo2::PoseidonTranscript<G1Affine, L, S, T, RATE, R_F, R_P>;

mod common {
    use super::*;
    use halo2_proofs::poly::commitment::Params;
    use plonk_verifier::{cost::CostEstimation, util::transcript::TranscriptWrite};

    pub struct Snark {
        pub protocol: Protocol<G1Affine>,
        pub instances: Vec<Vec<Fr>>,
        pub proof: Vec<u8>,
    }

    impl Snark {
        pub fn new(protocol: Protocol<G1Affine>, instances: Vec<Vec<Fr>>, proof: Vec<u8>) -> Self {
            Self {
                protocol,
                instances,
                proof,
            }
        }
    }

    impl From<Snark> for SnarkWitness {
        fn from(snark: Snark) -> Self {
            Self {
                protocol: snark.protocol,
                instances: snark
                    .instances
                    .into_iter()
                    .map(|instances| instances.into_iter().map(Value::known).collect_vec())
                    .collect(),
                proof: Value::known(snark.proof),
            }
        }
    }

    #[derive(Clone)]
    pub struct SnarkWitness {
        pub protocol: Protocol<G1Affine>,
        pub instances: Vec<Vec<Value<Fr>>>,
        pub proof: Value<Vec<u8>>,
    }

    impl SnarkWitness {
        pub fn without_witnesses(&self) -> Self {
            SnarkWitness {
                protocol: self.protocol.clone(),
                instances: self
                    .instances
                    .iter()
                    .map(|instances| vec![Value::unknown(); instances.len()])
                    .collect(),
                proof: Value::unknown(),
            }
        }

        pub fn proof(&self) -> Value<&[u8]> {
            self.proof.as_ref().map(Vec::as_slice)
        }
    }

    pub trait CircuitExt<F: Field>: Circuit<F> {
        fn num_instance() -> Vec<usize>;

        fn instances(&self) -> Vec<Vec<F>>;

        fn accumulator_indices() -> Option<Vec<(usize, usize)>> {
            None
        }
    }

    pub fn gen_srs(k: u32) -> ParamsKZG<Bn256> {
        let path = format!("./examples/k-{}.srs", k);
        match fs::File::open(path.as_str()) {
            Ok(mut file) => ParamsKZG::read(&mut file).unwrap(),
            Err(_) => {
                let params =
                    ParamsKZG::<Bn256>::setup(k, ChaCha20Rng::from_seed(Default::default()));
                params.write(&mut fs::File::create(path).unwrap()).unwrap();
                params
            }
        }
    }

    pub fn gen_pk<C: Circuit<Fr>>(params: &ParamsKZG<Bn256>, circuit: &C) -> ProvingKey<G1Affine> {
        let vk = keygen_vk(params, circuit).unwrap();
        keygen_pk(params, vk, circuit).unwrap()
    }

    pub fn gen_proof<C: Circuit<Fr>>(
        params: &ParamsKZG<Bn256>,
        pk: &ProvingKey<G1Affine>,
        circuit: C,
        instances: Vec<Vec<Fr>>,
    ) -> Vec<u8> {
        if params.k() > 3 {
            MockProver::run(params.k(), &circuit, instances.clone())
                .unwrap()
                .assert_satisfied();
        }

        let instances = instances
            .iter()
            .map(|instances| instances.as_slice())
            .collect_vec();
        let proof = {
            let mut transcript = PoseidonTranscript::<NativeLoader, _>::new(Vec::new());
            create_proof::<
                KZGCommitmentScheme<Bn256>,
                ProverGWC<_>,
                _,
                _,
                PoseidonTranscript<NativeLoader, _>,
                _,
            >(
                params,
                pk,
                &[circuit],
                &[instances.as_slice()],
                OsRng,
                &mut transcript,
            )
            .unwrap();
            transcript.finalize()
        };

        let accept = {
            let mut transcript = PoseidonTranscript::<NativeLoader, _>::new(proof.as_slice());
            VerificationStrategy::<_, VerifierGWC<_>>::finalize(
            plonk::verify_proof::<_, VerifierGWC<_>, _, PoseidonTranscript<NativeLoader, _>, _>(
                params.verifier_params(),
                pk.get_vk(),
                AccumulatorStrategy::new(params.verifier_params()),
                &[instances.as_slice()],
                &mut transcript,
            )
            .unwrap(),
        )
        };
        assert!(accept);

        proof
    }

    pub fn gen_snark<ConcreteCircuit: CircuitExt<Fr>>(
        params: &ParamsKZG<Bn256>,
        pk: &ProvingKey<G1Affine>,
        circuit: ConcreteCircuit,
    ) -> Snark {
        let protocol = compile(
            params,
            pk.get_vk(),
            Config::kzg()
                .with_num_instance(ConcreteCircuit::num_instance())
                .with_accumulator_indices(ConcreteCircuit::accumulator_indices()),
        );

        let instances = circuit.instances();
        let proof = gen_proof(params, pk, circuit, instances.clone());

        Snark::new(protocol, instances, proof)
    }

    pub fn gen_dummy_snark<ConcreteCircuit: CircuitExt<Fr>>(
        params: &ParamsKZG<Bn256>,
        vk: Option<&VerifyingKey<G1Affine>>,
    ) -> Snark {
        struct CsProxy<F, C>(PhantomData<(F, C)>);

        impl<F: Field, C: Circuit<F>> Circuit<F> for CsProxy<F, C> {
            type Config = C::Config;
            type FloorPlanner = C::FloorPlanner;

            fn without_witnesses(&self) -> Self {
                CsProxy(PhantomData)
            }

            fn configure(meta: &mut plonk::ConstraintSystem<F>) -> Self::Config {
                C::configure(meta)
            }

            fn synthesize(&self, _: Self::Config, _: impl Layouter<F>) -> Result<(), plonk::Error> {
                Ok(())
            }
        }

        let dummy_vk = vk
            .is_none()
            .then(|| keygen_vk(params, &CsProxy::<Fr, ConcreteCircuit>(PhantomData)).unwrap());
        let protocol = compile(
            params,
            vk.or(dummy_vk.as_ref()).unwrap(),
            Config::kzg()
                .with_num_instance(ConcreteCircuit::num_instance())
                .with_accumulator_indices(ConcreteCircuit::accumulator_indices()),
        );
        let instances = ConcreteCircuit::num_instance()
            .into_iter()
            .map(|n| iter::repeat_with(|| Fr::random(OsRng)).take(n).collect())
            .collect();
        let proof = {
            let mut transcript = PoseidonTranscript::<NativeLoader, _>::new(Vec::new());
            for _ in 0..protocol
                .num_witness
                .iter()
                .chain(Some(&protocol.quotient.num_chunk()))
                .sum::<usize>()
            {
                transcript.write_ec_point(G1Affine::random(OsRng)).unwrap();
            }
            for _ in 0..protocol.evaluations.len() {
                transcript.write_scalar(Fr::random(OsRng)).unwrap();
            }
            let queries = PlonkProof::<G1Affine, NativeLoader, Pcs>::empty_queries(&protocol);
            for _ in 0..Pcs::estimate_cost(&queries).num_commitment {
                transcript.write_ec_point(G1Affine::random(OsRng)).unwrap();
            }
            transcript.finalize()
        };

        Snark::new(protocol, instances, proof)
    }
}

mod application {
    use super::*;

    #[derive(Clone, Default)]
    pub struct Square(Fr);

    impl Circuit<Fr> for Square {
        type Config = Selector;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<Fr>) -> Self::Config {
            let q = meta.selector();
            let i = meta.instance_column();
            meta.create_gate("square", |meta| {
                let q = meta.query_selector(q);
                let [i, i_w] = [0, 1].map(|rotation| meta.query_instance(i, Rotation(rotation)));
                Some(q * (i.clone() * i - i_w))
            });
            q
        }

        fn synthesize(
            &self,
            q: Self::Config,
            mut layouter: impl Layouter<Fr>,
        ) -> Result<(), Error> {
            layouter.assign_region(|| "", |mut region| q.enable(&mut region, 0))
        }
    }

    impl CircuitExt<Fr> for Square {
        fn num_instance() -> Vec<usize> {
            vec![2]
        }

        fn instances(&self) -> Vec<Vec<Fr>> {
            vec![vec![self.0, self.0.square()]]
        }
    }

    impl recursion::StateTransition for Square {
        type Input = ();

        fn new(state: Fr) -> Self {
            Self(state)
        }

        fn state_transition(&self, _: Self::Input) -> Fr {
            self.0.square()
        }
    }
}

mod recursion {
    use super::*;
    use halo2_wrong_ecc::{
        integer::rns::Rns,
        maingate::{
            MainGate, MainGateConfig, MainGateInstructions, RangeChip, RangeConfig,
            RangeInstructions, RegionCtx,
        },
        EccConfig,
    };

    type BaseFieldEccChip = halo2_wrong_ecc::BaseFieldEccChip<G1Affine, LIMBS, BITS>;
    type Halo2Loader<'a> = loader::halo2::Halo2Loader<'a, G1Affine, BaseFieldEccChip>;

    pub trait StateTransition {
        type Input;

        fn new(state: Fr) -> Self;

        fn state_transition(&self, input: Self::Input) -> Fr;
    }

    fn succinct_verify<'a>(
        svk: &Svk,
        loader: &Rc<Halo2Loader<'a>>,
        snark: &SnarkWitness,
        expected_preprocessed_digest: Option<AssignedCell<Fr, Fr>>,
    ) -> (
        Vec<Vec<AssignedCell<Fr, Fr>>>,
        Vec<KzgAccumulator<G1Affine, Rc<Halo2Loader<'a>>>>,
    ) {
        let protocol = if let Some(expected_preprocessed_digest) = expected_preprocessed_digest {
            let protocol = snark.protocol.loaded_preprocessed_as_witness(loader);
            let inputs = protocol
                .preprocessed
                .iter()
                .flat_map(|preprocessed| {
                    let assigned = preprocessed.assigned();
                    [assigned.x(), assigned.y()]
                        .map(|coordinate| loader.scalar_from_assigned(coordinate.native().clone()))
                })
                .chain(protocol.transcript_initial_state.clone())
                .collect_vec();
            let preprocessed_digest = {
                let mut hasher = Poseidon::new(loader, R_F, R_P);
                hasher.update(&inputs);
                hasher.squeeze()
            };
            let expected_preprocessed_digest =
                loader.scalar_from_assigned(expected_preprocessed_digest);
            loader
                .assert_eq("", &preprocessed_digest, &expected_preprocessed_digest)
                .unwrap();
            protocol
        } else {
            snark.protocol.loaded(loader)
        };

        let instances = snark
            .instances
            .iter()
            .map(|instances| {
                instances
                    .iter()
                    .map(|instance| loader.assign_scalar(*instance))
                    .collect_vec()
            })
            .collect_vec();
        let mut transcript = PoseidonTranscript::<Rc<Halo2Loader>, _>::new(loader, snark.proof());
        let proof = Plonk::read_proof(svk, &protocol, &instances, &mut transcript).unwrap();
        let accumulators = Plonk::succinct_verify(svk, &protocol, &instances, &proof).unwrap();

        (
            instances
                .into_iter()
                .map(|instance| {
                    instance
                        .into_iter()
                        .map(|instance| instance.assigned())
                        .collect()
                })
                .collect(),
            accumulators,
        )
    }

    fn select_accumulator<'a>(
        loader: &Rc<Halo2Loader<'a>>,
        condition: &AssignedCell<Fr, Fr>,
        lhs: &KzgAccumulator<G1Affine, Rc<Halo2Loader<'a>>>,
        rhs: &KzgAccumulator<G1Affine, Rc<Halo2Loader<'a>>>,
    ) -> Result<KzgAccumulator<G1Affine, Rc<Halo2Loader<'a>>>, Error> {
        let [lhs, rhs]: [_; 2] = [lhs.lhs.assigned(), lhs.rhs.assigned()]
            .iter()
            .zip([rhs.lhs.assigned(), rhs.rhs.assigned()].iter())
            .map(|(lhs, rhs)| {
                loader
                    .ecc_chip()
                    .select(&mut loader.ctx_mut(), condition, lhs, rhs)
            })
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .unwrap();
        Ok(KzgAccumulator::new(
            loader.ec_point_from_assigned(lhs),
            loader.ec_point_from_assigned(rhs),
        ))
    }

    fn accumulate<'a>(
        loader: &Rc<Halo2Loader<'a>>,
        accumulators: Vec<KzgAccumulator<G1Affine, Rc<Halo2Loader<'a>>>>,
        as_proof: Value<&'_ [u8]>,
    ) -> KzgAccumulator<G1Affine, Rc<Halo2Loader<'a>>> {
        let mut transcript = PoseidonTranscript::<Rc<Halo2Loader>, _>::new(loader, as_proof);
        let proof = As::read_proof(&Default::default(), &accumulators, &mut transcript).unwrap();
        As::verify(&Default::default(), &accumulators, &proof).unwrap()
    }

    #[derive(Clone)]
    pub struct RecursionConfig {
        main_gate_config: MainGateConfig,
        range_config: RangeConfig,
    }

    impl RecursionConfig {
        pub fn main_gate(&self) -> MainGate<Fr> {
            MainGate::new(self.main_gate_config.clone())
        }

        pub fn range_chip(&self) -> RangeChip<Fr> {
            RangeChip::new(self.range_config.clone())
        }

        pub fn ecc_chip(&self) -> BaseFieldEccChip {
            BaseFieldEccChip::new(EccConfig::new(
                self.range_config.clone(),
                self.main_gate_config.clone(),
            ))
        }
    }

    #[derive(Clone)]
    pub struct RecursionCircuit {
        svk: Svk,
        default_accumulator: KzgAccumulator<G1Affine, NativeLoader>,
        app: SnarkWitness,
        previous: SnarkWitness,
        round: usize,
        instances: Vec<Fr>,
        as_proof: Value<Vec<u8>>,
    }

    impl RecursionCircuit {
        const PREPROCESSED_DIGEST_ROW: usize = 4 * LIMBS;
        const INITIAL_STATE_ROW: usize = 4 * LIMBS + 1;
        const STATE_ROW: usize = 4 * LIMBS + 2;
        const ROUND_ROW: usize = 4 * LIMBS + 3;

        pub fn new(
            params: &ParamsKZG<Bn256>,
            app: Snark,
            previous: Snark,
            initial_state: Fr,
            state: Fr,
            round: usize,
        ) -> Self {
            let svk = params.get_g()[0].into();
            let default_accumulator = KzgAccumulator::new(params.get_g()[1], params.get_g()[0]);

            let succinct_verify = |snark: &Snark| {
                let mut transcript =
                    PoseidonTranscript::<NativeLoader, _>::new(snark.proof.as_slice());
                let proof =
                    Plonk::read_proof(&svk, &snark.protocol, &snark.instances, &mut transcript)
                        .unwrap();
                Plonk::succinct_verify(&svk, &snark.protocol, &snark.instances, &proof).unwrap()
            };

            let accumulators = iter::empty()
                .chain(succinct_verify(&app))
                .chain(
                    (round > 0)
                        .then_some(succinct_verify(&previous))
                        .unwrap_or_else(|| vec![default_accumulator.clone(); 2]),
                )
                .collect_vec();

            let (accumulator, as_proof) = {
                let mut transcript = PoseidonTranscript::<NativeLoader, _>::new(Vec::new());
                let accumulator =
                    As::create_proof(&Default::default(), &accumulators, &mut transcript, OsRng)
                        .unwrap();
                (accumulator, transcript.finalize())
            };

            let preprocessed_digest = {
                let inputs = previous
                    .protocol
                    .preprocessed
                    .iter()
                    .flat_map(|preprocessed| {
                        let coordinates = preprocessed.coordinates().unwrap();
                        [*coordinates.x(), *coordinates.y()]
                    })
                    .map(fe_to_fe)
                    .chain(previous.protocol.transcript_initial_state)
                    .collect_vec();
                let mut poseidon = Poseidon::new(&NativeLoader, R_F, R_P);
                poseidon.update(&inputs);
                poseidon.squeeze()
            };
            let instances = [
                accumulator.lhs.x,
                accumulator.lhs.y,
                accumulator.rhs.x,
                accumulator.rhs.y,
            ]
            .into_iter()
            .flat_map(fe_to_limbs::<_, _, LIMBS, BITS>)
            .chain([
                preprocessed_digest,
                initial_state,
                state,
                Fr::from(round as u64),
            ])
            .collect();

            Self {
                svk,
                default_accumulator,
                app: app.into(),
                previous: previous.into(),
                round,
                instances,
                as_proof: Value::known(as_proof),
            }
        }

        fn initial_snark(params: &ParamsKZG<Bn256>, vk: Option<&VerifyingKey<G1Affine>>) -> Snark {
            let mut snark = gen_dummy_snark::<RecursionCircuit>(params, vk);
            let g = params.get_g();
            snark.instances = vec![[g[1].x, g[1].y, g[0].x, g[0].y]
                .into_iter()
                .flat_map(fe_to_limbs::<_, _, LIMBS, BITS>)
                .chain([Fr::zero(); 4])
                .collect_vec()];
            snark
        }

        fn as_proof(&self) -> Value<&[u8]> {
            self.as_proof.as_ref().map(Vec::as_slice)
        }

        fn load_default_accumulator<'a>(
            &self,
            loader: &Rc<Halo2Loader<'a>>,
        ) -> Result<KzgAccumulator<G1Affine, Rc<Halo2Loader<'a>>>, Error> {
            let [lhs, rhs] =
                [self.default_accumulator.lhs, self.default_accumulator.rhs].map(|default| {
                    loader
                        .ecc_chip()
                        .assign_constant(&mut loader.ctx_mut(), default)
                        .unwrap()
                });
            Ok(KzgAccumulator::new(
                loader.ec_point_from_assigned(lhs),
                loader.ec_point_from_assigned(rhs),
            ))
        }
    }

    impl Circuit<Fr> for RecursionCircuit {
        type Config = RecursionConfig;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self {
                svk: self.svk,
                default_accumulator: self.default_accumulator.clone(),
                app: self.app.without_witnesses(),
                previous: self.previous.without_witnesses(),
                round: self.round,
                instances: self.instances.clone(),
                as_proof: Value::unknown(),
            }
        }

        fn configure(meta: &mut plonk::ConstraintSystem<Fr>) -> Self::Config {
            let main_gate_config = MainGate::<Fr>::configure(meta);
            let range_config = RangeChip::<Fr>::configure(
                meta,
                &main_gate_config,
                vec![BITS / LIMBS],
                Rns::<Fq, Fr, LIMBS, BITS>::construct().overflow_lengths(),
            );
            RecursionConfig {
                main_gate_config,
                range_config,
            }
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<Fr>,
        ) -> Result<(), plonk::Error> {
            let main_gate = config.main_gate();
            let range_chip = config.range_chip();

            range_chip.load_table(&mut layouter)?;

            let [preprocessed_digest, initial_state, state, round, first_round, not_first_round] =
                layouter.assign_region(
                    || "",
                    |region| {
                        let mut ctx = RegionCtx::new(region, 0);
                        let [preprocessed_digest, initial_state, state, round] = [
                            self.instances[Self::PREPROCESSED_DIGEST_ROW],
                            self.instances[Self::INITIAL_STATE_ROW],
                            self.instances[Self::STATE_ROW],
                            self.instances[Self::ROUND_ROW],
                        ]
                        .map(|instance| {
                            main_gate
                                .assign_value(&mut ctx, Value::known(instance))
                                .unwrap()
                        });
                        let first_round = main_gate.is_zero(&mut ctx, &round)?;
                        let not_first_round = main_gate.not(&mut ctx, &first_round)?;
                        Ok([
                            preprocessed_digest,
                            initial_state,
                            state,
                            round,
                            first_round,
                            not_first_round,
                        ])
                    },
                )?;

            let (lhs, rhs, app_instances, previous_instances) = layouter.assign_region(
                || "",
                |region| {
                    let loader = Halo2Loader::new(config.ecc_chip(), RegionCtx::new(region, 0));
                    let (app_instances, app_accumulators) =
                        succinct_verify(&self.svk, &loader, &self.app, None);
                    let (previous_instances, previous_accumulators) = succinct_verify(
                        &self.svk,
                        &loader,
                        &self.previous,
                        Some(preprocessed_digest.clone()),
                    );

                    let default_accmulator = self.load_default_accumulator(&loader)?;
                    let previous_accumulators = previous_accumulators
                        .iter()
                        .map(|previous_accumulator| {
                            select_accumulator(
                                &loader,
                                &first_round,
                                &default_accmulator,
                                previous_accumulator,
                            )
                        })
                        .collect::<Result<Vec<_>, Error>>()?;

                    let KzgAccumulator { lhs, rhs } = accumulate(
                        &loader,
                        [app_accumulators, previous_accumulators].concat(),
                        self.as_proof(),
                    );

                    Ok((
                        lhs.assigned(),
                        rhs.assigned(),
                        app_instances,
                        previous_instances,
                    ))
                },
            )?;

            layouter.assign_region(
                || "",
                |region| {
                    let mut ctx = RegionCtx::new(region, 0);
                    for (lhs, rhs) in [
                        // Propagate preprocessed_digest
                        (
                            &main_gate.mul(&mut ctx, &preprocessed_digest, &not_first_round)?,
                            &previous_instances[0][Self::PREPROCESSED_DIGEST_ROW],
                        ),
                        // Propagate initial_state
                        (
                            &main_gate.mul(&mut ctx, &initial_state, &not_first_round)?,
                            &previous_instances[0][Self::INITIAL_STATE_ROW],
                        ),
                        // Verify initial_state is same as the first application snark
                        (
                            &main_gate.mul(&mut ctx, &initial_state, &first_round)?,
                            &main_gate.mul(&mut ctx, &app_instances[0][0], &first_round)?,
                        ),
                        // Verify current state is same as the current application snark
                        (&state, &app_instances[0][1]),
                        // Verify previous state is same as the current application snark
                        (
                            &main_gate.mul(&mut ctx, &app_instances[0][0], &not_first_round)?,
                            &previous_instances[0][Self::STATE_ROW],
                        ),
                        // Verify round is increased by 1 when not at first round
                        (
                            &round,
                            &main_gate.add(
                                &mut ctx,
                                &not_first_round,
                                &previous_instances[0][Self::ROUND_ROW],
                            )?,
                        ),
                    ] {
                        main_gate.assert_equal(&mut ctx, lhs, rhs)?;
                    }
                    Ok(())
                },
            )?;

            for (row, limb) in [lhs.x(), lhs.y(), rhs.x(), rhs.y()]
                .into_iter()
                .flat_map(|coordinate| coordinate.limbs())
                .map_into()
                .chain([preprocessed_digest, initial_state, state, round])
                .enumerate()
            {
                main_gate.expose_public(layouter.namespace(|| ""), limb, row)?;
            }

            Ok(())
        }
    }

    impl CircuitExt<Fr> for RecursionCircuit {
        fn num_instance() -> Vec<usize> {
            // [..lhs, ..rhs, preprocessed_digest, initial_state, state, round]
            vec![4 * LIMBS + 4]
        }

        fn instances(&self) -> Vec<Vec<Fr>> {
            vec![self.instances.clone()]
        }

        fn accumulator_indices() -> Option<Vec<(usize, usize)>> {
            Some((0..4 * LIMBS).map(|idx| (0, idx)).collect())
        }
    }

    pub fn gen_recursion_pk<ConcreteCircuit: CircuitExt<Fr>>(
        app_params: &ParamsKZG<Bn256>,
        recursion_params: &ParamsKZG<Bn256>,
        app_vk: &VerifyingKey<G1Affine>,
    ) -> ProvingKey<G1Affine> {
        let recursion = RecursionCircuit::new(
            recursion_params,
            gen_dummy_snark::<ConcreteCircuit>(app_params, Some(app_vk)),
            RecursionCircuit::initial_snark(recursion_params, None),
            Fr::zero(),
            Fr::zero(),
            0,
        );
        gen_pk(recursion_params, &recursion)
    }

    pub fn gen_recursion_snark<ConcreteCircuit: CircuitExt<Fr> + StateTransition>(
        app_params: &ParamsKZG<Bn256>,
        recursion_params: &ParamsKZG<Bn256>,
        app_pk: &ProvingKey<G1Affine>,
        recursion_pk: &ProvingKey<G1Affine>,
        initial_state: Fr,
        inputs: Vec<ConcreteCircuit::Input>,
    ) -> (Fr, Snark) {
        let mut state = initial_state;
        let mut app = ConcreteCircuit::new(state);
        let mut previous =
            RecursionCircuit::initial_snark(recursion_params, Some(recursion_pk.get_vk()));
        for (round, input) in inputs.into_iter().enumerate() {
            state = app.state_transition(input);
            let recursion = RecursionCircuit::new(
                recursion_params,
                gen_snark(app_params, app_pk, app),
                previous,
                initial_state,
                state,
                round,
            );
            previous = gen_snark(recursion_params, recursion_pk, recursion);
            app = ConcreteCircuit::new(state);
        }
        (state, previous)
    }
}

fn main() {
    let app_params = gen_srs(3);
    let recursion_params = gen_srs(22);

    let app_pk = gen_pk(&app_params, &application::Square::default());
    let recursion_pk = recursion::gen_recursion_pk::<application::Square>(
        &app_params,
        &recursion_params,
        app_pk.get_vk(),
    );

    let num_round = 3;
    let (final_state, snark) = recursion::gen_recursion_snark::<application::Square>(
        &app_params,
        &recursion_params,
        &app_pk,
        &recursion_pk,
        Fr::from(2),
        vec![(); num_round],
    );
    assert_eq!(final_state, Fr::from(256));

    let accept = {
        let svk = recursion_params.get_g()[0].into();
        let dk = (recursion_params.g2(), recursion_params.s_g2()).into();
        let mut transcript = PoseidonTranscript::<NativeLoader, _>::new(snark.proof.as_slice());
        let proof =
            Plonk::read_proof(&svk, &snark.protocol, &snark.instances, &mut transcript).unwrap();
        Plonk::verify(&svk, &dk, &snark.protocol, &snark.instances, &proof).unwrap()
    };
    assert!(accept)
}

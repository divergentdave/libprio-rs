// SPDX-License-Identifier: MPL-2.0

//! **(NOTE: This module is experimental. Applications should not use it yet.)** This modulde
//! implements the prio3 [VDAF]. The construction is based on a transform of a Fully Linear Proof
//! (FLP) system (i.e., a concrete [`Type`](crate::pcp::Type) into a zero-knowledge proof system on
//! distributed data as described in [[BBCG+19], Section 6].
//!
//! [BBCG+19]: https://ia.cr/2019/188
//! [BBCG+21]: https://ia.cr/2021/017
//! [VDAF]: https://datatracker.ietf.org/doc/draft-patton-cfrg-vdaf/

use crate::codec::{CodecError, Decode, Encode, ParameterizedDecode};
use crate::field::{Field128, Field64, FieldElement};
#[cfg(feature = "multithreaded")]
use crate::pcp::gadgets::ParallelSumMultithreaded;
use crate::pcp::gadgets::{BlindPolyEval, ParallelSum, ParallelSumGadget};
use crate::pcp::types::{Count, CountVec, Histogram, Sum};
use crate::pcp::Type;
use crate::prng::Prng;
use crate::vdaf::prg::{Prg, PrgAes128, Seed};
use crate::vdaf::{
    Aggregatable, AggregateShare, Aggregator, Client, Collector, OutputShare, PrepareTransition,
    Share, ShareDecodingParameter, Vdaf, VdafError,
};
use std::convert::{TryFrom, TryInto};
use std::fmt::Debug;
use std::io::Cursor;
use std::iter::IntoIterator;
use std::marker::PhantomData;

// TODO Add domain separation tag to field element generation as required by spec.

// TODO Add test vectors and make sure they pass.

/// The count type. Each measurement is an integer in `[0,2)` and the aggregate is the sum.
pub type Prio3Aes128Count = Prio3<Count<Field64>, Prio3Result<u64>, PrgAes128, 16>;

impl Prio3Aes128Count {
    /// Construct an instance of this VDAF with the given suite and the given number of aggregators.
    pub fn new(num_aggregators: u8) -> Result<Self, VdafError> {
        check_num_aggregators(num_aggregators)?;

        Ok(Prio3 {
            num_aggregators,
            typ: Count::new(),
            phantom: PhantomData,
        })
    }
}

/// The count-vector type. Each measurement is a vector of integers in `[0,2)` and the aggregate is
/// the element-wise sum.
pub type Prio3Aes128CountVec = Prio3<
    CountVec<Field128, ParallelSum<Field128, BlindPolyEval<Field128>>>,
    Prio3ResultVec<u64>,
    PrgAes128,
    16,
>;

/// Like [`Prio3CountVec`] except this type uses multithreading to improve sharding and
/// preparation time. Note that the improvement is only noticeable for very large input lengths,
/// e.g., 200 and up. (Your system's mileage may vary.)
#[cfg(feature = "multithreaded")]
#[cfg_attr(docsrs, doc(cfg(feature = "multithreaded")))]
pub type Prio3Aes128CountVecMultithreaded = Prio3<
    CountVec<Field128, ParallelSumMultithreaded<Field128, BlindPolyEval<Field128>>>,
    Prio3ResultVec<u64>,
    PrgAes128,
    16,
>;

impl<S, P, const L: usize> Prio3<CountVec<Field128, S>, Prio3ResultVec<u64>, P, L>
where
    S: 'static + ParallelSumGadget<Field128, BlindPolyEval<Field128>> + Eq,
    P: Prg<L>,
{
    /// Construct an instance of this VDAF with the given suite and the given number of
    /// aggregators. `len` defines the length of each measurement.
    pub fn new(num_aggregators: u8, len: usize) -> Result<Self, VdafError> {
        check_num_aggregators(num_aggregators)?;

        Ok(Prio3 {
            num_aggregators,
            typ: CountVec::new(len),
            phantom: PhantomData,
        })
    }
}

/// The sum type. Each measurement is an integer in `[0,2^bits)` for some `0 < bits < 64` and the
/// aggregate is the sum.
pub type Prio3Aes128Sum = Prio3<Sum<Field128>, Prio3Result<u64>, PrgAes128, 16>;

impl Prio3Aes128Sum {
    /// Construct an instance of this VDAF with the given suite, number of aggregators and required
    /// bit length. The bit length must not exceed 64.
    pub fn new(num_aggregators: u8, bits: u32) -> Result<Self, VdafError> {
        check_num_aggregators(num_aggregators)?;

        if bits > 64 {
            return Err(VdafError::Uncategorized(format!(
                "bit length ({}) exceeds limit for aggregate type (64)",
                bits
            )));
        }

        Ok(Prio3 {
            num_aggregators,
            typ: Sum::new(bits as usize)?,
            phantom: PhantomData,
        })
    }
}

/// the histogram type. Each measurement is an unsigned, 64-bit integer and the result is a
/// histogram representation of the measurement.
pub type Prio3Aes128Histogram = Prio3<Histogram<Field128>, Prio3ResultVec<u64>, PrgAes128, 16>;

impl Prio3Aes128Histogram {
    /// Constructs an instance of this VDAF with the given suite, number of aggregators, and
    /// desired histogram bucket boundaries.
    pub fn new(num_aggregators: u8, buckets: &[u64]) -> Result<Self, VdafError> {
        check_num_aggregators(num_aggregators)?;

        let buckets = buckets.iter().map(|bucket| *bucket as u128).collect();

        Ok(Prio3 {
            num_aggregators,
            typ: Histogram::<Field128>::new(buckets)?,
            phantom: PhantomData,
        })
    }
}

/// Aggregate result for singleton data types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prio3Result<T: Eq>(pub T);

impl<F: FieldElement> TryFrom<AggregateShare<F>> for Prio3Result<u64> {
    type Error = VdafError;

    fn try_from(data: AggregateShare<F>) -> Result<Self, VdafError> {
        if data.0.len() != 1 {
            return Err(VdafError::Uncategorized(format!(
                "unexpected aggregate length for count type: got {}; want 1",
                data.0.len()
            )));
        }

        let out: u64 = F::Integer::from(data.0[0]).try_into().map_err(|err| {
            VdafError::Uncategorized(format!("result too large for output type: {:?}", err))
        })?;

        Ok(Prio3Result(out))
    }
}

/// Aggregate result for vector data types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prio3ResultVec<T: Eq>(pub Vec<T>);

impl<F: FieldElement> TryFrom<AggregateShare<F>> for Prio3ResultVec<u64> {
    type Error = VdafError;

    fn try_from(data: AggregateShare<F>) -> Result<Self, VdafError> {
        let mut out = Vec::with_capacity(data.0.len());
        for elem in data.0.into_iter() {
            out.push(F::Integer::from(elem).try_into().map_err(|err| {
                VdafError::Uncategorized(format!("result too large for output type: {:?}", err))
            })?);
        }

        Ok(Prio3ResultVec(out))
    }
}

fn check_num_aggregators(num_aggregators: u8) -> Result<(), VdafError> {
    if num_aggregators == 0 {
        return Err(VdafError::Uncategorized(format!(
            "at least one aggregator is required; got {}",
            num_aggregators
        )));
    } else if num_aggregators > 254 {
        return Err(VdafError::Uncategorized(format!(
            "number of aggregators must not exceed 254; got {}",
            num_aggregators
        )));
    }

    Ok(())
}

/// The base type for prio3.
#[derive(Clone, Debug)]
pub struct Prio3<T, A, P, const L: usize>
where
    T: Type,
    A: Clone + Debug,
    P: Prg<L>,
{
    num_aggregators: u8,
    typ: T,
    phantom: PhantomData<(A, P)>,
}

impl<T, A, P, const L: usize> Prio3<T, A, P, L>
where
    T: Type,
    A: Clone + Debug,
    P: Prg<L>,
{
    /// The output length of the underlying FLP.
    pub fn output_len(&self) -> usize {
        self.typ.output_len()
    }

    /// The verifier length of the underlying FLP.
    pub fn verifier_len(&self) -> usize {
        self.typ.verifier_len()
    }
}

impl<T, A, P, const L: usize> Vdaf for Prio3<T, A, P, L>
where
    T: Type,
    A: Clone + Debug + Sync + Send,
    P: Prg<L>,
{
    type Measurement = T::Measurement;
    type AggregateResult = A;
    type AggregationParam = ();
    type PublicParam = ();
    type VerifyParam = Prio3VerifyParam<L>;
    type InputShare = Prio3InputShare<T::Field, L>;
    type OutputShare = OutputShare<T::Field>;
    type AggregateShare = AggregateShare<T::Field>;

    fn setup(&self) -> Result<((), Vec<Prio3VerifyParam<L>>), VdafError> {
        let query_rand_init = Seed::generate()?;
        Ok((
            (),
            (0..self.num_aggregators)
                .map(|aggregator_id| Prio3VerifyParam {
                    query_rand_init: query_rand_init.clone(),
                    aggregator_id,
                    input_len: self.typ.input_len(),
                    proof_len: self.typ.proof_len(),
                    joint_rand_len: self.typ.joint_rand_len(),
                })
                .collect(),
        ))
    }

    fn num_aggregators(&self) -> usize {
        self.num_aggregators as usize
    }
}

/// The verification parameter used by each aggregator to evaluate the VDAF.
#[derive(Clone, Debug)]
pub struct Prio3VerifyParam<const L: usize> {
    /// Key used to derive the query randomness from the nonce.
    pub query_rand_init: Seed<L>,

    /// The identity of the aggregator.
    pub aggregator_id: u8,

    /// Length in field elements of an uncompressed input share.
    input_len: usize,

    /// Length in field elements of an uncompressed proof.
    proof_len: usize,

    /// Length of the joint randomness.
    joint_rand_len: usize,
}

/// The message sent by the client to each aggregator. This includes the client's input share and
/// the initial message of the input-validation protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Prio3InputShare<F, const L: usize> {
    /// The input share.
    input_share: Share<F, L>,

    /// The proof share.
    proof_share: Share<F, L>,

    /// Parameters used by the Aggregator to compute the joint randomness. This field is optional
    /// because not every [`pcp::Type`] requires joint randomness.
    joint_rand_param: Option<JointRandParam<L>>,
}

impl<F: FieldElement, const L: usize> Encode for Prio3InputShare<F, L> {
    fn encode(&self, bytes: &mut Vec<u8>) {
        if matches!(
            (&self.input_share, &self.proof_share),
            (Share::Leader(_), Share::Helper(_)) | (Share::Helper(_), Share::Leader(_))
        ) {
            panic!("tried to encode input share with ambiguous encoding")
        }

        self.input_share.encode(bytes);
        self.proof_share.encode(bytes);
        if let Some(ref param) = self.joint_rand_param {
            param.seed_hint.encode(bytes);
            param.blind.encode(bytes);
        }
    }
}

impl<F: FieldElement, const L: usize> ParameterizedDecode<Prio3VerifyParam<L>>
    for Prio3InputShare<F, L>
{
    fn decode_with_param(
        decoding_parameter: &Prio3VerifyParam<L>,
        bytes: &mut Cursor<&[u8]>,
    ) -> Result<Self, CodecError> {
        let (input_decoding_parameter, proof_decoding_parameter) =
            if decoding_parameter.aggregator_id == 0 {
                (
                    ShareDecodingParameter::Leader(decoding_parameter.input_len),
                    ShareDecodingParameter::Leader(decoding_parameter.proof_len),
                )
            } else {
                (
                    ShareDecodingParameter::Helper,
                    ShareDecodingParameter::Helper,
                )
            };

        let input_share = Share::decode_with_param(&input_decoding_parameter, bytes)?;
        let proof_share = Share::decode_with_param(&proof_decoding_parameter, bytes)?;
        let joint_rand_param = if decoding_parameter.joint_rand_len > 0 {
            Some(JointRandParam {
                seed_hint: Seed::decode(bytes)?,
                blind: Seed::decode(bytes)?,
            })
        } else {
            None
        };

        Ok(Prio3InputShare {
            input_share,
            proof_share,
            joint_rand_param,
        })
    }
}

#[derive(Clone, Debug)]
/// The verification message emitted by each aggregator during the Prepare process.
pub struct Prio3PrepareMessage<F, const L: usize> {
    /// (A share of) the FLP verifier message. (See [`Type`](crate::pcp::Type).)
    pub verifier: Vec<F>,

    /// (A share of) the joint randomness seed.
    pub joint_rand_seed: Option<Seed<L>>,
}

impl<F: FieldElement, const L: usize> Encode for Prio3PrepareMessage<F, L> {
    fn encode(&self, bytes: &mut Vec<u8>) {
        for x in &self.verifier {
            x.encode(bytes);
        }
        if let Some(ref seed) = self.joint_rand_seed {
            seed.encode(bytes);
        }
    }
}

impl<F: FieldElement, const L: usize> ParameterizedDecode<Prio3PrepareStep<F, L>>
    for Prio3PrepareMessage<F, L>
{
    fn decode_with_param(
        decoding_parameter: &Prio3PrepareStep<F, L>,
        bytes: &mut Cursor<&[u8]>,
    ) -> Result<Self, CodecError> {
        let verifier_len = decoding_parameter.verifier_len();
        let mut verifier = Vec::with_capacity(verifier_len);
        for _ in 0..verifier_len {
            verifier.push(F::decode(bytes)?);
        }

        let joint_rand_seed = if decoding_parameter.check_joint_rand() {
            Some(Seed::decode(bytes)?)
        } else {
            None
        };

        Ok(Prio3PrepareMessage {
            verifier,
            joint_rand_seed,
        })
    }
}

impl<T, A, P, const L: usize> Client for Prio3<T, A, P, L>
where
    T: Type,
    A: Clone + Debug + Sync + Send,
    P: Prg<L>,
{
    fn shard(
        &self,
        _public_param: &(),
        measurement: &T::Measurement,
    ) -> Result<Vec<Prio3InputShare<T::Field, L>>, VdafError> {
        let num_aggregators = self.num_aggregators;
        let input = self.typ.encode(measurement)?;

        // Generate the input shares and compute the joint randomness.
        let mut helper_shares = Vec::with_capacity(num_aggregators as usize - 1);
        let mut leader_input_share = input.clone();
        let mut joint_rand_seed = Seed::uninitialized();
        for aggregator_id in 1..num_aggregators {
            let mut helper = HelperShare::new()?;

            let mut deriver = P::init(&helper.joint_rand_param.blind);
            deriver.update(&[aggregator_id]);
            let prng: Prng<T::Field, _> =
                Prng::from_seed_stream(P::seed_stream(&helper.input_share, b"input share"));
            for (x, y) in leader_input_share
                .iter_mut()
                .zip(prng)
                .take(self.typ.input_len())
            {
                *x -= y;
                deriver.update(&y.into());
            }

            helper.joint_rand_param.seed_hint = deriver.into_seed();
            joint_rand_seed.xor_accumulate(&helper.joint_rand_param.seed_hint);

            helper_shares.push(helper);
        }

        let leader_blind = Seed::generate()?;

        let mut deriver = P::init(&leader_blind);
        deriver.update(&[0]); // ID of the leader
        for x in leader_input_share.iter() {
            deriver.update(&(*x).into());
        }

        let mut leader_joint_rand_seed_hint = deriver.into_seed();
        joint_rand_seed.xor_accumulate(&leader_joint_rand_seed_hint);

        // Run the proof-generation algorithm.
        let prng: Prng<T::Field, _> =
            Prng::from_seed_stream(P::seed_stream(&joint_rand_seed, b"joint rand"));
        let joint_rand: Vec<T::Field> = prng.take(self.typ.joint_rand_len()).collect();
        let prng: Prng<T::Field, _> =
            Prng::from_seed_stream(P::seed_stream(&Seed::generate()?, b"prove rand"));
        let prove_rand: Vec<T::Field> = prng.take(self.typ.prove_rand_len()).collect();
        let mut leader_proof_share = self.typ.prove(&input, &prove_rand, &joint_rand)?;

        // Generate the proof shares and finalize the joint randomness seed hints.
        for helper in helper_shares.iter_mut() {
            let prng: Prng<T::Field, _> =
                Prng::from_seed_stream(P::seed_stream(&helper.proof_share, b"proof share"));
            for (x, y) in leader_proof_share
                .iter_mut()
                .zip(prng)
                .take(self.typ.proof_len())
            {
                *x -= y;
            }

            helper
                .joint_rand_param
                .seed_hint
                .xor_accumulate(&joint_rand_seed);
        }

        leader_joint_rand_seed_hint.xor_accumulate(&joint_rand_seed);

        let leader_joint_rand_param = if self.typ.joint_rand_len() > 0 {
            Some(JointRandParam {
                seed_hint: leader_joint_rand_seed_hint,
                blind: leader_blind,
            })
        } else {
            None
        };

        // Prep the output messages.
        let mut out = Vec::with_capacity(num_aggregators as usize);
        out.push(Prio3InputShare {
            input_share: Share::Leader(leader_input_share),
            proof_share: Share::Leader(leader_proof_share),
            joint_rand_param: leader_joint_rand_param,
        });

        for helper in helper_shares.into_iter() {
            let helper_joint_rand_param = if self.typ.joint_rand_len() > 0 {
                Some(helper.joint_rand_param)
            } else {
                None
            };

            out.push(Prio3InputShare {
                input_share: Share::Helper(helper.input_share),
                proof_share: Share::Helper(helper.proof_share),
                joint_rand_param: helper_joint_rand_param,
            });
        }

        Ok(out)
    }
}

/// State of each aggregator during the Prepare process.
#[allow(missing_docs)]
#[derive(Clone, Debug)]
pub enum Prio3PrepareStep<F, const L: usize> {
    /// Ready to send the verifier message.
    Ready {
        input_share: Share<F, L>,
        joint_rand_seed: Option<Seed<L>>,
        verifier_msg: Prio3PrepareMessage<F, L>,
    },
    /// Waiting for the set of verifier messages.
    Waiting {
        input_share: Share<F, L>,
        joint_rand_seed: Option<Seed<L>>,
        verifier_len: usize,
    },
}

impl<F, const L: usize> Prio3PrepareStep<F, L> {
    fn verifier_len(&self) -> usize {
        match self {
            Self::Ready { verifier_msg, .. } => verifier_msg.verifier.len(),
            Self::Waiting { verifier_len, .. } => *verifier_len,
        }
    }

    fn check_joint_rand(&self) -> bool {
        let joint_rand_seed = match self {
            Self::Ready {
                joint_rand_seed, ..
            } => joint_rand_seed,
            Self::Waiting {
                joint_rand_seed, ..
            } => joint_rand_seed,
        };

        if joint_rand_seed.is_some() {
            return true;
        }
        false
    }
}

impl<T, A, P, const L: usize> Aggregator for Prio3<T, A, P, L>
where
    T: Type,
    A: Clone + Debug + Sync + Send,
    P: Prg<L>,
{
    type PrepareStep = Prio3PrepareStep<T::Field, L>;
    type PrepareMessage = Prio3PrepareMessage<T::Field, L>;

    /// Begins the Prep process with the other aggregators. The result of this process is
    /// the aggregator's output share.
    fn prepare_init(
        &self,
        verify_param: &Prio3VerifyParam<L>,
        _agg_param: &(),
        nonce: &[u8],
        msg: &Prio3InputShare<T::Field, L>,
    ) -> Result<Prio3PrepareStep<T::Field, L>, VdafError> {
        let mut deriver = P::init(&verify_param.query_rand_init);
        deriver.update(&[255]);
        deriver.update(nonce);
        let query_rand_seed = deriver.into_seed();

        // Create a reference to the (expanded) input share.
        let expanded_input_share: Option<Vec<T::Field>> = match msg.input_share {
            Share::Leader(_) => None,
            Share::Helper(ref seed) => {
                let prng = Prng::from_seed_stream(P::seed_stream(seed, b"input share"));
                Some(prng.take(self.typ.input_len()).collect())
            }
        };
        let input_share = match msg.input_share {
            Share::Leader(ref data) => data,
            Share::Helper(_) => expanded_input_share.as_ref().unwrap(),
        };

        // Create a reference to the (expanded) proof share.
        let expanded_proof_share: Option<Vec<T::Field>> = match msg.proof_share {
            Share::Leader(_) => None,
            Share::Helper(ref seed) => {
                let prng = Prng::from_seed_stream(P::seed_stream(seed, b"proof share"));
                Some(prng.take(self.typ.proof_len()).collect())
            }
        };
        let proof_share = match msg.proof_share {
            Share::Leader(ref data) => data,
            Share::Helper(_) => expanded_proof_share.as_ref().unwrap(),
        };

        // Compute the joint randomness.
        let (joint_rand_seed, joint_rand_seed_share, joint_rand) = if self.typ.joint_rand_len() > 0
        {
            let mut deriver = P::init(&msg.joint_rand_param.as_ref().unwrap().blind);
            deriver.update(&[verify_param.aggregator_id]);
            for x in input_share {
                deriver.update(&(*x).into());
            }
            let joint_rand_seed_share = deriver.into_seed();

            let mut joint_rand_seed = Seed::uninitialized();
            joint_rand_seed.xor(
                &msg.joint_rand_param.as_ref().unwrap().seed_hint,
                &joint_rand_seed_share,
            );

            let prng: Prng<T::Field, _> =
                Prng::from_seed_stream(P::seed_stream(&joint_rand_seed, b"joint rand"));
            (
                Some(joint_rand_seed),
                Some(joint_rand_seed_share),
                prng.take(self.typ.joint_rand_len()).collect(),
            )
        } else {
            (None, None, Vec::new())
        };

        // Compute the query randomness.
        let prng: Prng<T::Field, _> =
            Prng::from_seed_stream(P::seed_stream(&query_rand_seed, b"query rand"));
        let query_rand: Vec<T::Field> = prng.take(self.typ.query_rand_len()).collect();

        // Run the query-generation algorithm.
        let verifier_share = self.typ.query(
            input_share,
            proof_share,
            &query_rand,
            &joint_rand,
            self.num_aggregators as usize,
        )?;

        Ok(Prio3PrepareStep::Ready {
            input_share: msg.input_share.clone(),
            joint_rand_seed,
            verifier_msg: Prio3PrepareMessage {
                verifier: verifier_share,
                joint_rand_seed: joint_rand_seed_share,
            },
        })
    }

    fn prepare_preprocess<M: IntoIterator<Item = Prio3PrepareMessage<T::Field, L>>>(
        &self,
        inputs: M,
    ) -> Result<Self::PrepareMessage, VdafError> {
        let mut verifier = vec![T::Field::zero(); self.typ.verifier_len()];
        let mut joint_rand_seed = Seed::uninitialized();
        let mut count = 0;
        for share in inputs.into_iter() {
            count += 1;

            if share.verifier.len() != verifier.len() {
                return Err(VdafError::Uncategorized(format!(
                    "unexpected verifier share length: got {}; want {}",
                    share.verifier.len(),
                    verifier.len(),
                )));
            }

            if self.typ.joint_rand_len() > 0 {
                let joint_rand_seed_share = share.joint_rand_seed.unwrap();
                joint_rand_seed.xor_accumulate(&joint_rand_seed_share);
            }

            for (x, y) in verifier.iter_mut().zip(share.verifier) {
                *x += y;
            }
        }

        if count != self.num_aggregators {
            return Err(VdafError::Uncategorized(format!(
                "unexpected message count: got {}; want {}",
                count, self.num_aggregators,
            )));
        }

        let joint_rand_seed = if self.typ.joint_rand_len() > 0 {
            Some(joint_rand_seed)
        } else {
            None
        };

        Ok(Prio3PrepareMessage {
            verifier,
            joint_rand_seed,
        })
    }

    // TODO Fix this clippy warning instead of bypassing it.
    #[allow(clippy::type_complexity)]
    fn prepare_step(
        &self,
        state: Prio3PrepareStep<T::Field, L>,
        input: Option<Prio3PrepareMessage<T::Field, L>>,
    ) -> PrepareTransition<
        Prio3PrepareStep<T::Field, L>,
        Prio3PrepareMessage<T::Field, L>,
        OutputShare<T::Field>,
    > {
        match (state, input) {
            (
                Prio3PrepareStep::Ready {
                    input_share,
                    joint_rand_seed,
                    verifier_msg,
                },
                None,
            ) => PrepareTransition::Continue(
                Prio3PrepareStep::Waiting {
                    input_share,
                    joint_rand_seed,
                    verifier_len: verifier_msg.verifier.len(),
                },
                verifier_msg,
            ),

            (
                Prio3PrepareStep::Waiting {
                    input_share,
                    joint_rand_seed,
                    ..
                },
                Some(msg),
            ) => {
                if self.typ.joint_rand_len() > 0 {
                    // Check that the joint randomness was correct.
                    if joint_rand_seed.as_ref().unwrap() != msg.joint_rand_seed.as_ref().unwrap() {
                        return PrepareTransition::Fail(VdafError::Uncategorized(
                            "joint randomness mismatch".to_string(),
                        ));
                    }
                }

                // Check the proof.
                let res = match self.typ.decide(&msg.verifier) {
                    Ok(res) => res,
                    Err(err) => {
                        return PrepareTransition::Fail(VdafError::from(err));
                    }
                };

                if !res {
                    return PrepareTransition::Fail(VdafError::Uncategorized(
                        "proof check failed".to_string(),
                    ));
                }

                // Compute the output share.
                let input_share = match input_share {
                    Share::Leader(data) => data,
                    Share::Helper(seed) => {
                        let prng = Prng::from_seed_stream(P::seed_stream(&seed, b"input share"));
                        prng.take(self.typ.input_len()).collect()
                    }
                };

                let output_share = match self.typ.truncate(input_share) {
                    Ok(data) => OutputShare(data),
                    Err(err) => {
                        return PrepareTransition::Fail(VdafError::from(err));
                    }
                };

                PrepareTransition::Finish(output_share)
            }
            _ => PrepareTransition::Fail(VdafError::Uncategorized(
                "invalid state transition".to_string(),
            )),
        }
    }

    /// Aggregates a sequence of output shares into an aggregate share.
    fn aggregate<It: IntoIterator<Item = OutputShare<T::Field>>>(
        &self,
        _agg_param: &(),
        output_shares: It,
    ) -> Result<AggregateShare<T::Field>, VdafError> {
        let mut agg_share = AggregateShare(vec![T::Field::zero(); self.typ.output_len()]);
        for output_share in output_shares.into_iter() {
            agg_share.accumulate(&output_share)?;
        }

        Ok(agg_share)
    }
}

impl<T, A, P, const L: usize> Collector for Prio3<T, A, P, L>
where
    T: Type,
    A: Clone + Debug + Sync + Send + TryFrom<AggregateShare<T::Field>, Error = VdafError> + Eq,
    P: Prg<L>,
{
    /// Combines aggregate shares into the aggregate result.
    fn unshard<It: IntoIterator<Item = AggregateShare<T::Field>>>(
        &self,
        _agg_param: &(),
        agg_shares: It,
    ) -> Result<A, VdafError> {
        let mut agg = AggregateShare(vec![T::Field::zero(); self.typ.output_len()]);
        for agg_share in agg_shares.into_iter() {
            agg.merge(&agg_share)?;
        }

        A::try_from(agg)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct JointRandParam<const L: usize> {
    /// The sum of the joint randomness seed shares sent to the other Aggregators.
    seed_hint: Seed<L>,

    /// The blinding factor, used to derive the aggregator's joint randomness seed share.
    blind: Seed<L>,
}

#[derive(Clone)]
struct HelperShare<const L: usize> {
    input_share: Seed<L>,
    proof_share: Seed<L>,
    joint_rand_param: JointRandParam<L>,
}

impl<const L: usize> HelperShare<L> {
    fn new() -> Result<Self, VdafError> {
        Ok(HelperShare {
            input_share: Seed::generate()?,
            proof_share: Seed::generate()?,
            joint_rand_param: JointRandParam {
                seed_hint: Seed::uninitialized(),
                blind: Seed::generate()?,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdaf::{run_vdaf, run_vdaf_prepare};
    use assert_matches::assert_matches;

    #[test]
    fn test_prio3_count() {
        let prio3 = Prio3Aes128Count::new(2).unwrap();

        assert_eq!(
            run_vdaf(&prio3, &(), [1, 0, 0, 1, 1]).unwrap(),
            Prio3Result(3)
        );

        let (_, verify_params) = prio3.setup().unwrap();
        let nonce = b"This is a good nonce.";

        let input_shares = prio3.shard(&(), &0).unwrap();
        run_vdaf_prepare(&prio3, &verify_params, &(), nonce, input_shares).unwrap();

        let input_shares = prio3.shard(&(), &1).unwrap();
        run_vdaf_prepare(&prio3, &verify_params, &(), nonce, input_shares).unwrap();
    }

    #[test]
    fn test_prio3_sum() {
        let prio3 = Prio3Aes128Sum::new(3, 16).unwrap();

        assert_eq!(
            run_vdaf(&prio3, &(), [0, (1 << 16) - 1, 0, 1, 1]).unwrap(),
            Prio3Result((1 << 16) + 1)
        );

        let (_, verify_params) = prio3.setup().unwrap();
        let nonce = b"This is a good nonce.";

        let mut input_shares = prio3.shard(&(), &1).unwrap();
        input_shares[0].joint_rand_param.as_mut().unwrap().blind.0[0] ^= 255;
        let result = run_vdaf_prepare(&prio3, &verify_params, &(), nonce, input_shares);
        assert_matches!(result, Err(VdafError::Uncategorized(_)));

        let mut input_shares = prio3.shard(&(), &1).unwrap();
        input_shares[0]
            .joint_rand_param
            .as_mut()
            .unwrap()
            .seed_hint
            .0[0] ^= 255;
        let result = run_vdaf_prepare(&prio3, &verify_params, &(), nonce, input_shares);
        assert_matches!(result, Err(VdafError::Uncategorized(_)));

        let mut input_shares = prio3.shard(&(), &1).unwrap();
        assert_matches!(input_shares[0].input_share, Share::Leader(ref mut data) => {
            data[0] += Field128::one();
        });
        let result = run_vdaf_prepare(&prio3, &verify_params, &(), nonce, input_shares);
        assert_matches!(result, Err(VdafError::Uncategorized(_)));

        let mut input_shares = prio3.shard(&(), &1).unwrap();
        assert_matches!(input_shares[0].proof_share, Share::Leader(ref mut data) => {
                data[0] += Field128::one();
        });
        let result = run_vdaf_prepare(&prio3, &verify_params, &(), nonce, input_shares);
        assert_matches!(result, Err(VdafError::Uncategorized(_)));
    }

    #[test]
    fn test_prio3_histogram() {
        let prio3 = Prio3Aes128Histogram::new(2, &[0, 10, 20]).unwrap();

        assert_eq!(
            run_vdaf(&prio3, &(), [0, 10, 20, 9999]).unwrap(),
            Prio3ResultVec(vec![1, 1, 1, 1])
        );

        assert_eq!(
            run_vdaf(&prio3, &(), [0]).unwrap(),
            Prio3ResultVec(vec![1, 0, 0, 0])
        );

        assert_eq!(
            run_vdaf(&prio3, &(), [5]).unwrap(),
            Prio3ResultVec(vec![0, 1, 0, 0])
        );

        assert_eq!(
            run_vdaf(&prio3, &(), [10]).unwrap(),
            Prio3ResultVec(vec![0, 1, 0, 0])
        );

        assert_eq!(
            run_vdaf(&prio3, &(), [15]).unwrap(),
            Prio3ResultVec(vec![0, 0, 1, 0])
        );

        assert_eq!(
            run_vdaf(&prio3, &(), [20]).unwrap(),
            Prio3ResultVec(vec![0, 0, 1, 0])
        );

        assert_eq!(
            run_vdaf(&prio3, &(), [25]).unwrap(),
            Prio3ResultVec(vec![0, 0, 0, 1])
        );
    }

    #[test]
    fn test_prio3_input_share() {
        let prio3 = Prio3Aes128Sum::new(5, 16).unwrap();
        let input_shares = prio3.shard(&(), &1).unwrap();

        // Check that seed shares are distinct.
        for (i, x) in input_shares.iter().enumerate() {
            for (j, y) in input_shares.iter().enumerate() {
                if i != j {
                    if let (Share::Helper(left), Share::Helper(right)) =
                        (&x.input_share, &y.input_share)
                    {
                        assert_ne!(left, right);
                    }

                    if let (Share::Helper(left), Share::Helper(right)) =
                        (&x.proof_share, &y.proof_share)
                    {
                        assert_ne!(left, right);
                    }

                    assert_ne!(x.joint_rand_param, y.joint_rand_param);
                }
            }
        }
    }
}

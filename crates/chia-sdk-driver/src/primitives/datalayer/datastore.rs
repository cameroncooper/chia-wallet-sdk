use chia_protocol::{Bytes, Bytes32, Coin, CoinSpend};
use chia_puzzles::{
    nft::{NftStateLayerArgs, NftStateLayerSolution, NFT_STATE_LAYER_PUZZLE_HASH},
    singleton::{
        LauncherSolution, SingletonArgs, SingletonSolution, SINGLETON_LAUNCHER_PUZZLE_HASH,
    },
    EveProof, LineageProof, Proof,
};
use chia_sdk_types::{run_puzzle, CreateCoin, NewMetadataInfo, NewMetadataOutput};
use chia_sdk_types::{Condition, NewMetadataCondition};
use clvm_traits::{FromClvm, FromClvmError, ToClvm};
use clvm_utils::{tree_hash, CurriedProgram, ToTreeHash, TreeHash};
use clvmr::{Allocator, NodePtr};
use num_bigint::BigInt;

use crate::{
    DelegationLayerArgs, DelegationLayerSolution, DriverError, Layer, NftStateLayer, Puzzle,
    SingletonLayer, Spend, SpendContext, DELEGATION_LAYER_PUZZLE_HASH,
    DL_METADATA_UPDATER_PUZZLE_HASH,
};

use super::{get_merkle_tree, DataStoreInfo, DataStoreMetadata, DelegatedPuzzle, HintType};

/// Everything that is required to spend a ``DataStore`` coin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataStore<M = DataStoreMetadata> {
    /// The coin that holds this ``DataStore``.
    pub coin: Coin,
    /// The lineage proof for the singletonlayer.
    pub proof: Proof,
    /// The info associated with the ``DataStore``, including the metadata.
    pub info: DataStoreInfo<M>,
}

impl<M> DataStore<M>
where
    M: ToClvm<Allocator> + FromClvm<Allocator>,
{
    pub fn new(coin: Coin, proof: Proof, info: DataStoreInfo<M>) -> Self {
        DataStore { coin, proof, info }
    }

    /// Creates a coin spend for this ``DataStore``.
    pub fn spend(self, ctx: &mut SpendContext, inner_spend: Spend) -> Result<CoinSpend, DriverError>
    where
        M: ToClvm<Allocator> + FromClvm<Allocator> + Clone,
    {
        let (puzzle_ptr, solution_ptr) = if self.info.delegated_puzzles.is_empty() {
            let layers = self
                .info
                .clone()
                .into_layers_without_delegation_layer(inner_spend.puzzle);

            let solution_ptr = layers.construct_solution(
                ctx,
                SingletonSolution {
                    lineage_proof: self.proof,
                    amount: self.coin.amount,
                    inner_solution: NftStateLayerSolution {
                        inner_solution: inner_spend.solution,
                    },
                },
            )?;

            (layers.construct_puzzle(ctx)?, solution_ptr)
        } else {
            let layers = self.info.clone().into_layers_with_delegation_layer(ctx)?;
            let puzzle_ptr = layers.construct_puzzle(ctx)?;

            let delegated_puzzle_hash = tree_hash(&ctx.allocator, inner_spend.puzzle);

            let tree = get_merkle_tree(ctx, self.info.delegated_puzzles)?;

            let inner_solution = DelegationLayerSolution {
                // if running owner puzzle, the line below will return 'None', thus ensuring correct puzzle behavior
                merkle_proof: tree.get_proof(delegated_puzzle_hash.into()),
                puzzle_reveal: inner_spend.puzzle,
                puzzle_solution: inner_spend.solution,
            };

            let solution_ptr = layers.construct_solution(
                ctx,
                SingletonSolution {
                    lineage_proof: self.proof,
                    amount: self.coin.amount,
                    inner_solution: NftStateLayerSolution { inner_solution },
                },
            )?;
            (puzzle_ptr, solution_ptr)
        };

        let puzzle = ctx.serialize(&puzzle_ptr)?;
        let solution = ctx.serialize(&solution_ptr)?;

        Ok(CoinSpend::new(self.coin, puzzle, solution))
    }

    /// Returns the lineage proof that would be used by the child.
    pub fn child_lineage_proof(&self, ctx: &mut SpendContext) -> Result<LineageProof, DriverError>
    where
        M: ToTreeHash,
    {
        Ok(LineageProof {
            parent_parent_coin_info: self.coin.parent_coin_info,
            parent_inner_puzzle_hash: self.info.inner_puzzle_hash(ctx)?.into(),
            parent_amount: self.coin.amount,
        })
    }
}

#[derive(ToClvm, FromClvm, Debug, Clone, PartialEq, Eq)]
#[clvm(list)]
pub struct DLLauncherKVList<M = DataStoreMetadata, T = NodePtr> {
    pub metadata: M,
    pub state_layer_inner_puzzle_hash: Bytes32,
    #[clvm(rest)]
    pub memos: Vec<T>,
}

#[derive(ToClvm, FromClvm, Debug, Clone, PartialEq, Eq)]
#[clvm(list)]
pub struct OldDLLauncherKVList<T = NodePtr> {
    pub root_hash: Bytes32,
    pub state_layer_inner_puzzle_hash: Bytes32,
    #[clvm(rest)]
    pub memos: Vec<T>,
}

pub trait MetadataWithRootHash {
    fn root_hash(&self) -> Bytes32;
    fn root_hash_only(root_hash: Bytes32) -> Self;
}

impl MetadataWithRootHash for DataStoreMetadata {
    fn root_hash(&self) -> Bytes32 {
        self.root_hash
    }

    fn root_hash_only(root_hash: Bytes32) -> Self {
        super::DataStoreMetadata::root_hash_only(root_hash)
    }
}

// does not Primitive because it needs extra info :(
impl<M> DataStore<M>
where
    M: ToClvm<Allocator> + FromClvm<Allocator> + ToTreeHash + MetadataWithRootHash,
{
    pub fn build_datastore(
        coin: Coin,
        launcher_id: Bytes32,
        proof: Proof,
        metadata: M,
        fallback_owner_ph: Bytes32,
        memos: Vec<Bytes>,
    ) -> Result<Self, DriverError> {
        let mut memos = memos;

        if memos.is_empty() {
            // no hints; owner puzzle hash is the inner puzzle hash
            return Ok(DataStore {
                coin,
                proof,
                info: DataStoreInfo {
                    launcher_id,
                    metadata,
                    owner_puzzle_hash: fallback_owner_ph,
                    delegated_puzzles: vec![],
                },
            });
        }

        if memos.drain(0..1).next().ok_or(DriverError::MissingMemo)? != launcher_id.into() {
            return Err(DriverError::InvalidMemo);
        }

        if memos.len() == 2 && memos[0] == metadata.root_hash().into() {
            // vanilla store using old memo format
            let owner_puzzle_hash = Bytes32::new(
                memos[1]
                    .to_vec()
                    .try_into()
                    .map_err(|_| DriverError::InvalidMemo)?,
            );
            return Ok(DataStore {
                coin,
                proof,
                info: DataStoreInfo {
                    launcher_id,
                    metadata,
                    owner_puzzle_hash,
                    delegated_puzzles: vec![],
                },
            });
        }

        let owner_puzzle_hash: Bytes32 = if memos.is_empty() {
            fallback_owner_ph
        } else {
            Bytes32::new(
                memos
                    .drain(0..1)
                    .next()
                    .ok_or(DriverError::MissingMemo)?
                    .to_vec()
                    .try_into()
                    .map_err(|_| DriverError::InvalidMemo)?,
            )
        };

        let mut delegated_puzzles = vec![];
        while memos.len() > 1 {
            delegated_puzzles.push(DelegatedPuzzle::from_memos(&mut memos)?);
        }

        Ok(DataStore {
            coin,
            proof,
            info: DataStoreInfo {
                launcher_id,
                metadata,
                owner_puzzle_hash,
                delegated_puzzles,
            },
        })
    }

    pub fn from_spend(
        allocator: &mut Allocator,
        cs: &CoinSpend,
        parent_delegated_puzzles: Vec<DelegatedPuzzle>,
    ) -> Result<Option<Self>, DriverError>
    where
        Self: Sized,
    {
        let solution_node_ptr = cs
            .solution
            .to_clvm(allocator)
            .map_err(DriverError::ToClvm)?;

        if cs.coin.puzzle_hash == SINGLETON_LAUNCHER_PUZZLE_HASH.into() {
            // we're just launching this singleton :)
            // solution is (singleton_full_puzzle_hash amount key_value_list)
            // kv_list is (metadata state_layer_hash)
            let launcher_id = cs.coin.coin_id();

            let proof = Proof::Eve(EveProof {
                parent_parent_coin_info: cs.coin.parent_coin_info,
                parent_amount: cs.coin.amount,
            });

            let solution = LauncherSolution::<DLLauncherKVList<M, Bytes>>::from_clvm(
                allocator,
                solution_node_ptr,
            );

            return match solution {
                Ok(solution) => {
                    let metadata = solution.key_value_list.metadata;

                    let new_coin = Coin {
                        parent_coin_info: launcher_id,
                        puzzle_hash: solution.singleton_puzzle_hash,
                        amount: solution.amount,
                    };

                    let mut memos: Vec<Bytes> = vec![launcher_id.into()];
                    memos.extend(solution.key_value_list.memos);

                    Ok(Some(Self::build_datastore(
                        new_coin,
                        launcher_id,
                        proof,
                        metadata,
                        solution.key_value_list.state_layer_inner_puzzle_hash,
                        memos,
                    )?))
                }
                Err(err) => match err {
                    FromClvmError::ExpectedPair => {
                        // datastore launched using old memo format
                        let solution = LauncherSolution::<OldDLLauncherKVList<Bytes>>::from_clvm(
                            allocator,
                            solution_node_ptr,
                        )?;

                        let coin = Coin {
                            parent_coin_info: launcher_id,
                            puzzle_hash: solution.singleton_puzzle_hash,
                            amount: solution.amount,
                        };

                        Ok(Some(Self::build_datastore(
                            coin,
                            launcher_id,
                            proof,
                            M::root_hash_only(solution.key_value_list.root_hash),
                            solution.key_value_list.state_layer_inner_puzzle_hash,
                            solution.key_value_list.memos,
                        )?))
                    }
                    _ => Err(DriverError::FromClvm(err)),
                },
            };
        }

        let parent_puzzle_ptr = cs
            .puzzle_reveal
            .to_clvm(allocator)
            .map_err(DriverError::ToClvm)?;
        let parent_puzzle = Puzzle::parse(allocator, parent_puzzle_ptr);

        let Some(singleton_layer) =
            SingletonLayer::<Puzzle>::parse_puzzle(allocator, parent_puzzle)?
        else {
            return Ok(None);
        };

        let Some(state_layer) =
            NftStateLayer::<M, Puzzle>::parse_puzzle(allocator, singleton_layer.inner_puzzle)?
        else {
            return Ok(None);
        };

        let parent_solution_ptr = cs.solution.to_clvm(allocator)?;
        let parent_solution = SingletonLayer::<NftStateLayer<M, Puzzle>>::parse_solution(
            allocator,
            parent_solution_ptr,
        )?;

        // At this point, inner puzzle might be either a delegation layer or just an ownership layer.
        let inner_puzzle = state_layer.inner_puzzle.ptr();
        let inner_solution = parent_solution.inner_solution.inner_solution;

        let inner_output = run_puzzle(allocator, inner_puzzle, inner_solution)?;
        let inner_conditions = Vec::<Condition>::from_clvm(allocator, inner_output)?;

        let mut inner_create_coin_condition = None;
        let mut inner_new_metadata_condition = None;

        for condition in inner_conditions {
            match condition {
                Condition::CreateCoin(condition) if condition.amount % 2 == 1 => {
                    inner_create_coin_condition = Some(condition);
                }
                Condition::Other(condition) => {
                    if let Ok(condition) =
                        NewMetadataCondition::<NodePtr, NodePtr>::from_clvm(allocator, condition)
                    {
                        inner_new_metadata_condition = Some(condition);
                    }
                }
                _ => {}
            }
        }

        let Some(inner_create_coin_condition) = inner_create_coin_condition else {
            return Err(DriverError::MissingChild);
        };

        let new_metadata = if let Some(inner_new_metadata_condition) = inner_new_metadata_condition
        {
            NftStateLayer::<M, NodePtr>::get_next_metadata(
                allocator,
                &state_layer.metadata,
                state_layer.metadata_updater_puzzle_hash,
                inner_new_metadata_condition,
            )?
        } else {
            state_layer.metadata
        };

        // first, just compute new coin info - will be used in any case

        let new_puzzle_hash = SingletonArgs::curry_tree_hash(
            singleton_layer.launcher_id,
            CurriedProgram {
                program: NFT_STATE_LAYER_PUZZLE_HASH,
                args: NftStateLayerArgs::<TreeHash, TreeHash> {
                    mod_hash: NFT_STATE_LAYER_PUZZLE_HASH.into(),
                    metadata: new_metadata.tree_hash(),
                    metadata_updater_puzzle_hash: state_layer.metadata_updater_puzzle_hash,
                    inner_puzzle: inner_create_coin_condition.puzzle_hash.into(),
                },
            }
            .tree_hash(),
        );

        let new_coin = Coin {
            parent_coin_info: cs.coin.coin_id(),
            puzzle_hash: new_puzzle_hash.into(),
            amount: inner_create_coin_condition.amount,
        };

        // if the coin was re-created with memos, there is a delegation layer
        // and delegated puzzles have been updated (we can rebuild the list from memos)
        if inner_create_coin_condition.memos.len() > 1 {
            // keep in mind that there's always the launcher id memo being added
            return Ok(Some(Self::build_datastore(
                new_coin,
                singleton_layer.launcher_id,
                Proof::Lineage(singleton_layer.lineage_proof(cs.coin)),
                new_metadata,
                state_layer.inner_puzzle.tree_hash().into(),
                inner_create_coin_condition.memos,
            )?));
        }

        let mut owner_puzzle_hash: Bytes32 = state_layer.inner_puzzle.tree_hash().into();

        // does the parent coin currently have a delegation layer?
        let delegation_layer_maybe = state_layer.inner_puzzle;
        if delegation_layer_maybe.is_curried()
            && delegation_layer_maybe.mod_hash() == DELEGATION_LAYER_PUZZLE_HASH
        {
            let deleg_puzzle_args = DelegationLayerArgs::from_clvm(
                allocator,
                delegation_layer_maybe
                    .as_curried()
                    .ok_or(DriverError::NonStandardLayer)?
                    .args,
            )
            .map_err(DriverError::FromClvm)?;
            owner_puzzle_hash = deleg_puzzle_args.owner_puzzle_hash;

            let delegation_layer_solution =
                DelegationLayerSolution::<NodePtr, NodePtr>::from_clvm(allocator, inner_solution)?;

            // to get more info, we'll need to run the delegated puzzle (delegation layer's "inner" puzzle)
            let output = run_puzzle(
                allocator,
                delegation_layer_solution.puzzle_reveal,
                delegation_layer_solution.puzzle_solution,
            )?;

            let odd_create_coin = Vec::<NodePtr>::from_clvm(allocator, output)?
                .iter()
                .map(|cond| Condition::<NodePtr>::from_clvm(allocator, *cond))
                .find(|cond| match cond {
                    Ok(Condition::CreateCoin(create_coin)) => create_coin.amount % 2 == 1,
                    _ => false,
                });

            let Some(odd_create_coin) = odd_create_coin else {
                // no CREATE_COIN was created by the innermost puzzle
                // delegation layer therefore added one (assuming the spend is valid)]
                return Ok(Some(DataStore {
                    coin: new_coin,
                    proof: Proof::Lineage(singleton_layer.lineage_proof(cs.coin)),
                    info: DataStoreInfo {
                        launcher_id: singleton_layer.launcher_id,
                        metadata: new_metadata,
                        owner_puzzle_hash,
                        delegated_puzzles: parent_delegated_puzzles,
                    },
                }));
            };

            let odd_create_coin = odd_create_coin?;

            // if there were any memos, the if above would have caught it since it processes
            // output conditions of the state layer inner puzzle (i.e., it runs the delegation layer)
            // therefore, this spend is either 'exiting' the delegation layer or re-creatign it
            if let Condition::CreateCoin(create_coin) = odd_create_coin {
                let prev_deleg_layer_ph = delegation_layer_maybe.tree_hash();

                if create_coin.puzzle_hash == prev_deleg_layer_ph.into() {
                    // owner is re-creating the delegation layer with the same options
                    return Ok(Some(DataStore {
                        coin: new_coin,
                        proof: Proof::Lineage(singleton_layer.lineage_proof(cs.coin)),
                        info: DataStoreInfo {
                            launcher_id: singleton_layer.launcher_id,
                            metadata: new_metadata,
                            owner_puzzle_hash, // owner puzzle was ran
                            delegated_puzzles: parent_delegated_puzzles,
                        },
                    }));
                }

                // owner is exiting the delegation layer
                owner_puzzle_hash = create_coin.puzzle_hash;
            }
        }

        // all methods exhausted; this coin doesn't seem to have a delegation layer
        Ok(Some(DataStore {
            coin: new_coin,
            proof: Proof::Lineage(singleton_layer.lineage_proof(cs.coin)),
            info: DataStoreInfo {
                launcher_id: singleton_layer.launcher_id,
                metadata: new_metadata,
                owner_puzzle_hash,
                delegated_puzzles: vec![],
            },
        }))
    }
}

impl<M> DataStore<M> {
    pub fn get_recreation_memos(
        launcher_id: Bytes32,
        owner_puzzle_hash: TreeHash,
        delegated_puzzles: Vec<DelegatedPuzzle>,
    ) -> Vec<Bytes> {
        let owner_puzzle_hash: Bytes32 = owner_puzzle_hash.into();
        let mut memos: Vec<Bytes> = vec![launcher_id.into(), owner_puzzle_hash.into()];

        for delegated_puzzle in delegated_puzzles {
            match delegated_puzzle {
                DelegatedPuzzle::Admin(inner_puzzle_hash) => {
                    memos.push(Bytes::new([HintType::AdminPuzzle.value()].into()));
                    memos.push(inner_puzzle_hash.into());
                }
                DelegatedPuzzle::Writer(inner_puzzle_hash) => {
                    memos.push(Bytes::new([HintType::WriterPuzzle.value()].into()));
                    memos.push(inner_puzzle_hash.into());
                }
                DelegatedPuzzle::Oracle(oracle_puzzle_hash, oracle_fee) => {
                    memos.push(Bytes::new([HintType::OraclePuzzle.value()].into()));
                    memos.push(oracle_puzzle_hash.into());

                    let fee_bytes = BigInt::from(oracle_fee).to_signed_bytes_be();
                    let mut fee_bytes = fee_bytes.as_slice();

                    // https://github.com/Chia-Network/clvm_rs/blob/66a17f9576d26011321bb4c8c16eb1c63b169f1f/src/allocator.rs#L295
                    while (!fee_bytes.is_empty()) && (fee_bytes[0] == 0) {
                        if fee_bytes.len() > 1 && (fee_bytes[1] & 0x80 == 0x80) {
                            break;
                        }
                        fee_bytes = &fee_bytes[1..];
                    }

                    memos.push(fee_bytes.into());
                }
            }
        }

        memos
    }

    // As an owner use CREATE_COIN to:
    //  - just re-create store (no hints needed)
    //  - change delegated puzzles (hints needed)
    pub fn owner_create_coin_condition(
        ctx: &mut SpendContext,
        launcher_id: Bytes32,
        new_inner_puzzle_hash: Bytes32,
        new_delegated_puzzles: Vec<DelegatedPuzzle>,
        hint_delegated_puzzles: bool,
    ) -> Result<Condition, DriverError> {
        let new_puzzle_hash = if new_delegated_puzzles.is_empty() {
            new_inner_puzzle_hash
        } else {
            let new_merkle_root = get_merkle_tree(ctx, new_delegated_puzzles.clone())?.root;
            DelegationLayerArgs::curry_tree_hash(
                launcher_id,
                new_inner_puzzle_hash,
                new_merkle_root,
            )
            .into()
        };

        Ok(Condition::CreateCoin(CreateCoin {
            amount: 1,
            puzzle_hash: new_puzzle_hash,
            memos: if hint_delegated_puzzles {
                Self::get_recreation_memos(
                    launcher_id,
                    new_inner_puzzle_hash.into(),
                    new_delegated_puzzles,
                )
            } else {
                vec![launcher_id.into()]
            },
        }))
    }

    pub fn new_metadata_condition(
        ctx: &mut SpendContext,
        new_metadata: M,
    ) -> Result<Condition, DriverError>
    where
        M: ToClvm<Allocator>,
    {
        let new_metadata_condition = NewMetadataCondition::<i32, NewMetadataOutput<M, ()>> {
            metadata_updater_reveal: 11,
            // metadata updater will just return solution, so we can set the solution to NewMetadataOutput :)
            metadata_updater_solution: NewMetadataOutput {
                metadata_part: NewMetadataInfo::<M> {
                    new_metadata,
                    new_metadata_updater_puzhash: DL_METADATA_UPDATER_PUZZLE_HASH.into(),
                },
                conditions: (),
            },
        }
        .to_clvm(&mut ctx.allocator)?;

        Ok(Condition::Other(new_metadata_condition))
    }
}

#[allow(unused_imports)]
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
pub mod tests {
    use chia_bls::SecretKey;
    use chia_protocol::Program;
    use chia_puzzles::standard::{StandardArgs, StandardSolution};
    use chia_sdk_test::{test_secret_keys, test_transaction, Simulator};
    use chia_sdk_types::Conditions;
    use clvm_traits::clvm_quote;
    use clvmr::{
        serde::{node_from_bytes, node_to_bytes},
        sha2::Sha256,
    };
    use rstest::rstest;

    use crate::{Launcher, NewMerkleRootCondition, OracleLayer, StandardLayer, WriterLayer};

    use super::*;

    #[derive(Debug, PartialEq, Copy, Clone)]
    pub enum Label {
        None,
        Some,
        New,
    }

    impl Label {
        pub fn value(&self) -> Option<String> {
            match self {
                Label::None => None,
                Label::Some => Some(String::from("label")),
                Label::New => Some(String::from("new_label")),
            }
        }
    }

    #[derive(Debug, PartialEq, Copy, Clone)]
    pub enum Description {
        None,
        Some,
        New,
    }

    impl Description {
        pub fn value(&self) -> Option<String> {
            match self {
                Description::None => None,
                Description::Some => Some(String::from("description")),
                Description::New => Some(String::from("new_description")),
            }
        }
    }

    #[derive(Debug, PartialEq, Copy, Clone)]
    pub enum RootHash {
        Zero,
        Some,
    }

    impl RootHash {
        pub fn value(&self) -> Bytes32 {
            match self {
                RootHash::Zero => Bytes32::from([0; 32]),
                RootHash::Some => Bytes32::from([1; 32]),
            }
        }
    }

    #[derive(Debug, PartialEq, Copy, Clone)]
    pub enum ByteSize {
        None,
        Some,
        New,
    }

    impl ByteSize {
        pub fn value(&self) -> Option<u64> {
            match self {
                ByteSize::None => None,
                ByteSize::Some => Some(1337),
                ByteSize::New => Some(42),
            }
        }
    }

    #[tokio::test]
    async fn test_simple_datastore() -> anyhow::Result<()> {
        let sim = Simulator::new().await?;
        let peer = sim.connect().await?;

        let [sk]: [SecretKey; 1] = test_secret_keys(1).unwrap().try_into().unwrap();
        let pk = sk.public_key();

        let puzzle_hash = StandardArgs::curry_tree_hash(pk).into();
        let coin = sim.mint_coin(puzzle_hash, 1).await;

        let ctx = &mut SpendContext::new();

        let (launch_singleton, datastore) = Launcher::new(coin.coin_id(), 1).mint_datastore(
            ctx,
            DataStoreMetadata::root_hash_only(RootHash::Zero.value()),
            puzzle_hash.into(),
            vec![],
        )?;

        ctx.spend_p2_coin(coin, pk, launch_singleton)?;

        let spends = ctx.take();
        for spend in spends {
            if spend.coin.coin_id() == datastore.info.launcher_id {
                let new_datastore =
                    DataStore::from_spend(&mut ctx.allocator, &spend, vec![])?.unwrap();

                assert_eq!(datastore, new_datastore);
            }

            ctx.insert(spend);
        }

        let datastore_inner_spend = StandardLayer::new(pk)
            .spend(ctx, Conditions::new().create_coin(puzzle_hash, 1, vec![]))?;

        let old_datastore_coin = datastore.coin;
        let new_spend = datastore.spend(ctx, datastore_inner_spend)?;

        ctx.insert(new_spend);

        test_transaction(&peer, ctx.take(), &[sk], &sim.config().constants).await;

        // Make sure the datastore was created.
        let coin_state = sim
            .coin_state(old_datastore_coin.coin_id())
            .await
            .expect("expected datastore coin");
        assert_eq!(coin_state.coin, old_datastore_coin);
        assert!(coin_state.spent_height.is_some());

        Ok(())
    }

    #[allow(clippy::similar_names)]
    #[tokio::test]
    async fn test_datastore_with_delegation_layer() -> anyhow::Result<()> {
        let sim = Simulator::new().await?;
        let peer = sim.connect().await?;

        let [owner_sk, admin_sk, writer_sk]: [SecretKey; 3] =
            test_secret_keys(3).unwrap().try_into().unwrap();

        let owner_pk = owner_sk.public_key();
        let admin_pk = admin_sk.public_key();
        let writer_pk = writer_sk.public_key();

        let oracle_puzzle_hash: Bytes32 = [1; 32].into();
        let oracle_fee = 1000;

        let owner_puzzle_hash = StandardArgs::curry_tree_hash(owner_pk).into();
        let coin = sim.mint_coin(owner_puzzle_hash, 1).await;

        let ctx = &mut SpendContext::new();

        let admin_puzzle: NodePtr = CurriedProgram {
            program: ctx.standard_puzzle()?,
            args: StandardArgs::new(admin_pk),
        }
        .to_clvm(&mut ctx.allocator)?;
        let admin_puzzle_hash = tree_hash(&ctx.allocator, admin_puzzle);

        let writer_inner_puzzle: NodePtr = CurriedProgram {
            program: ctx.standard_puzzle()?,
            args: StandardArgs::new(writer_pk),
        }
        .to_clvm(&mut ctx.allocator)?;
        let writer_inner_puzzle_hash = tree_hash(&ctx.allocator, writer_inner_puzzle);

        let admin_delegated_puzzle = DelegatedPuzzle::Admin(admin_puzzle_hash.into());
        let writer_delegated_puzzle = DelegatedPuzzle::Writer(writer_inner_puzzle_hash.into());

        let oracle_delegated_puzzle = DelegatedPuzzle::Oracle(oracle_puzzle_hash, oracle_fee);

        let (launch_singleton, datastore) = Launcher::new(coin.coin_id(), 1).mint_datastore(
            ctx,
            DataStoreMetadata::default(),
            owner_puzzle_hash.into(),
            vec![
                admin_delegated_puzzle,
                writer_delegated_puzzle,
                oracle_delegated_puzzle,
            ],
        )?;

        ctx.spend_p2_coin(coin, owner_pk, launch_singleton)?;

        let spends = ctx.take();
        for spend in spends {
            if spend.coin.coin_id() == datastore.info.launcher_id {
                let new_datastore =
                    DataStore::from_spend(&mut ctx.allocator, &spend, vec![])?.unwrap();

                assert_eq!(datastore, new_datastore);
            }

            ctx.insert(spend);
        }

        assert_eq!(datastore.info.metadata.root_hash, RootHash::Zero.value());

        // writer: update metadata
        let new_metadata = DataStoreMetadata {
            root_hash: RootHash::Some.value(),
            label: Label::Some.value(),
            description: Description::Some.value(),
            bytes: ByteSize::Some.value(),
        };

        let new_metadata_condition = DataStore::new_metadata_condition(ctx, new_metadata.clone())?;

        let writer_layer = WriterLayer::new(StandardLayer::new(writer_pk));

        let dp = ctx.alloc(&clvm_quote!(Conditions::new().with(new_metadata_condition)))?;
        let writer_layer_solution = writer_layer.construct_solution(
            ctx,
            StandardSolution {
                original_public_key: None,
                delegated_puzzle: dp,
                solution: NodePtr::NIL,
            },
        )?;
        let writer_layer_puzzle = writer_layer.construct_puzzle(ctx)?;
        let inner_spend = Spend {
            puzzle: writer_layer_puzzle,
            solution: writer_layer_solution,
        };
        let new_spend = datastore.clone().spend(ctx, inner_spend)?;

        let datastore = DataStore::<DataStoreMetadata>::from_spend(
            &mut ctx.allocator,
            &new_spend,
            datastore.info.delegated_puzzles.clone(),
        )?
        .unwrap();
        ctx.insert(new_spend);

        assert_eq!(datastore.info.metadata, new_metadata);

        // admin: remove writer from delegated puzzles
        let delegated_puzzles = vec![admin_delegated_puzzle, oracle_delegated_puzzle];
        let new_merkle_tree = get_merkle_tree(ctx, delegated_puzzles.clone())?;
        let new_merkle_root = new_merkle_tree.root;

        let new_merkle_root_condition = NewMerkleRootCondition {
            new_merkle_root,
            memos: DataStore::<DataStoreMetadata>::get_recreation_memos(
                datastore.info.launcher_id,
                owner_puzzle_hash.into(),
                delegated_puzzles.clone(),
            ),
        }
        .to_clvm(&mut ctx.allocator)?;

        let inner_spend = StandardLayer::new(admin_pk).spend(
            ctx,
            Conditions::new().with(Condition::Other(new_merkle_root_condition)),
        )?;
        let new_spend = datastore.clone().spend(ctx, inner_spend)?;

        let datastore = DataStore::<DataStoreMetadata>::from_spend(
            &mut ctx.allocator,
            &new_spend,
            datastore.info.delegated_puzzles.clone(),
        )?
        .unwrap();
        ctx.insert(new_spend);

        assert!(!datastore.info.delegated_puzzles.is_empty());
        assert_eq!(datastore.info.delegated_puzzles, delegated_puzzles);

        // oracle: just spend :)

        let oracle_layer = OracleLayer::new(oracle_puzzle_hash, oracle_fee);
        let inner_datastore_spend = oracle_layer.construct_spend(ctx, ())?;

        let new_spend = datastore.clone().spend(ctx, inner_datastore_spend)?;

        let new_datastore = DataStore::<DataStoreMetadata>::from_spend(
            &mut ctx.allocator,
            &new_spend,
            datastore.info.delegated_puzzles.clone(),
        )?
        .unwrap();
        ctx.insert(new_spend);

        assert_eq!(new_datastore.info, new_datastore.info);
        let datastore = new_datastore;

        // mint a coin that asserts the announcement and has enough value
        let new_coin = sim.mint_coin(owner_puzzle_hash, oracle_fee).await;

        let mut hasher = Sha256::new();
        hasher.update(datastore.coin.puzzle_hash);
        hasher.update(Bytes::new("$".into()).to_vec());

        ctx.spend_p2_coin(
            new_coin,
            owner_pk,
            Conditions::new().assert_puzzle_announcement(Bytes32::new(hasher.finalize())),
        )?;

        // finally, remove delegation layer altogether
        let owner_layer = StandardLayer::new(owner_pk);
        let output_condition = DataStore::<DataStoreMetadata>::owner_create_coin_condition(
            ctx,
            datastore.info.launcher_id,
            owner_puzzle_hash,
            vec![],
            true,
        )?;
        let datastore_remove_delegation_layer_inner_spend =
            owner_layer.spend(ctx, Conditions::new().with(output_condition))?;
        let new_spend = datastore
            .clone()
            .spend(ctx, datastore_remove_delegation_layer_inner_spend)?;

        let new_datastore =
            DataStore::<DataStoreMetadata>::from_spend(&mut ctx.allocator, &new_spend, vec![])?
                .unwrap();
        ctx.insert(new_spend);

        assert!(new_datastore.info.delegated_puzzles.is_empty());
        assert_eq!(new_datastore.info.owner_puzzle_hash, owner_puzzle_hash);

        test_transaction(
            &peer,
            ctx.take(),
            &[owner_sk, admin_sk, writer_sk],
            &sim.config().constants,
        )
        .await;

        // Make sure the datastore was created.
        let coin_state = sim
            .coin_state(new_datastore.coin.parent_coin_info)
            .await
            .expect("expected datastore coin");
        assert_eq!(coin_state.coin, datastore.coin);
        assert!(coin_state.spent_height.is_some());

        Ok(())
    }

    #[derive(PartialEq, Debug, Clone, Copy)]
    pub enum DstAdminLayer {
        None,
        Same,
        New,
    }

    fn assert_metadata_like_tests(
        meta: &DataStoreMetadata,
        test_meta: (RootHash, Label, Description, ByteSize),
    ) {
        assert_eq!(meta.root_hash, test_meta.0.value());
        assert_eq!(meta.label, test_meta.1.value());
        assert_eq!(meta.description, test_meta.2.value());
        assert_eq!(meta.bytes, test_meta.3.value());
    }

    fn assert_delegated_puzzles_contain(
        dps: &[DelegatedPuzzle],
        values: &[DelegatedPuzzle],
        contained: &[bool],
    ) {
        for (i, value) in values.iter().enumerate() {
            assert_eq!(dps.iter().any(|dp| dp == value), contained[i]);
        }
    }

    #[rstest(
    src_with_writer => [true, false],
    src_with_oracle => [true, false],
    dst_with_writer => [true, false],
    dst_with_oracle => [true, false],
    src_meta => [
      (RootHash::Zero, Label::None, Description::None, ByteSize::None),
      (RootHash::Some, Label::Some, Description::Some, ByteSize::Some),
    ],
    dst_meta => [
      (RootHash::Zero, Label::None, Description::None, ByteSize::None),
      (RootHash::Zero, Label::Some, Description::Some, ByteSize::Some),
      (RootHash::Zero, Label::New, Description::New, ByteSize::New),
    ],
    dst_admin => [
      DstAdminLayer::None,
      DstAdminLayer::Same,
      DstAdminLayer::New,
    ]
  )]
    #[tokio::test]
    async fn test_datastore_admin_transition(
        src_meta: (RootHash, Label, Description, ByteSize),
        src_with_writer: bool,
        // src must have admin layer in this scenario
        src_with_oracle: bool,
        dst_with_writer: bool,
        dst_with_oracle: bool,
        dst_admin: DstAdminLayer,
        dst_meta: (RootHash, Label, Description, ByteSize),
    ) -> anyhow::Result<()> {
        let sim = Simulator::new().await?;
        let peer = sim.connect().await?;

        let [owner_sk, admin_sk, admin2_sk, writer_sk]: [SecretKey; 4] =
            test_secret_keys(4).unwrap().try_into().unwrap();

        let owner_pk = owner_sk.public_key();
        let admin_pk = admin_sk.public_key();
        let admin2_pk = admin2_sk.public_key();
        let writer_pk = writer_sk.public_key();

        let oracle_puzzle_hash: Bytes32 = [7; 32].into();
        let oracle_fee = 1000;

        let owner_puzzle_hash = StandardArgs::curry_tree_hash(owner_pk).into();
        let coin = sim.mint_coin(owner_puzzle_hash, 1).await;

        let ctx = &mut SpendContext::new();

        let admin_delegated_puzzle =
            DelegatedPuzzle::Admin(StandardArgs::curry_tree_hash(admin_pk).into());
        let admin2_delegated_puzzle =
            DelegatedPuzzle::Admin(StandardArgs::curry_tree_hash(admin2_pk).into());
        let writer_delegated_puzzle =
            DelegatedPuzzle::Writer(StandardArgs::curry_tree_hash(writer_pk).into());
        let oracle_delegated_puzzle = DelegatedPuzzle::Oracle(oracle_puzzle_hash, oracle_fee);

        let mut src_delegated_puzzles: Vec<DelegatedPuzzle> = vec![];
        src_delegated_puzzles.push(admin_delegated_puzzle);
        if src_with_writer {
            src_delegated_puzzles.push(writer_delegated_puzzle);
        }
        if src_with_oracle {
            src_delegated_puzzles.push(oracle_delegated_puzzle);
        }

        let (launch_singleton, src_datastore) = Launcher::new(coin.coin_id(), 1).mint_datastore(
            ctx,
            DataStoreMetadata {
                root_hash: src_meta.0.value(),
                label: src_meta.1.value(),
                description: src_meta.2.value(),
                bytes: src_meta.3.value(),
            },
            owner_puzzle_hash.into(),
            src_delegated_puzzles.clone(),
        )?;

        ctx.spend_p2_coin(coin, owner_pk, launch_singleton)?;

        // transition from src to dst
        let mut admin_inner_output = Conditions::new();

        let mut dst_delegated_puzzles: Vec<DelegatedPuzzle> = src_delegated_puzzles.clone();
        if src_with_writer != dst_with_writer
            || src_with_oracle != dst_with_oracle
            || dst_admin != DstAdminLayer::Same
        {
            dst_delegated_puzzles.clear();

            if dst_with_writer {
                dst_delegated_puzzles.push(writer_delegated_puzzle);
            }
            if dst_with_oracle {
                dst_delegated_puzzles.push(oracle_delegated_puzzle);
            }

            match dst_admin {
                DstAdminLayer::None => {}
                DstAdminLayer::Same => {
                    dst_delegated_puzzles.push(admin_delegated_puzzle);
                }
                DstAdminLayer::New => {
                    dst_delegated_puzzles.push(admin2_delegated_puzzle);
                }
            }

            let new_merkle_tree = get_merkle_tree(ctx, dst_delegated_puzzles.clone())?;

            let new_merkle_root_condition = NewMerkleRootCondition {
                new_merkle_root: new_merkle_tree.root,
                memos: DataStore::<DataStoreMetadata>::get_recreation_memos(
                    src_datastore.info.launcher_id,
                    owner_puzzle_hash.into(),
                    dst_delegated_puzzles.clone(),
                ),
            }
            .to_clvm(&mut ctx.allocator)
            .unwrap();

            admin_inner_output =
                admin_inner_output.with(Condition::Other(new_merkle_root_condition));
        }

        if src_meta.0 != dst_meta.0 || src_meta.1 != dst_meta.1 || src_meta.2 != dst_meta.2 {
            let new_metadata = DataStoreMetadata {
                root_hash: dst_meta.0.value(),
                label: dst_meta.1.value(),
                description: dst_meta.2.value(),
                bytes: dst_meta.3.value(),
            };

            admin_inner_output =
                admin_inner_output.with(DataStore::new_metadata_condition(ctx, new_metadata)?);
        }

        // delegated puzzle info + inner puzzle reveal + solution
        let inner_datastore_spend = StandardLayer::new(admin_pk).spend(ctx, admin_inner_output)?;
        let src_datastore_coin = src_datastore.coin;
        let new_spend = src_datastore.clone().spend(ctx, inner_datastore_spend)?;

        let dst_datastore = DataStore::<DataStoreMetadata>::from_spend(
            &mut ctx.allocator,
            &new_spend,
            src_datastore.info.delegated_puzzles.clone(),
        )?
        .unwrap();
        ctx.insert(new_spend);

        assert_eq!(src_datastore.info.delegated_puzzles, src_delegated_puzzles);
        assert_eq!(src_datastore.info.owner_puzzle_hash, owner_puzzle_hash);

        assert_metadata_like_tests(&src_datastore.info.metadata, src_meta);
        assert_delegated_puzzles_contain(
            &src_datastore.info.delegated_puzzles,
            &[
                admin2_delegated_puzzle,
                admin_delegated_puzzle,
                writer_delegated_puzzle,
                oracle_delegated_puzzle,
            ],
            &[false, true, src_with_writer, src_with_oracle],
        );

        assert_eq!(dst_datastore.info.delegated_puzzles, dst_delegated_puzzles);
        assert_eq!(dst_datastore.info.owner_puzzle_hash, owner_puzzle_hash);

        assert_metadata_like_tests(&dst_datastore.info.metadata, dst_meta);

        assert_delegated_puzzles_contain(
            &dst_datastore.info.delegated_puzzles,
            &[
                admin2_delegated_puzzle,
                admin_delegated_puzzle,
                writer_delegated_puzzle,
                oracle_delegated_puzzle,
            ],
            &[
                dst_admin == DstAdminLayer::New,
                dst_admin == DstAdminLayer::Same,
                dst_with_writer,
                dst_with_oracle,
            ],
        );

        test_transaction(
            &peer,
            ctx.take(),
            &[owner_sk, admin_sk, writer_sk],
            &sim.config().constants,
        )
        .await;

        let src_coin_state = sim
            .coin_state(src_datastore_coin.coin_id())
            .await
            .expect("expected src datastore coin");
        assert_eq!(src_coin_state.coin, src_datastore_coin);
        assert!(src_coin_state.spent_height.is_some());
        let dst_coin_state = sim
            .coin_state(dst_datastore.coin.coin_id())
            .await
            .expect("expected dst datastore coin");
        assert_eq!(dst_coin_state.coin, dst_datastore.coin);
        assert!(dst_coin_state.created_height.is_some());

        Ok(())
    }

    #[rstest(
        src_with_admin => [true, false],
        src_with_writer => [true, false],
        src_with_oracle => [true, false],
        dst_with_admin => [true, false],
        dst_with_writer => [true, false],
        dst_with_oracle => [true, false],
        src_meta => [
          (RootHash::Zero, Label::None, Description::None, ByteSize::None),
          (RootHash::Some, Label::Some, Description::Some, ByteSize::Some),
        ],
        dst_meta => [
          (RootHash::Zero, Label::None, Description::None, ByteSize::None),
          (RootHash::Some, Label::Some, Description::Some, ByteSize::Some),
          (RootHash::Some, Label::New, Description::New, ByteSize::New),
        ],
        change_owner => [true, false],
      )]
    #[tokio::test]
    async fn test_datastore_owner_transition(
        src_meta: (RootHash, Label, Description, ByteSize),
        src_with_admin: bool,
        src_with_writer: bool,
        src_with_oracle: bool,
        dst_with_admin: bool,
        dst_with_writer: bool,
        dst_with_oracle: bool,
        dst_meta: (RootHash, Label, Description, ByteSize),
        change_owner: bool,
    ) -> anyhow::Result<()> {
        let sim = Simulator::new().await?;
        let peer = sim.connect().await?;

        let [owner_sk, owner2_sk, admin_sk, writer_sk]: [SecretKey; 4] =
            test_secret_keys(4).unwrap().try_into().unwrap();

        let owner_pk = owner_sk.public_key();
        let owner2_pk = owner2_sk.public_key();
        let admin_pk = admin_sk.public_key();
        let writer_pk = writer_sk.public_key();

        let oracle_puzzle_hash: Bytes32 = [7; 32].into();
        let oracle_fee = 1000;

        let owner_puzzle_hash = StandardArgs::curry_tree_hash(owner_pk).into();
        let coin = sim.mint_coin(owner_puzzle_hash, 1).await;

        let owner2_puzzle_hash = StandardArgs::curry_tree_hash(owner2_pk).into();
        assert_ne!(owner_puzzle_hash, owner2_puzzle_hash);

        let ctx = &mut SpendContext::new();

        let admin_delegated_puzzle =
            DelegatedPuzzle::Admin(StandardArgs::curry_tree_hash(admin_pk).into());
        let writer_delegated_puzzle =
            DelegatedPuzzle::Writer(StandardArgs::curry_tree_hash(writer_pk).into());
        let oracle_delegated_puzzle = DelegatedPuzzle::Oracle(oracle_puzzle_hash, oracle_fee);

        let mut src_delegated_puzzles: Vec<DelegatedPuzzle> = vec![];
        if src_with_admin {
            src_delegated_puzzles.push(admin_delegated_puzzle);
        }
        if src_with_writer {
            src_delegated_puzzles.push(writer_delegated_puzzle);
        }
        if src_with_oracle {
            src_delegated_puzzles.push(oracle_delegated_puzzle);
        }

        let (launch_singleton, src_datastore) = Launcher::new(coin.coin_id(), 1).mint_datastore(
            ctx,
            DataStoreMetadata {
                root_hash: src_meta.0.value(),
                label: src_meta.1.value(),
                description: src_meta.2.value(),
                bytes: src_meta.3.value(),
            },
            owner_puzzle_hash.into(),
            src_delegated_puzzles.clone(),
        )?;
        ctx.spend_p2_coin(coin, owner_pk, launch_singleton)?;

        // transition from src to dst using owner puzzle
        let mut owner_output_conds = Conditions::new();

        let mut dst_delegated_puzzles: Vec<DelegatedPuzzle> = src_delegated_puzzles.clone();
        let mut hint_new_delegated_puzzles = change_owner;
        if src_with_admin != dst_with_admin
            || src_with_writer != dst_with_writer
            || src_with_oracle != dst_with_oracle
            || dst_delegated_puzzles.is_empty()
        {
            dst_delegated_puzzles.clear();
            hint_new_delegated_puzzles = true;

            if dst_with_admin {
                dst_delegated_puzzles.push(admin_delegated_puzzle);
            }
            if dst_with_writer {
                dst_delegated_puzzles.push(writer_delegated_puzzle);
            }
            if dst_with_oracle {
                dst_delegated_puzzles.push(oracle_delegated_puzzle);
            }
        }

        owner_output_conds =
            owner_output_conds.with(DataStore::<DataStoreMetadata>::owner_create_coin_condition(
                ctx,
                src_datastore.info.launcher_id,
                if change_owner {
                    owner2_puzzle_hash
                } else {
                    owner_puzzle_hash
                },
                dst_delegated_puzzles.clone(),
                hint_new_delegated_puzzles,
            )?);

        if src_meta != dst_meta {
            let new_metadata = DataStoreMetadata {
                root_hash: dst_meta.0.value(),
                label: dst_meta.1.value(),
                description: dst_meta.2.value(),
                bytes: dst_meta.3.value(),
            };

            owner_output_conds =
                owner_output_conds.with(DataStore::new_metadata_condition(ctx, new_metadata)?);
        }

        // delegated puzzle info + inner puzzle reveal + solution
        let inner_datastore_spend = StandardLayer::new(owner_pk).spend(ctx, owner_output_conds)?;
        let new_spend = src_datastore.clone().spend(ctx, inner_datastore_spend)?;

        let dst_datastore = DataStore::from_spend(
            &mut ctx.allocator,
            &new_spend,
            src_datastore.info.delegated_puzzles.clone(),
        )?
        .unwrap();

        ctx.insert(new_spend);

        assert_eq!(src_datastore.info.delegated_puzzles, src_delegated_puzzles);
        assert_eq!(src_datastore.info.owner_puzzle_hash, owner_puzzle_hash);

        assert_metadata_like_tests(&src_datastore.info.metadata, src_meta);

        assert_delegated_puzzles_contain(
            &src_datastore.info.delegated_puzzles,
            &[
                admin_delegated_puzzle,
                writer_delegated_puzzle,
                oracle_delegated_puzzle,
            ],
            &[src_with_admin, src_with_writer, src_with_oracle],
        );

        assert_eq!(dst_datastore.info.delegated_puzzles, dst_delegated_puzzles);
        assert_eq!(
            dst_datastore.info.owner_puzzle_hash,
            if change_owner {
                owner2_puzzle_hash
            } else {
                owner_puzzle_hash
            }
        );

        assert_metadata_like_tests(&dst_datastore.info.metadata, dst_meta);

        assert_delegated_puzzles_contain(
            &dst_datastore.info.delegated_puzzles,
            &[
                admin_delegated_puzzle,
                writer_delegated_puzzle,
                oracle_delegated_puzzle,
            ],
            &[dst_with_admin, dst_with_writer, dst_with_oracle],
        );

        test_transaction(
            &peer,
            ctx.take(),
            &[owner_sk, admin_sk, writer_sk],
            &sim.config().constants,
        )
        .await;

        let src_coin_state = sim
            .coin_state(src_datastore.coin.coin_id())
            .await
            .expect("expected src datastore coin");
        assert_eq!(src_coin_state.coin, src_datastore.coin);
        assert!(src_coin_state.spent_height.is_some());

        let dst_coin_state = sim
            .coin_state(dst_datastore.coin.coin_id())
            .await
            .expect("expected dst datastore coin");
        assert_eq!(dst_coin_state.coin, dst_datastore.coin);
        assert!(dst_coin_state.created_height.is_some());

        Ok(())
    }
}

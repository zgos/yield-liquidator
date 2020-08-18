use crate::{
    borrowers::{Borrower, Borrowers},
    liquidations::{Auction, Liquidator},
};

use anyhow::Result;
use ethers::prelude::*;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::Write, path::PathBuf, sync::Arc};
use tracing::debug_span;

#[derive(Serialize, Deserialize, Default)]
/// The state which is stored in our logs
pub struct State {
    /// The vaults being monitored
    vaults: HashMap<Address, Auction>,
    /// The borrowers being monitored
    borrowers: HashMap<Address, Borrower>,
    /// The last observed block
    last_block: U64,
}

/// The keeper monitors the chain for both liquidation opportunities and for
/// participation in auctions using Uniswap as a liquidity source
pub struct Keeper<'a> {
    client: Arc<Client<Http, Wallet>>,
    last_block: U64,

    borrowers: Borrowers<'a>,
    liquidator: Liquidator<'a>,
}

impl<'a> Keeper<'a> {
    /// Instantiates the keeper. `state` should be passed if there is previous
    /// data which should be taken into account from a previous run
    pub async fn new(
        client: Arc<Client<Http, Wallet>>,
        multicall: &'a Multicall<Http, Wallet>,
        controller: Address,
        liquidations: Address,
        uniswap: Address,
        flashloan: Address,
        state: Option<State>,
    ) -> Result<Keeper<'a>> {
        let (borrowers, vaults, last_block) = match state {
            Some(state) => (state.borrowers, state.vaults, state.last_block),
            None => (HashMap::new(), HashMap::new(), 0.into()),
        };

        let borrowers = Borrowers::new(controller, client.clone(), multicall, borrowers);
        let liquidator = Liquidator::new(
            liquidations,
            uniswap,
            flashloan,
            client.clone(),
            multicall,
            vaults,
        );

        Ok(Self {
            client,
            borrowers,
            liquidator,
            last_block,
        })
    }

    pub async fn run(&mut self, fname: PathBuf) -> Result<()> {
        let watcher = self.client.clone();
        let mut on_block = watcher.watch_blocks().await?.stream();

        while let Some(_) = on_block.next().await {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&fname)
                .unwrap();

            let block_number = self.client.get_block_number().await?;
            let span = debug_span!("eloop", block = %block_number);
            let _enter = span.enter();
            self.on_block(block_number).await?;

            // update our last block
            self.last_block = block_number;

            // Log once every 20 blocks (~300s)
            if block_number % 10 == 0.into() {
                self.log(file);
            }
        }

        Ok(())
    }

    /// Runs the liquidation business logic for the specified block
    async fn on_block(&mut self, block_number: U64) -> Result<()> {
        // 1. update our dataset with the new block's data
        self.borrowers
            .update_borrowers(self.last_block, block_number)
            .await?;

        // 2. trigger the auction for any undercollateralized borrowers
        self.liquidator
            .trigger_liquidations(self.borrowers.borrowers.iter())
            .await?;

        // 3. try buying the ones which are worth buying
        self.liquidator
            .buy_opportunities(self.last_block, block_number)
            .await?;

        Ok(())
    }

    fn log<W: Write>(&self, w: W) {
        serde_json::to_writer(
            w,
            &State {
                vaults: self.liquidator.auctions.clone(),
                borrowers: self.borrowers.borrowers.clone(),
                last_block: self.last_block,
            },
        )
        .unwrap();
    }
}
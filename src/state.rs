use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::convert::TryInto;

use cosmwasm_std::{Addr, BlockInfo, CosmosMsg, Empty, StdError, StdResult, Storage};

use crate::expiration::{Duration, Expiration};
use crate::query::Status;
use cw_storage_plus::{Item, Map, U64Key};

#[derive(Serialize, Deserialize, Clone, PartialEq, JsonSchema, Debug)]
pub struct Config {
    pub threshold_weight: u64,
    pub total_weight: u64,
    pub max_voting_period: Duration,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, JsonSchema, Debug)]
pub struct Proposal {
    pub title: String,
    pub description: String,
    pub expires: Expiration,
    pub msgs: Vec<CosmosMsg<Empty>>,
    pub status: Status,
    // voters that have already casted a vote on this proposal
    pub voters: Vec<Addr>,
    /// how many votes have already said yes
    pub yes_weight: u64,
}

impl Proposal {
    pub fn current_status(&self, block: &BlockInfo, threshold_weight: &u64) -> Status {
        let mut status = self.status;

        // if open, check if voting is passed or timed out
        if status == Status::Open && self.yes_weight >= *threshold_weight {
            status = Status::Passed;
        }
        if status == Status::Open && self.expires.is_expired(block) {
            status = Status::Rejected;
        }

        status
    }
}

// unique items
pub const CONFIG: Item<Config> = Item::new("config");
pub const PROPOSAL_COUNT: Item<u64> = Item::new("proposal_count");

// multiple-item maps
pub const VOTERS: Map<&Addr, u64> = Map::new("voters");
pub const PROPOSALS: Map<U64Key, Proposal> = Map::new("proposals");

pub fn consume_next_id(store: &mut dyn Storage) -> StdResult<u64> {
    let id: u64 = PROPOSAL_COUNT.may_load(store)?.unwrap_or_default() + 1;
    PROPOSAL_COUNT.save(store, &id)?;
    Ok(id)
}

pub fn parse_id(data: &[u8]) -> StdResult<u64> {
    match data[0..8].try_into() {
        Ok(bytes) => Ok(u64::from_be_bytes(bytes)),
        Err(_) => Err(StdError::generic_err(
            "Corrupted data found. 8 byte expected.",
        )),
    }
}

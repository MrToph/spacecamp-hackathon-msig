use std::cmp::Ordering;

#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{
    to_binary, Binary, BlockInfo, CosmosMsg, Deps, DepsMut, Empty, Env, MessageInfo, Order,
    Response, StdResult,
};

// use cw2::set_contract_version;
use crate::msg::Vote;
use crate::query::{
    ProposalListResponse, ProposalResponse, Status, ThresholdResponse, VoteInfo, VoteListResponse,
    VoteResponse, VoterDetail, VoterListResponse, VoterResponse,
};
use cw_storage_plus::Bound;

use crate::error::ContractError;
use crate::expiration::Expiration;
use crate::msg::{ExecuteMsg, InstantiateMsg, QueryMsg};
use crate::state::{consume_next_id, parse_id, Config, Proposal, CONFIG, PROPOSALS, VOTERS};

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    if msg.threshold_weight == 0 {
        return Err(ContractError::ZeroWeight {});
    }
    if msg.voters.is_empty() {
        return Err(ContractError::NoVoters {});
    }

    let mut total_weight = 0;
    for voter in msg.voters.iter() {
        if voter.weight == 0 {
            return Err(ContractError::VoterZeroWeight {});
        }
        total_weight += voter.weight;
    }

    if total_weight < msg.threshold_weight {
        return Err(ContractError::UnreachableWeight {});
    }

    let cfg = Config {
        threshold_weight: msg.threshold_weight,
        total_weight,
        max_voting_period: msg.max_voting_period,
    };
    CONFIG.save(deps.storage, &cfg)?;

    // add all voters
    for voter in msg.voters.iter() {
        let key = deps.api.addr_validate(&voter.addr)?;
        VOTERS.save(deps.storage, &key, &voter.weight)?;
    }
    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response<Empty>, ContractError> {
    match msg {
        ExecuteMsg::Propose {
            title,
            description,
            msgs,
            latest,
        } => execute_propose(deps, env, info, title, description, msgs, latest),
        ExecuteMsg::Vote { proposal_id, vote } => execute_vote(deps, env, info, proposal_id, vote),
        ExecuteMsg::Execute { proposal_id } => execute_execute(deps, env, info, proposal_id),
        ExecuteMsg::Close { proposal_id } => execute_close(deps, env, info, proposal_id),
    }
}

pub fn execute_propose(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    title: String,
    description: String,
    msgs: Vec<CosmosMsg>,
    // we ignore earliest
    latest: Option<Expiration>,
) -> Result<Response<Empty>, ContractError> {
    // anyone can create a proposal
    let vote_power = VOTERS.may_load(deps.storage, &info.sender)?.unwrap_or(0);

    let cfg = CONFIG.load(deps.storage)?;

    // max expires also used as default
    let max_expires = cfg.max_voting_period.after(&env.block);
    let mut expires = latest.unwrap_or(max_expires);
    let comp = expires.partial_cmp(&max_expires);
    if let Some(Ordering::Greater) = comp {
        expires = max_expires;
    } else if comp.is_none() {
        return Err(ContractError::WrongExpiration {});
    }

    let status = if vote_power < cfg.threshold_weight {
        Status::Open
    } else {
        Status::Passed
    };
    // if info.sender is actually a voter they will never have a vote power of zero (enforced)
    let voters = if vote_power > 0 {
        vec![info.sender.clone()]
    } else {
        vec![]
    };

    // create a proposal
    let prop = Proposal {
        title,
        description,
        expires,
        msgs,
        status,
        voters,
        yes_weight: vote_power,
    };
    let id = consume_next_id(deps.storage)?;
    PROPOSALS.save(deps.storage, id.into(), &prop)?;

    Ok(Response::new()
        .add_attribute("action", "propose")
        .add_attribute("sender", info.sender)
        .add_attribute("proposal_id", id.to_string())
        .add_attribute("status", format!("{:?}", prop.status)))
}

pub fn execute_vote(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    proposal_id: u64,
    vote: Vote,
) -> Result<Response<Empty>, ContractError> {
    // only members of the multisig can vote
    let vote_power = VOTERS
        .may_load(deps.storage, &info.sender)?
        .ok_or(ContractError::Unauthorized {})?;

    // ensure proposal exists and can be voted on
    let mut prop = PROPOSALS.load(deps.storage, proposal_id.into())?;
    if prop.status != Status::Open {
        return Err(ContractError::NotOpen {});
    }
    if prop.expires.is_expired(&env.block) {
        return Err(ContractError::Expired {});
    }

    // cast vote if no vote previously cast
    // voters cannot change their votes from "yes" to "no" for now
    if prop.voters.contains(&info.sender) {
        return Err(ContractError::AlreadyVoted {});
    }

    // if yes vote, update tally
    let cfg = CONFIG.load(deps.storage)?;
    if vote == Vote::Yes {
        // only store "yes" voters that actually contribute to the vote weight
        // "no" voters can still change their mind
        prop.voters.push(info.sender.clone());
        prop.yes_weight += vote_power;
        // update status when the passing vote comes in
        if prop.yes_weight >= cfg.threshold_weight {
            prop.status = Status::Passed;
        }
        PROPOSALS.save(deps.storage, proposal_id.into(), &prop)?;
    }

    Ok(Response::new()
        .add_attribute("action", "vote")
        .add_attribute("sender", info.sender)
        .add_attribute("proposal_id", proposal_id.to_string())
        .add_attribute("status", format!("{:?}", prop.status)))
}

pub fn execute_execute(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    proposal_id: u64,
) -> Result<Response, ContractError> {
    // anyone can trigger this if the vote passed

    let mut prop = PROPOSALS.load(deps.storage, proposal_id.into())?;
    // we allow execution even after the proposal "expiration" as long as all vote come in before
    // that point. If it was approved on time, it can be executed any time.
    if prop.status != Status::Passed {
        return Err(ContractError::WrongExecuteStatus {});
    }

    // set it to executed
    prop.status = Status::Executed;
    PROPOSALS.save(deps.storage, proposal_id.into(), &prop)?;

    // TODO: how do we actually send the messages?

    // dispatch all proposed messages
    Ok(Response::new()
        .add_messages(prop.msgs)
        .add_attribute("action", "execute")
        .add_attribute("sender", info.sender)
        .add_attribute("proposal_id", proposal_id.to_string()))
}

pub fn execute_close(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    proposal_id: u64,
) -> Result<Response<Empty>, ContractError> {
    // anyone can trigger this if the vote passed

    let mut prop = PROPOSALS.load(deps.storage, proposal_id.into())?;
    if [Status::Executed, Status::Rejected, Status::Passed]
        .iter()
        .any(|x| *x == prop.status)
    {
        return Err(ContractError::WrongCloseStatus {});
    }
    if !prop.expires.is_expired(&env.block) {
        return Err(ContractError::NotExpired {});
    }

    // set it to failed
    prop.status = Status::Rejected;
    PROPOSALS.save(deps.storage, proposal_id.into(), &prop)?;

    Ok(Response::new()
        .add_attribute("action", "close")
        .add_attribute("sender", info.sender)
        .add_attribute("proposal_id", proposal_id.to_string()))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Threshold {} => to_binary(&query_threshold(deps)?),
        QueryMsg::Proposal { proposal_id } => to_binary(&query_proposal(deps, env, proposal_id)?),
        QueryMsg::Vote { proposal_id, voter } => to_binary(&query_vote(deps, proposal_id, voter)?),
        QueryMsg::ListProposals { start_after, limit } => {
            to_binary(&list_proposals(deps, env, start_after, limit)?)
        }
        QueryMsg::ReverseProposals {
            start_before,
            limit,
        } => to_binary(&reverse_proposals(deps, env, start_before, limit)?),
        QueryMsg::ListVotes {
            proposal_id,
            start_after,
            limit,
        } => to_binary(&list_votes(deps, proposal_id, start_after, limit)?),
        QueryMsg::Voter { address } => to_binary(&query_voter(deps, address)?),
        QueryMsg::ListVoters { start_after, limit } => {
            to_binary(&list_voters(deps, start_after, limit)?)
        }
    }
}

fn query_threshold(deps: Deps) -> StdResult<ThresholdResponse> {
    let cfg = CONFIG.load(deps.storage)?;
    Ok(ThresholdResponse::AbsoluteCount {
        weight: cfg.threshold_weight,
        total_weight: cfg.total_weight,
    })
}

fn query_proposal(deps: Deps, env: Env, id: u64) -> StdResult<ProposalResponse> {
    let prop = PROPOSALS.load(deps.storage, id.into())?;
    let cfg = CONFIG.load(deps.storage)?;
    let threshold = ThresholdResponse::AbsoluteCount {
        weight: cfg.threshold_weight,
        total_weight: cfg.total_weight,
    };

    let status = prop.current_status(&env.block, &cfg.threshold_weight);

    Ok(ProposalResponse {
        id,
        title: prop.title,
        description: prop.description,
        msgs: prop.msgs,
        status,
        expires: prop.expires,
        threshold,
    })
}

// settings for pagination
const MAX_LIMIT: u32 = 30;
const DEFAULT_LIMIT: u32 = 10;

fn list_proposals(
    deps: Deps,
    env: Env,
    start_after: Option<u64>,
    limit: Option<u32>,
) -> StdResult<ProposalListResponse> {
    let cfg = CONFIG.load(deps.storage)?;
    let threshold = ThresholdResponse::AbsoluteCount {
        weight: cfg.threshold_weight,
        total_weight: cfg.total_weight,
    };

    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let start = start_after.map(Bound::exclusive_int);
    let props: StdResult<Vec<_>> = PROPOSALS
        .range(deps.storage, start, None, Order::Ascending)
        .take(limit)
        .map(|p| map_proposal(&env.block, &threshold, p))
        .collect();

    Ok(ProposalListResponse { proposals: props? })
}

fn reverse_proposals(
    deps: Deps,
    env: Env,
    start_before: Option<u64>,
    limit: Option<u32>,
) -> StdResult<ProposalListResponse> {
    let cfg = CONFIG.load(deps.storage)?;
    let threshold = ThresholdResponse::AbsoluteCount {
        weight: cfg.threshold_weight,
        total_weight: cfg.total_weight,
    };

    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let end = start_before.map(Bound::exclusive_int);
    let props: StdResult<Vec<_>> = PROPOSALS
        .range(deps.storage, None, end, Order::Descending)
        .take(limit)
        .map(|p| map_proposal(&env.block, &threshold, p))
        .collect();

    Ok(ProposalListResponse { proposals: props? })
}

fn map_proposal(
    block: &BlockInfo,
    threshold: &ThresholdResponse,
    item: StdResult<(Vec<u8>, Proposal)>,
) -> StdResult<ProposalResponse> {
    let (key, prop) = item?;
    let status = prop.current_status(
        block,
        match threshold {
            ThresholdResponse::AbsoluteCount { weight, .. } => weight,
        },
    );
    Ok(ProposalResponse {
        id: parse_id(&key)?,
        title: prop.title,
        description: prop.description,
        msgs: prop.msgs,
        status,
        expires: prop.expires,
        threshold: threshold.clone(),
    })
}

// as we only store yes votes instead of all votes, this misses the "no"/"abstain" voters
fn query_vote(deps: Deps, proposal_id: u64, voter: String) -> StdResult<VoteResponse> {
    let voter = deps.api.addr_validate(&voter)?;
    let prop = PROPOSALS.load(deps.storage, proposal_id.into())?;

    let vote = prop
        .voters
        .iter()
        .find(|&addr| addr == &voter)
        .map(|addr| VoteInfo {
            voter: addr.into(),
            vote: Vote::Yes,
            // could use the weight of the voter but that does not necessarily represent the weight at time of voting anymore
            weight: 0,
        });
    Ok(VoteResponse { vote })
}

fn list_votes(
    deps: Deps,
    proposal_id: u64,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<VoteListResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let voter = start_after
        .map(|v| deps.api.addr_validate(&v).ok())
        .flatten();
    let prop = PROPOSALS.load(deps.storage, proposal_id.into())?;

    let skip_elements = if let Some(voter) = voter {
        prop.voters
            .iter()
            .position(|x| x == &voter)
            .unwrap_or_default()
    } else {
        0
    };
    let votes: StdResult<Vec<_>> = prop
        .voters
        .iter()
        .skip(skip_elements)
        .take(limit)
        .map(|addr| {
            Ok(VoteInfo {
                voter: addr.into(),
                vote: Vote::Yes,
                weight: 0,
            })
        })
        .collect();

    Ok(VoteListResponse { votes: votes? })
}

fn query_voter(deps: Deps, voter: String) -> StdResult<VoterResponse> {
    let voter = deps.api.addr_validate(&voter)?;
    let weight = VOTERS.may_load(deps.storage, &voter)?;
    Ok(VoterResponse { weight })
}

fn list_voters(
    deps: Deps,
    start_after: Option<String>,
    limit: Option<u32>,
) -> StdResult<VoterListResponse> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize;
    let start = start_after.map(Bound::exclusive);

    let voters: StdResult<Vec<_>> = VOTERS
        .range(deps.storage, start, None, Order::Ascending)
        .take(limit)
        .map(|item| {
            let (key, weight) = item?;
            Ok(VoterDetail {
                addr: String::from_utf8(key)?,
                weight,
            })
        })
        .collect();

    Ok(VoterListResponse { voters: voters? })
}

#[cfg(test)]
mod tests {
    use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info};
    use cosmwasm_std::{coin, from_binary, BankMsg};

    use crate::expiration::Duration;
    use crate::msg::Voter;

    use super::*;

    fn mock_env_height(height_delta: u64) -> Env {
        let mut env = mock_env();
        env.block.height += height_delta;
        env
    }

    fn mock_env_time(time_delta: u64) -> Env {
        let mut env = mock_env();
        env.block.time = env.block.time.plus_seconds(time_delta);
        env
    }

    const OWNER: &str = "admin0001";
    const VOTER1: &str = "voter0001";
    const VOTER2: &str = "voter0002";
    const VOTER3: &str = "voter0003";
    const VOTER4: &str = "voter0004";
    const VOTER5: &str = "voter0005";
    const SOMEBODY: &str = "somebody";

    fn voter<T: Into<String>>(addr: T, weight: u64) -> Voter {
        Voter {
            addr: addr.into(),
            weight,
        }
    }

    // this will set up the instantiation for other tests
    fn setup_test_case(
        deps: DepsMut,
        info: MessageInfo,
        threshold_weight: u64,
        max_voting_period: Duration,
    ) -> Result<Response<Empty>, ContractError> {
        // Instantiate a contract with voters
        let voters = vec![
            voter(&info.sender, 1),
            voter(VOTER1, 1),
            voter(VOTER2, 2),
            voter(VOTER3, 3),
            voter(VOTER4, 4),
            voter(VOTER5, 5),
        ];

        let instantiate_msg = InstantiateMsg {
            voters,
            threshold_weight,
            max_voting_period,
        };
        instantiate(deps, mock_env(), info, instantiate_msg)
    }

    fn get_tally(deps: Deps, proposal_id: u64) -> u64 {
        // Get all the voters on the proposal
        let voters = QueryMsg::ListVotes {
            proposal_id,
            start_after: None,
            limit: None,
        };
        let votes: VoteListResponse =
            from_binary(&query(deps, mock_env(), voters).unwrap()).unwrap();
        // Sum the weights of the Yes votes to get the tally
        votes
            .votes
            .iter()
            .filter(|&v| v.vote == Vote::Yes)
            .map(|v| v.weight)
            .sum()
    }

    #[test]
    fn test_instantiate_works() {
        let mut deps = mock_dependencies(&[]);
        let info = mock_info(OWNER, &[]);

        let max_voting_period = Duration::Time(1234567);

        // No voters fails
        let instantiate_msg = InstantiateMsg {
            voters: vec![],
            threshold_weight: 1,
            max_voting_period,
        };
        let err =
            instantiate(deps.as_mut(), mock_env(), info.clone(), instantiate_msg).unwrap_err();
        assert_eq!(err, ContractError::NoVoters {});

        // Zero required weight fails
        let instantiate_msg = InstantiateMsg {
            voters: vec![voter(OWNER, 1)],
            threshold_weight: 0,
            max_voting_period,
        };
        let err =
            instantiate(deps.as_mut(), mock_env(), info.clone(), instantiate_msg).unwrap_err();
        assert_eq!(err, ContractError::ZeroWeight {});

        // Total weight less than required weight not allowed
        let threshold_weight = 100;
        let err = setup_test_case(
            deps.as_mut(),
            info.clone(),
            threshold_weight,
            max_voting_period,
        )
        .unwrap_err();
        assert_eq!(err, ContractError::UnreachableWeight {});

        // All valid
        let threshold_weight = 1;
        setup_test_case(deps.as_mut(), info, threshold_weight, max_voting_period).unwrap();
    }

    // TODO: query() tests

    #[test]
    fn test_propose_works() {
        let mut deps = mock_dependencies(&[]);

        let threshold_weight = 4;
        let voting_period = Duration::Time(2000000);

        let info = mock_info(OWNER, &[]);
        setup_test_case(deps.as_mut(), info, threshold_weight, voting_period).unwrap();

        let bank_msg = BankMsg::Send {
            to_address: SOMEBODY.into(),
            amount: vec![coin(1, "BTC")],
        };
        let msgs = vec![CosmosMsg::Bank(bank_msg)];

        // Wrong expiration option fails
        let info = mock_info(OWNER, &[]);
        let proposal_wrong_exp = ExecuteMsg::Propose {
            title: "Rewarding somebody".to_string(),
            description: "Do we reward her?".to_string(),
            msgs: msgs.clone(),
            latest: Some(Expiration::AtHeight(123456)),
        };
        let err = execute(deps.as_mut(), mock_env(), info, proposal_wrong_exp).unwrap_err();
        assert_eq!(err, ContractError::WrongExpiration {});

        // Anyone can propose
        let info = mock_info(SOMEBODY, &[]);
        let proposal = ExecuteMsg::Propose {
            title: "Rewarding somebody".to_string(),
            description: "Do we reward her?".to_string(),
            msgs: msgs.clone(),
            latest: None,
        };
        let res = execute(deps.as_mut(), mock_env(), info, proposal.clone()).unwrap();
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "propose")
                .add_attribute("sender", SOMEBODY)
                .add_attribute("proposal_id", 1.to_string())
                .add_attribute("status", "Open")
        );

        // Proposal from voter works
        let info = mock_info(VOTER3, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, proposal.clone()).unwrap();

        // Verify
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "propose")
                .add_attribute("sender", VOTER3)
                .add_attribute("proposal_id", 2.to_string())
                .add_attribute("status", "Open")
        );

        // Proposal from voter with enough vote power directly passes
        let info = mock_info(VOTER4, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, proposal).unwrap();

        // Verify
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "propose")
                .add_attribute("sender", VOTER4)
                .add_attribute("proposal_id", 3.to_string())
                .add_attribute("status", "Passed")
        );
    }

    #[test]
    fn test_vote_works() {
        let mut deps = mock_dependencies(&[]);

        let threshold_weight = 3;
        let voting_period = Duration::Time(2000000);

        let info = mock_info(OWNER, &[]);
        setup_test_case(deps.as_mut(), info.clone(), threshold_weight, voting_period).unwrap();

        // Propose
        let bank_msg = BankMsg::Send {
            to_address: SOMEBODY.into(),
            amount: vec![coin(1, "BTC")],
        };
        let msgs = vec![CosmosMsg::Bank(bank_msg)];
        let proposal = ExecuteMsg::Propose {
            title: "Pay somebody".to_string(),
            description: "Do I pay her?".to_string(),
            msgs,
            latest: None,
        };
        let res = execute(deps.as_mut(), mock_env(), info.clone(), proposal).unwrap();

        // Get the proposal id from the logs
        let proposal_id: u64 = res.attributes[2].value.parse().unwrap();

        // Owner cannot vote (again)
        let yes_vote = ExecuteMsg::Vote {
            proposal_id,
            vote: Vote::Yes,
        };
        let err = execute(deps.as_mut(), mock_env(), info, yes_vote.clone()).unwrap_err();
        assert_eq!(err, ContractError::AlreadyVoted {});

        // Only voters can vote
        let info = mock_info(SOMEBODY, &[]);
        let err = execute(deps.as_mut(), mock_env(), info, yes_vote.clone()).unwrap_err();
        assert_eq!(err, ContractError::Unauthorized {});

        // But voter1 can
        let info = mock_info(VOTER1, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, yes_vote.clone()).unwrap();

        // Verify
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "vote")
                .add_attribute("sender", VOTER1)
                .add_attribute("proposal_id", proposal_id.to_string())
                .add_attribute("status", "Open")
        );

        // No/Veto votes have no effect on the tally
        // Get the proposal id from the logs
        let proposal_id: u64 = res.attributes[2].value.parse().unwrap();

        // Compute the current tally
        let tally = get_tally(deps.as_ref(), proposal_id);

        // Cast a No vote
        let no_vote = ExecuteMsg::Vote {
            proposal_id,
            vote: Vote::No,
        };
        let info = mock_info(VOTER2, &[]);
        execute(deps.as_mut(), mock_env(), info, no_vote.clone()).unwrap();

        // Cast a Veto vote
        let veto_vote = ExecuteMsg::Vote {
            proposal_id,
            vote: Vote::Veto,
        };
        let info = mock_info(VOTER3, &[]);
        execute(deps.as_mut(), mock_env(), info.clone(), veto_vote).unwrap();

        // Verify
        assert_eq!(tally, get_tally(deps.as_ref(), proposal_id));

        // Once voted, yes votes cannot be changed
        let info = mock_info(VOTER1, &[]);
        let err = execute(deps.as_mut(), mock_env(), info.clone(), yes_vote.clone()).unwrap_err();
        assert_eq!(err, ContractError::AlreadyVoted {});
        assert_eq!(tally, get_tally(deps.as_ref(), proposal_id));

        // Expired proposals cannot be voted
        let env = match voting_period {
            Duration::Time(duration) => mock_env_time(duration + 1),
            Duration::Height(duration) => mock_env_height(duration + 1),
        };
        let err = execute(deps.as_mut(), env, info, no_vote).unwrap_err();
        assert_eq!(err, ContractError::Expired {});

        // Vote it again, so it passes
        let info = mock_info(VOTER4, &[]);
        let res = execute(deps.as_mut(), mock_env(), info, yes_vote.clone()).unwrap();

        // Verify
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "vote")
                .add_attribute("sender", VOTER4)
                .add_attribute("proposal_id", proposal_id.to_string())
                .add_attribute("status", "Passed")
        );

        // non-Open proposals cannot be voted
        let info = mock_info(VOTER5, &[]);
        let err = execute(deps.as_mut(), mock_env(), info, yes_vote).unwrap_err();
        assert_eq!(err, ContractError::NotOpen {});
    }

    #[test]
    fn test_execute_works() {
        let mut deps = mock_dependencies(&[]);

        let threshold_weight = 3;
        let voting_period = Duration::Time(2000000);

        let info = mock_info(OWNER, &[]);
        setup_test_case(deps.as_mut(), info.clone(), threshold_weight, voting_period).unwrap();

        // Propose
        let bank_msg = BankMsg::Send {
            to_address: SOMEBODY.into(),
            amount: vec![coin(1, "BTC")],
        };
        let msgs = vec![CosmosMsg::Bank(bank_msg)];
        let proposal = ExecuteMsg::Propose {
            title: "Pay somebody".to_string(),
            description: "Do I pay her?".to_string(),
            msgs: msgs.clone(),
            latest: None,
        };
        let res = execute(deps.as_mut(), mock_env(), info.clone(), proposal).unwrap();

        // Get the proposal id from the logs
        let proposal_id: u64 = res.attributes[2].value.parse().unwrap();

        // Only Passed can be executed
        let execution = ExecuteMsg::Execute { proposal_id };
        let err = execute(deps.as_mut(), mock_env(), info, execution.clone()).unwrap_err();
        assert_eq!(err, ContractError::WrongExecuteStatus {});

        // Vote it, so it passes
        let vote = ExecuteMsg::Vote {
            proposal_id,
            vote: Vote::Yes,
        };
        let info = mock_info(VOTER3, &[]);
        let res = execute(deps.as_mut(), mock_env(), info.clone(), vote).unwrap();

        // Verify
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "vote")
                .add_attribute("sender", VOTER3)
                .add_attribute("proposal_id", proposal_id.to_string())
                .add_attribute("status", "Passed")
        );

        // In passing: Try to close Passed fails
        let closing = ExecuteMsg::Close { proposal_id };
        let err = execute(deps.as_mut(), mock_env(), info, closing).unwrap_err();
        assert_eq!(err, ContractError::WrongCloseStatus {});

        // Execute works. Anybody can execute Passed proposals
        let info = mock_info(SOMEBODY, &[]);
        let res = execute(deps.as_mut(), mock_env(), info.clone(), execution).unwrap();

        // Verify
        assert_eq!(
            res,
            Response::new()
                .add_messages(msgs)
                .add_attribute("action", "execute")
                .add_attribute("sender", SOMEBODY)
                .add_attribute("proposal_id", proposal_id.to_string())
        );

        // In passing: Try to close Executed fails
        let closing = ExecuteMsg::Close { proposal_id };
        let err = execute(deps.as_mut(), mock_env(), info, closing).unwrap_err();
        assert_eq!(err, ContractError::WrongCloseStatus {});
    }

    #[test]
    fn test_close_works() {
        let mut deps = mock_dependencies(&[]);

        let threshold_weight = 3;
        let voting_period = Duration::Height(2000000);

        let info = mock_info(OWNER, &[]);
        setup_test_case(deps.as_mut(), info.clone(), threshold_weight, voting_period).unwrap();

        // Propose
        let bank_msg = BankMsg::Send {
            to_address: SOMEBODY.into(),
            amount: vec![coin(1, "BTC")],
        };
        let msgs = vec![CosmosMsg::Bank(bank_msg)];
        let proposal = ExecuteMsg::Propose {
            title: "Pay somebody".to_string(),
            description: "Do I pay her?".to_string(),
            msgs: msgs.clone(),
            latest: None,
        };
        let res = execute(deps.as_mut(), mock_env(), info, proposal).unwrap();

        // Get the proposal id from the logs
        let proposal_id: u64 = res.attributes[2].value.parse().unwrap();

        let closing = ExecuteMsg::Close { proposal_id };

        // Anybody can close
        let info = mock_info(SOMEBODY, &[]);

        // Non-expired proposals cannot be closed
        let err = execute(deps.as_mut(), mock_env(), info, closing).unwrap_err();
        assert_eq!(err, ContractError::NotExpired {});

        // Expired proposals can be closed
        let info = mock_info(OWNER, &[]);

        let proposal = ExecuteMsg::Propose {
            title: "(Try to) pay somebody".to_string(),
            description: "Pay somebody after time?".to_string(),
            msgs,
            latest: Some(Expiration::AtHeight(123456)),
        };
        let res = execute(deps.as_mut(), mock_env(), info.clone(), proposal).unwrap();

        // Get the proposal id from the logs
        let proposal_id: u64 = res.attributes[2].value.parse().unwrap();

        let closing = ExecuteMsg::Close { proposal_id };

        // Close expired works
        let env = mock_env_height(1234567);
        let res = execute(
            deps.as_mut(),
            env,
            mock_info(SOMEBODY, &[]),
            closing.clone(),
        )
        .unwrap();

        // Verify
        assert_eq!(
            res,
            Response::new()
                .add_attribute("action", "close")
                .add_attribute("sender", SOMEBODY)
                .add_attribute("proposal_id", proposal_id.to_string())
        );

        // Trying to close it again fails
        let err = execute(deps.as_mut(), mock_env(), info, closing).unwrap_err();
        assert_eq!(err, ContractError::WrongCloseStatus {});
    }
}
